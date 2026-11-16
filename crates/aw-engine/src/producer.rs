//! Producer: source -> demux/decode -> stereo f32 48 kHz -> filters -> Opus 20 ms packets -> SPSC ring.
//!
//! * Opus sources (webm/ogg) with NO active filter and 20 ms stereo packets are forwarded untouched
//!   (`passthrough = true`): zero decode, zero encode, zero resample.
//! * Everything else: symphonia decode (or libopus decode for Opus) -> rubato resample -> C++ filter
//!   chain -> libopus encode (128 kbps VBR, music).
//! * Seek: producer bumps `Control::epoch`; the consumer drops packets whose epoch is stale, so the
//!   SPSC ring never needs to be flushed from the wrong side.
use crate::dsp::FilterChain;
use audiopus_sys as opus;
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::{FftFixedIn, Resampler};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering::{Relaxed, SeqCst}};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_NULL, CODEC_TYPE_OPUS};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions, ReadOnlySource};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

pub const MAX_PACKET: usize = 1275;
pub const FRAME_SAMPLES: usize = 960; // per channel, 20 ms @ 48 kHz
pub const RING_FRAMES: usize = 48;
pub const TARGET_AHEAD: usize = 15; // ~300 ms buffered ahead of the network clock
pub const BITRATE: i32 = 128_000;

// libopus ctl numbers (stable ABI constants from opus_defines.h)
const OPUS_APPLICATION_AUDIO: i32 = 2049;
const OPUS_SET_BITRATE: i32 = 4002;
const OPUS_SET_VBR: i32 = 4006;
const OPUS_SET_COMPLEXITY: i32 = 4010;
const OPUS_SET_SIGNAL: i32 = 4024;
const OPUS_SIGNAL_MUSIC: i32 = 3002;

#[derive(Clone)]
pub struct Packet {
    pub epoch: u32,
    pub len: u16,
    pub passthrough: bool,
    pub data: [u8; MAX_PACKET],
}

pub fn packet_ring() -> (Producer<Packet>, Consumer<Packet>) { RingBuffer::new(RING_FRAMES) }

/// Shared control block (producer <-> player/consumer).
pub struct Control {
    pub stop: AtomicBool,
    pub paused: AtomicBool,      // consumer side stops popping; kept here so one Arc carries all state
    pub seek_ms: AtomicI64,      // -1 = none
    pub epoch: AtomicU32,        // consumer drops packets with a different epoch
    pub position_ms: AtomicU64,
    pub finished: AtomicBool,
    pub error: Mutex<Option<String>>,
}
impl Control {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            stop: AtomicBool::new(false), paused: AtomicBool::new(false), seek_ms: AtomicI64::new(-1),
            epoch: AtomicU32::new(0), position_ms: AtomicU64::new(0), finished: AtomicBool::new(false),
            error: Mutex::new(None),
        })
    }
}

pub enum Input { Url(String), File(PathBuf) }

#[derive(Default, Debug, Clone)]
pub struct Stats { pub frames: u64, pub passthrough_frames: u64, pub codec: String, pub source_rate: u32 }

// ---------------------------------------------------------------- libopus wrappers
struct OpusEnc(*mut opus::OpusEncoder);
unsafe impl Send for OpusEnc {}
impl OpusEnc {
    fn new() -> Result<Self, String> {
        let mut err: i32 = 0;
        let p = unsafe { opus::opus_encoder_create(48000, 2, OPUS_APPLICATION_AUDIO, &mut err) };
        if p.is_null() || err != 0 { return Err(format!("opus_encoder_create failed ({err})")); }
        unsafe {
            opus::opus_encoder_ctl(p, OPUS_SET_BITRATE, BITRATE);
            opus::opus_encoder_ctl(p, OPUS_SET_VBR, 1i32);
            opus::opus_encoder_ctl(p, OPUS_SET_COMPLEXITY, 6i32);
            opus::opus_encoder_ctl(p, OPUS_SET_SIGNAL, OPUS_SIGNAL_MUSIC);
        }
        Ok(Self(p))
    }
    fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> i32 {
        unsafe { opus::opus_encode_float(self.0, pcm.as_ptr(), FRAME_SAMPLES as i32, out.as_mut_ptr(), out.len() as i32) }
    }
}
impl Drop for OpusEnc { fn drop(&mut self) { unsafe { opus::opus_encoder_destroy(self.0) } } }

struct OpusDec(*mut opus::OpusDecoder);
unsafe impl Send for OpusDec {}
impl OpusDec {
    fn new() -> Result<Self, String> {
        let mut err: i32 = 0;
        let p = unsafe { opus::opus_decoder_create(48000, 2, &mut err) };
        if p.is_null() || err != 0 { return Err(format!("opus_decoder_create failed ({err})")); }
        Ok(Self(p))
    }
    /// returns samples per channel, or negative error
    fn decode(&mut self, pkt: &[u8], out: &mut [f32]) -> i32 {
        unsafe { opus::opus_decode_float(self.0, pkt.as_ptr(), pkt.len() as i32, out.as_mut_ptr(), (out.len() / 2) as i32, 0) }
    }
}
impl Drop for OpusDec { fn drop(&mut self) { unsafe { opus::opus_decoder_destroy(self.0) } } }

// ---------------------------------------------------------------- resampler
struct Resamp { r: FftFixedIn<f32>, planar: [Vec<f32>; 2] }
impl Resamp {
    fn new(rate: u32) -> Result<Self, String> {
        let r = FftFixedIn::<f32>::new(rate as usize, 48000, 1024, 2, 2).map_err(|e| e.to_string())?;
        Ok(Self { r, planar: [Vec::new(), Vec::new()] })
    }
    fn process(&mut self, stereo: &[f32], out: &mut Vec<f32>) {
        for f in stereo.chunks_exact(2) { self.planar[0].push(f[0]); self.planar[1].push(f[1]); }
        loop {
            let need = self.r.input_frames_next();
            if self.planar[0].len() < need { break; }
            let chunk = [self.planar[0][..need].to_vec(), self.planar[1][..need].to_vec()];
            self.planar[0].drain(..need);
            self.planar[1].drain(..need);
            if let Ok(o) = self.r.process(&chunk[..], None) {
                for i in 0..o[0].len() { out.push(o[0][i]); out.push(o[1][i]); }
            }
        }
    }
    fn reset(&mut self) { self.planar[0].clear(); self.planar[1].clear(); self.r.reset(); }
}

// ---------------------------------------------------------------- pipeline core
struct Pipe {
    chain: Arc<Mutex<FilterChain>>,
    enc: OpusEnc,
    acc: Vec<f32>,                 // pending interleaved stereo 48 kHz
    fbuf: [f32; FRAME_SAMPLES * 2],
    tx: Producer<Packet>,
    ctl: Arc<Control>,
    throttle: bool,
    stats: Stats,
}

impl Pipe {
    /// false => stop requested
    fn push(&mut self, pkt: Packet) -> bool {
        let mut pkt = pkt;
        loop {
            if self.ctl.stop.load(Relaxed) { return false; }
            let room = if self.throttle { self.tx.slots() > RING_FRAMES - TARGET_AHEAD } else { self.tx.slots() > 0 };
            if room {
                match self.tx.push(pkt) {
                    Ok(()) => return true,
                    Err(rtrb::PushError::Full(p)) => pkt = p,
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn emit(&mut self, data: &[u8], passthrough: bool) -> bool {
        let mut p = Packet { epoch: self.ctl.epoch.load(Relaxed), len: data.len() as u16, passthrough, data: [0; MAX_PACKET] };
        p.data[..data.len()].copy_from_slice(data);
        self.stats.frames += 1;
        if passthrough { self.stats.passthrough_frames += 1; }
        self.ctl.position_ms.fetch_add(20, Relaxed);
        self.push(p)
    }

    fn identity(&self) -> bool { self.chain.lock().map(|c| c.is_identity()).unwrap_or(false) }

    /// feed stereo 48 kHz PCM; encodes as many whole 20 ms frames as available
    fn feed(&mut self, stereo48: &[f32]) -> bool {
        self.acc.extend_from_slice(stereo48);
        let n = FRAME_SAMPLES * 2;
        let mut off = 0;
        let mut ok = true;
        while self.acc.len() - off >= n {
            self.fbuf.copy_from_slice(&self.acc[off..off + n]);
            off += n;
            if let Ok(mut c) = self.chain.lock() { c.process(&mut self.fbuf); }
            let mut out = [0u8; MAX_PACKET];
            let len = self.enc.encode(&self.fbuf, &mut out);
            if len > 0 && !self.emit(&out[..len as usize], false) { ok = false; break; }
        }
        self.acc.drain(..off);
        ok
    }

    fn flush_tail(&mut self) {
        if self.acc.is_empty() { return; }
        let n = FRAME_SAMPLES * 2;
        self.acc.resize(n, 0.0);
        let tail = std::mem::take(&mut self.acc);
        self.feed(&tail);
    }
}

fn ext_of(s: &str) -> Option<String> {
    std::path::Path::new(s).extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase())
}

fn open(input: &Input) -> Result<(Box<dyn MediaSource>, Hint), String> {
    let mut hint = Hint::new();
    match input {
        Input::Url(u) => {
            let path = u.split('?').next().unwrap_or(u);
            if let Some(e) = ext_of(path) { hint.with_extension(&e); }
            let agent = ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(10))
                .timeout_read(Duration::from_secs(15))
                .build();
            let resp = agent.get(u).call().map_err(|e| format!("http: {e}"))?;
            let rd = resp.into_reader();
            Ok((Box::new(ReadOnlySource::new(rd)), hint))
        }
        Input::File(p) => {
            if let Some(e) = p.to_str().and_then(ext_of) { hint.with_extension(&e); }
            let f = std::fs::File::open(p).map_err(|e| format!("file: {e}"))?;
            Ok((Box::new(f), hint))
        }
    }
}

fn to_stereo(inter: &[f32], ch: usize, out: &mut Vec<f32>) {
    match ch {
        0 => {}
        1 => for &s in inter { out.push(s); out.push(s); },
        2 => out.extend_from_slice(inter),
        n => for f in inter.chunks_exact(n) { out.push(f[0]); out.push(f[1]); },
    }
}

/// Blocking. Run on a dedicated thread (see `spawn`). `throttle=false` decodes as fast as possible (benchmarks).
pub fn run(input: Input, chain: Arc<Mutex<FilterChain>>, tx: Producer<Packet>, ctl: Arc<Control>, start_ms: i64, throttle: bool) -> Result<Stats, String> {
    let res = run_inner(input, chain, tx, ctl.clone(), start_ms, throttle);
    if let Err(e) = &res { if let Ok(mut g) = ctl.error.lock() { *g = Some(e.clone()); } }
    ctl.finished.store(true, SeqCst);
    res
}

fn run_inner(input: Input, chain: Arc<Mutex<FilterChain>>, tx: Producer<Packet>, ctl: Arc<Control>, start_ms: i64, throttle: bool) -> Result<Stats, String> {
    let (src, hint) = open(&input)?;
    let mss = MediaSourceStream::new(src, MediaSourceStreamOptions::default());
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("probe: {e}"))?;
    let mut format = probed.format;
    let track = format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or("no audio track")?.clone();
    let track_id = track.id;
    let is_opus = track.codec_params.codec == CODEC_TYPE_OPUS;

    let mut opus_dec: Option<OpusDec> = None;
    let mut sym_dec: Option<Box<dyn Decoder>> = None;
    if is_opus { opus_dec = Some(OpusDec::new()?); }
    else {
        sym_dec = Some(symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())
            .map_err(|e| format!("decoder: {e}"))?);
    }

    let mut pipe = Pipe {
        chain, enc: OpusEnc::new()?, acc: Vec::with_capacity(FRAME_SAMPLES * 8), fbuf: [0.0; FRAME_SAMPLES * 2],
        tx, ctl: ctl.clone(), throttle, stats: Stats::default(),
    };
    pipe.stats.codec = if is_opus { "opus".into() } else { format!("{:?}", track.codec_params.codec) };
    pipe.stats.source_rate = track.codec_params.sample_rate.unwrap_or(0);

    let mut resamp: Option<Resamp> = None;
    let mut sbuf: Option<SampleBuffer<f32>> = None;
    let mut tmp: Vec<f32> = Vec::new();
    let mut pcm_opus = vec![0f32; 5760 * 2];

    let do_seek = |format: &mut Box<dyn symphonia::core::formats::FormatReader>, ms: i64| {
        let time = Time::new((ms / 1000) as u64, (ms % 1000) as f64 / 1000.0);
        let _ = format.seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(track_id) });
    };
    if start_ms > 0 { do_seek(&mut format, start_ms); ctl.position_ms.store(start_ms as u64, Relaxed); }

    loop {
        if ctl.stop.load(Relaxed) { break; }
        let s = ctl.seek_ms.swap(-1, SeqCst);
        if s >= 0 {
            do_seek(&mut format, s);
            ctl.epoch.fetch_add(1, SeqCst);
            ctl.position_ms.store(s as u64, Relaxed);
            pipe.acc.clear();
            if let Some(r) = resamp.as_mut() { r.reset(); }
            if let Some(d) = sym_dec.as_mut() { d.reset(); }
        }

        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => return Err(format!("demux: {e}")),
        };
        if packet.track_id() != track_id { continue; }

        if let Some(od) = opus_dec.as_mut() {
            let buf = packet.buf();
            if buf.is_empty() { continue; }
            let nb = unsafe { opus::opus_packet_get_nb_samples(buf.as_ptr(), buf.len() as i32, 48000) };
            let st = unsafe { opus::opus_packet_get_nb_channels(buf.as_ptr()) };
            if nb == FRAME_SAMPLES as i32 && st == 2 && buf.len() <= MAX_PACKET && pipe.acc.is_empty() && pipe.identity() {
                if !pipe.emit(buf, true) { break; }       // PASSTHROUGH: no decode / encode
                continue;
            }
            let n = od.decode(buf, &mut pcm_opus);
            if n <= 0 { continue; }
            let slice = pcm_opus[..(n as usize) * 2].to_vec();
            if !pipe.feed(&slice) { break; }
        } else if let Some(d) = sym_dec.as_mut() {
            match d.decode(&packet) {
                Ok(decoded) => {
                    let spec = *decoded.spec();
                    let ch = spec.channels.count();
                    let need = decoded.frames() * ch;
                    if sbuf.as_ref().map(|b| b.capacity() < need).unwrap_or(true) {
                        sbuf = Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
                    }
                    let sb = sbuf.as_mut().unwrap();
                    sb.copy_interleaved_ref(decoded);
                    tmp.clear();
                    to_stereo(sb.samples(), ch, &mut tmp);
                    if spec.rate != 48000 {
                        if resamp.is_none() { resamp = Some(Resamp::new(spec.rate)?); }
                        let mut out = Vec::with_capacity(tmp.len() * 2);
                        resamp.as_mut().unwrap().process(&tmp, &mut out);
                        if !pipe.feed(&out) { break; }
                    } else if !pipe.feed(&tmp) { break; }
                }
                Err(SymError::DecodeError(_)) => continue,
                Err(e) => return Err(format!("decode: {e}")),
            }
        }
    }
    if !ctl.stop.load(Relaxed) { pipe.flush_tail(); }
    Ok(pipe.stats.clone())
}

/// One OS thread per *playing* track (mostly parked while the ring is full). A bounded pool is a later optimisation.
pub fn spawn(input: Input, chain: Arc<Mutex<FilterChain>>, tx: Producer<Packet>, ctl: Arc<Control>, start_ms: i64) -> std::thread::JoinHandle<Result<Stats, String>> {
    std::thread::Builder::new().name("aw-producer".into()).stack_size(1 << 20)
        .spawn(move || run(input, chain, tx, ctl, start_ms, true)).expect("spawn producer")
}
