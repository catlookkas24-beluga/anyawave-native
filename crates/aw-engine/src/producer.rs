//! Producer: source -> demux/decode -> stereo f32 48 kHz -> filters -> Opus 20 ms packets -> SPSC ring.
//!
//! * Opus sources (webm/ogg) with NO active filter and 20 ms stereo packets are forwarded untouched
//!   (`passthrough = true`): zero decode, zero encode, zero resample.
//! * Everything else: symphonia decode (or libopus decode for Opus) -> rubato resample -> C++ filter
//!   chain -> libopus encode (128 kbps VBR, music).
//! * Seek: producer bumps `Control::epoch`; the consumer drops packets whose epoch is stale, so the
//!   SPSC ring never needs to be flushed from the wrong side.
//!
//! Crossfade / gapless (see `crossfade.rs` for the maths):
//! * The owner queues the NEXT track with `Control::set_next`. One producer thread then plays both tracks
//!   back to back through the SAME encoder + ring, so there is never a gap and never a second encoder.
//! * ~15 s + fade length before the end a tiny loader thread opens the next track and decodes its first
//!   seconds (trims leading silence, measures loudness). Nothing is decoded twice and nothing runs earlier.
//! * Only during the fade (default 6 s) two tracks are decoded and mixed; Opus passthrough pauses for that
//!   window (the Opus decoder is warmed up 2 s earlier so the switch is clean) and resumes afterwards.
//! * `Control::request_skip` = manual skip: quick fade (<= 2.5 s) into the queued track.
//! * Hand-offs are reported through `Control::on_handoff`, and `Control::track_seq` counts them.
use crate::crossfade::{self, apply_fade_in, leading_silence_frames, mix_block, plan, rms_db, Level, PlanIn, Silence};
use audiopus_sys as opus;
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::{FftFixedIn, Resampler};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering::{Relaxed, SeqCst}};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_NULL, CODEC_TYPE_OPUS};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

pub const MAX_PACKET: usize = 1275;
pub const FRAME_SAMPLES: usize = 960; // per channel, 20 ms @ 48 kHz
pub const RING_FRAMES: usize = 128;          // hard capacity (~2.5 s). Actual fill target = AW_BUFFER_MS
pub const BASE_COMPLEXITY: i32 = 6;

/// Runtime tuning (env vars) so one binary fits both a 0.1-vCPU free host and a big VPS.
///   AW_BITRATE    Opus bitrate in bit/s, 6000..510000 (default 128000). Opus is the ONLY codec Discord voice accepts.
///   AW_BUFFER_MS  how far ahead the producer fills the ring (default 1000). Bigger = survives CPU throttling.
///   AW_ADAPTIVE   1 (default) = lower Opus complexity automatically while the buffer is draining.
///   AW_CROSSFADE_MS    default crossfade length for new players, 0 = off (default 0). Max 12000.
///   AW_CROSSFADE_MODE  smart (default) | fixed   (fixed = plain equal-power fade, no silence/loudness logic)
///   AW_XF_HANDBACK     1 (default) = go back to Opus passthrough after a fade, 0 = keep re-encoding that track
pub struct Config { pub bitrate: i32, pub complexity: i32, pub unconstrained_vbr: bool, pub target_ahead: usize, pub adaptive: bool }
fn env_i(k: &str, d: i64) -> i64 { std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d) }
pub fn cfg() -> &'static Config {
    static C: OnceLock<Config> = OnceLock::new();
    C.get_or_init(|| {
      // AW_QUALITY: eco | balanced (default) | high | max   (AW_BITRATE overrides the bitrate)
      let (br, cx, unc) = match std::env::var("AW_QUALITY").unwrap_or_default().as_str() {
          "eco" => (64_000, 4, false),
          "high" => (192_000, 9, false),
          "max" => (320_000, 10, true),
          _ => (128_000, BASE_COMPLEXITY as i64, false),
      };
      Config {
        bitrate: env_i("AW_BITRATE", br).clamp(6_000, 510_000) as i32,
        complexity: cx as i32,
        unconstrained_vbr: unc,
        target_ahead: ((env_i("AW_BUFFER_MS", 1000) / 20).clamp(5, (RING_FRAMES - 2) as i64)) as usize,
        adaptive: env_i("AW_ADAPTIVE", 1) != 0,
      }
    })
}

// libopus ctl numbers (stable ABI constants from opus_defines.h)
const OPUS_APPLICATION_AUDIO: i32 = 2049;
const OPUS_SET_BITRATE: i32 = 4002;
const OPUS_SET_VBR: i32 = 4006;
const OPUS_SET_COMPLEXITY: i32 = 4010;
const OPUS_SET_SIGNAL: i32 = 4024;
const OPUS_SET_MAX_BANDWIDTH: i32 = 4004;
const OPUS_SET_VBR_CONSTRAINT: i32 = 4020;
const OPUS_SET_LSB_DEPTH: i32 = 4036;
const OPUS_BANDWIDTH_FULLBAND: i32 = 1105;
const OPUS_SIGNAL_MUSIC: i32 = 3002;

// crossfade timing (ms)
const PREFETCH_LEAD_MS: u64 = 15_000;   // open + analyse the next track this long before the fade window
const WARM_MS: u64 = 2_000;             // warm the Opus decoder / measure level this long before the fade
const LEAD_SCAN_MS: usize = 4_500;      // decode this much of the next track up front
const MAX_TRIM_MS: usize = 3_000;       // never trim more than this of leading silence
const SILENCE_WINDOW_MS: u64 = 15_000;  // trailing-silence trigger only inside the last N ms of a known-length track
const SILENCE_CUT_MS: u64 = 400;        // this much continuous silence at the end => start the next track now

#[derive(Clone)]
pub struct Packet {
    pub epoch: u32,
    pub len: u16,
    pub passthrough: bool,
    pub pos_ms: u64,             // media position of this packet (for playerUpdate)
    pub data: [u8; MAX_PACKET],
}

pub fn packet_ring() -> (Producer<Packet>, Consumer<Packet>) { RingBuffer::new(RING_FRAMES) }

#[derive(Clone)]
pub enum Input { Url(String), File(PathBuf) }

/// the track that should follow the current one
#[derive(Clone)]
pub struct NextTrack {
    pub input: Input,
    pub start_ms: i64,
    /// track length if the container does not say (e.g. HLS); lets the fade start on time
    pub len_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum Handoff {
    /// the next track became the audible one by itself (end of track / crossfade)
    Natural,
    /// the same, but because `request_skip` was used
    Manual,
    /// the queued track could not be opened; it was dropped, the current track keeps playing
    Failed(String),
}
pub type HandoffCb = Arc<dyn Fn(Handoff) + Send + Sync>;

/// Shared control block (producer <-> player/consumer).
pub struct Control {
    pub stop: AtomicBool,
    pub paused: AtomicBool,      // consumer side stops popping; kept here so one Arc carries all state
    pub seek_ms: AtomicI64,      // -1 = none
    pub epoch: AtomicU32,        // consumer drops packets with a different epoch
    pub position_ms: AtomicU64,  // producer-side position (ahead of playback by the buffer)
    pub played_ms: AtomicU64,    // consumer-side position (what listeners actually hear)
    pub finished: AtomicBool,
    pub error: Mutex<Option<String>>,
    // ---- crossfade / gapless
    pub next: Mutex<Option<NextTrack>>,
    pub next_id: AtomicU32,        // bumped on every change of `next`
    pub xfade_ms: AtomicU32,       // 0 = no mixing (still gapless)
    pub xfade_smart: AtomicBool,
    pub skip_fade: AtomicBool,     // crossfade into `next` right now
    pub track_seq: AtomicU32,      // +1 every time the audible track changes inside this producer
    pub len_hint_ms: AtomicU64,    // owner may set the length of the FIRST track (0 = unknown)
    pub cur_len_ms: AtomicU64,     // length of the track currently being produced (0 = unknown)
    pub on_handoff: Mutex<Option<HandoffCb>>,
}
impl Control {
    pub fn new() -> Arc<Self> {
        let ms = env_i("AW_CROSSFADE_MS", 0).clamp(0, crossfade::MAX_FADE_MS as i64) as u32;
        let smart = std::env::var("AW_CROSSFADE_MODE").map(|v| v != "fixed").unwrap_or(true);
        Arc::new(Self {
            stop: AtomicBool::new(false), paused: AtomicBool::new(false), seek_ms: AtomicI64::new(-1),
            epoch: AtomicU32::new(0), position_ms: AtomicU64::new(0), played_ms: AtomicU64::new(0), finished: AtomicBool::new(false),
            error: Mutex::new(None),
            next: Mutex::new(None), next_id: AtomicU32::new(0), xfade_ms: AtomicU32::new(ms), xfade_smart: AtomicBool::new(smart),
            skip_fade: AtomicBool::new(false), track_seq: AtomicU32::new(0), len_hint_ms: AtomicU64::new(0),
            cur_len_ms: AtomicU64::new(0), on_handoff: Mutex::new(None),
        })
    }
    /// queue (Some) or clear (None) the track that follows the current one
    pub fn set_next(&self, n: Option<NextTrack>) {
        if let Ok(mut g) = self.next.lock() {
            let none = n.is_none();
            *g = n;
            if none { self.skip_fade.store(false, SeqCst); }
            self.next_id.fetch_add(1, SeqCst);
        }
    }
    /// clear `next` only if nobody replaced it since the producer looked (id = the id it saw)
    pub fn take_next_if(&self, id: u32) {
        if let Ok(mut g) = self.next.lock() {
            if self.next_id.load(SeqCst) == id {
                *g = None;
                self.skip_fade.store(false, SeqCst);
                self.next_id.fetch_add(1, SeqCst);
            }
        }
    }
    /// crossfade into the queued track now (manual skip). Call `set_next` first.
    pub fn request_skip(&self) { self.skip_fade.store(true, SeqCst); }
    /// ms = 0 turns mixing off; `smart` = silence trimming + loudness match + adaptive length
    pub fn set_crossfade(&self, ms: u32, smart: bool) {
        self.xfade_ms.store(ms.min(crossfade::MAX_FADE_MS), Relaxed);
        self.xfade_smart.store(smart, Relaxed);
    }
    pub fn set_on_handoff(&self, cb: Option<HandoffCb>) { if let Ok(mut g) = self.on_handoff.lock() { *g = cb; } }
    fn handoff_cb(&self) -> Option<HandoffCb> { self.on_handoff.lock().ok().and_then(|g| g.clone()) }
}

#[derive(Default, Debug, Clone)]
pub struct Stats { pub frames: u64, pub passthrough_frames: u64, pub codec: String, pub source_rate: u32, pub crossfades: u32, pub handoffs: u32 }

// ---------------------------------------------------------------- libopus wrappers
struct OpusEnc(*mut opus::OpusEncoder);
unsafe impl Send for OpusEnc {}
impl OpusEnc {
    fn new() -> Result<Self, String> {
        let mut err: i32 = 0;
        let p = unsafe { opus::opus_encoder_create(48000, 2, OPUS_APPLICATION_AUDIO, &mut err) };
        if p.is_null() || err != 0 { return Err(format!("opus_encoder_create failed ({err})")); }
        unsafe {
            opus::opus_encoder_ctl(p, OPUS_SET_BITRATE, cfg().bitrate);
            opus::opus_encoder_ctl(p, OPUS_SET_VBR, 1i32);
            opus::opus_encoder_ctl(p, OPUS_SET_COMPLEXITY, cfg().complexity);
            opus::opus_encoder_ctl(p, OPUS_SET_MAX_BANDWIDTH, OPUS_BANDWIDTH_FULLBAND); // keep everything up to 20 kHz
            opus::opus_encoder_ctl(p, OPUS_SET_LSB_DEPTH, 24i32);                       // our input is float, treat as 24-bit
            if cfg().unconstrained_vbr { opus::opus_encoder_ctl(p, OPUS_SET_VBR_CONSTRAINT, 0i32); }
            opus::opus_encoder_ctl(p, OPUS_SET_SIGNAL, OPUS_SIGNAL_MUSIC);
        }
        Ok(Self(p))
    }
    fn set_complexity(&mut self, c: i32) { unsafe { opus::opus_encoder_ctl(self.0, OPUS_SET_COMPLEXITY, c); } }
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
    chain: Arc<Mutex<crate::dsp::FilterChain>>,
    enc: OpusEnc,
    acc: Vec<f32>,                 // pending interleaved stereo 48 kHz
    fbuf: [f32; FRAME_SAMPLES * 2],
    tx: Producer<Packet>,
    ctl: Arc<Control>,
    throttle: bool,
    stats: Stats,
    cx: i32,        // current Opus complexity (adaptive)
    tsbuf: Vec<f32>,   // scratch for the time-scale pre-stage
    tempo: f64,        // media seconds per output second (speed*rate)
    pos_frac: f64,
    healthy: u32,   // consecutive frames with a full buffer
}

impl Pipe {
    /// false => stop requested
    fn push(&mut self, pkt: Packet) -> bool {
        let mut pkt = pkt;
        loop {
            if self.ctl.stop.load(Relaxed) { return false; }
            let room = if self.throttle { self.tx.slots() > RING_FRAMES - cfg().target_ahead } else { self.tx.slots() > 0 };
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
        let mut p = Packet { epoch: self.ctl.epoch.load(Relaxed), len: data.len() as u16, passthrough, pos_ms: self.ctl.position_ms.load(Relaxed), data: [0; MAX_PACKET] };
        p.data[..data.len()].copy_from_slice(data);
        self.stats.frames += 1;
        if passthrough { self.stats.passthrough_frames += 1; }
        self.pos_frac += 20.0 * self.tempo;
        let whole = self.pos_frac as u64;
        self.pos_frac -= whole as f64;
        self.ctl.position_ms.fetch_add(whole, Relaxed);
        self.push(p)
    }

    /// CPU-pressure feedback: buffer draining => cheaper encode; buffer full for a while => restore quality.
    fn adapt(&mut self) {
        if !self.throttle || !cfg().adaptive { return; }
        let t = cfg().target_ahead;
        let occ = RING_FRAMES - self.tx.slots();
        if occ * 3 < t {
            self.healthy = 0;
            if self.cx > 1 { self.cx -= 1; self.enc.set_complexity(self.cx); }
        } else if occ + 1 >= t {
            self.healthy += 1;
            if self.healthy >= 150 && self.cx < cfg().complexity { self.cx += 1; self.enc.set_complexity(self.cx); self.healthy = 0; }
        }
    }

    fn identity(&self) -> bool { self.chain.lock().map(|c| c.is_identity()).unwrap_or(false) }

    /// feed stereo 48 kHz PCM; encodes as many whole 20 ms frames as available
    fn feed(&mut self, stereo48: &[f32]) -> bool {
        // variable-length pre-stage (speed / pitch / rate); bypassed when inactive
        let mut tmp = std::mem::take(&mut self.tsbuf);
        tmp.clear();
        let used = match self.chain.lock() {
            Ok(mut c) => { let u = c.timescale(stereo48, &mut tmp); self.tempo = c.tempo(); u }
            Err(_) => false,
        };
        if used { self.acc.extend_from_slice(&tmp); } else { self.acc.extend_from_slice(stereo48); }
        self.tsbuf = tmp;
        let n = FRAME_SAMPLES * 2;
        let mut off = 0;
        let mut ok = true;
        while self.acc.len() - off >= n {
            self.fbuf.copy_from_slice(&self.acc[off..off + n]);
            off += n;
            if let Ok(mut c) = self.chain.lock() { c.process(&mut self.fbuf); }
            self.adapt();
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
            let (src, ext) = crate::sources::open_url(u)?;
            if let Some(e) = ext { hint.with_extension(&e); }
            Ok((src, hint))
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

// ---------------------------------------------------------------- one track (decoder state)
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Pass,   // Opus 20 ms stereo packets are forwarded untouched
    Warm,   // like Pass, but the packet is also decoded (decoder stays warm, level can be measured)
    Decode, // always return PCM
}

enum Item {
    Pcm(Vec<f32>),   // stereo 48 kHz
    Opus(Vec<u8>),   // passthrough packet
    Eof,
}

struct Track {
    format: Box<dyn FormatReader>,
    track_id: u32,
    sym_dec: Option<Box<dyn Decoder>>,
    opus_dec: Option<OpusDec>,
    resamp: Option<Resamp>,
    sbuf: Option<SampleBuffer<f32>>,
    tmp: Vec<f32>,
    pcm_opus: Vec<f32>,
    codec: String,
    source_rate: u32,
    len_ms: Option<u64>,
    start_ms: u64,
    frames: u64,          // 48 kHz frames decoded so far (source time)
    pre: Vec<f32>,        // already-decoded lead-in (incoming tracks only)
    pre_pos: usize,
    eof: bool,
    rms_in_db: f32,       // level of the first seconds (incoming tracks)
    warm_frames: usize,   // set by next(Mode::Warm): frames available in pcm_opus
    force_decode: bool,   // AW_XF_HANDBACK=0 after a fade
}

impl Track {
    fn open(input: &Input, start_ms: i64, len_hint: Option<u64>) -> Result<Track, String> {
        let (src, hint) = open(input)?;
        let mss = MediaSourceStream::new(src, MediaSourceStreamOptions::default());
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
            .map_err(|e| format!("probe: {e}"))?;
        let format = probed.format;
        let track = format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or("no audio track")?.clone();
        let is_opus = track.codec_params.codec == CODEC_TYPE_OPUS;
        let mut opus_dec: Option<OpusDec> = None;
        let mut sym_dec: Option<Box<dyn Decoder>> = None;
        if is_opus { opus_dec = Some(OpusDec::new()?); }
        else {
            sym_dec = Some(symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())
                .map_err(|e| format!("decoder: {e}"))?);
        }
        let len_ms = match (track.codec_params.n_frames, track.codec_params.sample_rate) {
            (Some(n), Some(sr)) if sr > 0 => Some(n * 1000 / sr as u64),
            _ => None,
        }.or(len_hint);
        let mut t = Track {
            format, track_id: track.id, sym_dec, opus_dec, resamp: None, sbuf: None, tmp: Vec::new(),
            pcm_opus: vec![0f32; 5760 * 2],
            codec: if is_opus { "opus".into() } else { format!("{:?}", track.codec_params.codec) },
            source_rate: track.codec_params.sample_rate.unwrap_or(0),
            len_ms, start_ms: start_ms.max(0) as u64, frames: 0, pre: Vec::new(), pre_pos: 0, eof: false,
            rms_in_db: -120.0, warm_frames: 0, force_decode: false,
        };
        if start_ms > 0 {
            t.seek_to(start_ms);
            if let Some(d) = t.sym_dec.as_mut() { d.reset(); }
        }
        Ok(t)
    }

    fn seek_to(&mut self, ms: i64) {
        let time = Time::new((ms / 1000) as u64, (ms % 1000) as f64 / 1000.0);
        let _ = self.format.seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(self.track_id) });
    }

    fn seek(&mut self, ms: i64) {
        self.seek_to(ms);
        self.start_ms = ms.max(0) as u64;
        self.frames = 0;
        self.pre.clear();
        self.pre_pos = 0;
        self.eof = false;
        if let Some(r) = self.resamp.as_mut() { r.reset(); }
        if let Some(d) = self.sym_dec.as_mut() { d.reset(); }
    }

    /// position in the source in ms (decoded frames minus lead-in that has not been delivered yet)
    fn pos_ms(&self) -> u64 {
        let pending = ((self.pre.len() - self.pre_pos) / 2) as u64;
        self.start_ms + (self.frames.saturating_sub(pending)) * 1000 / 48_000
    }

    /// next chunk of audio from the container
    fn next(&mut self, mode: Mode) -> Result<Item, String> {
        self.warm_frames = 0;
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(Item::Eof),
                Err(SymError::ResetRequired) => return Ok(Item::Eof),
                Err(e) => return Err(format!("demux: {e}")),
            };
            if packet.track_id() != self.track_id { continue; }

            if let Some(od) = self.opus_dec.as_mut() {
                let buf = packet.buf();
                if buf.is_empty() { continue; }
                let nb = unsafe { opus::opus_packet_get_nb_samples(buf.as_ptr(), buf.len() as i32, 48000) };
                let st = unsafe { opus::opus_packet_get_nb_channels(buf.as_ptr()) };
                let eligible = nb == FRAME_SAMPLES as i32 && st == 2 && buf.len() <= MAX_PACKET;
                if eligible && mode != Mode::Decode {
                    if mode == Mode::Warm {
                        let n = od.decode(buf, &mut self.pcm_opus);
                        if n > 0 { self.warm_frames = n as usize; }
                    }
                    self.frames += FRAME_SAMPLES as u64;
                    return Ok(Item::Opus(buf.to_vec()));
                }
                let n = od.decode(buf, &mut self.pcm_opus);
                if n <= 0 { continue; }
                self.frames += n as u64;
                return Ok(Item::Pcm(self.pcm_opus[..(n as usize) * 2].to_vec()));
            } else if let Some(d) = self.sym_dec.as_mut() {
                match d.decode(&packet) {
                    Ok(decoded) => {
                        let spec = *decoded.spec();
                        let ch = spec.channels.count();
                        let need = decoded.frames() * ch;
                        if self.sbuf.as_ref().map(|b| b.capacity() < need).unwrap_or(true) {
                            self.sbuf = Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
                        }
                        let sb = self.sbuf.as_mut().unwrap();
                        sb.copy_interleaved_ref(decoded);
                        self.tmp.clear();
                        to_stereo(sb.samples(), ch, &mut self.tmp);
                        if spec.rate != 48000 {
                            if self.resamp.is_none() { self.resamp = Some(Resamp::new(spec.rate)?); }
                            let mut out = Vec::with_capacity(self.tmp.len() * 2);
                            self.resamp.as_mut().unwrap().process(&self.tmp, &mut out);
                            if out.is_empty() { continue; }
                            self.frames += (out.len() / 2) as u64;
                            return Ok(Item::Pcm(out));
                        } else {
                            if self.tmp.is_empty() { continue; }
                            self.frames += (self.tmp.len() / 2) as u64;
                            return Ok(Item::Pcm(self.tmp.clone()));
                        }
                    }
                    Err(SymError::DecodeError(_)) => continue,
                    Err(e) => return Err(format!("decode: {e}")),
                }
            } else {
                return Err("no decoder".into());
            }
        }
    }

    /// append decoded PCM (lead-in first) to `q` until it holds `frames` frames. Returns true when the track is over.
    fn fill(&mut self, q: &mut VecDeque<f32>, frames: usize) -> Result<bool, String> {
        let need = frames * 2;
        while q.len() < need {
            if self.pre_pos < self.pre.len() {
                let take = ((self.pre.len() - self.pre_pos).min(need - q.len())) & !1;
                q.extend(self.pre[self.pre_pos..self.pre_pos + take].iter().copied());
                self.pre_pos += take;
                if self.pre_pos >= self.pre.len() { self.pre.clear(); self.pre_pos = 0; }
                continue;
            }
            if self.eof { return Ok(true); }
            match self.next(Mode::Decode)? {
                Item::Pcm(v) => q.extend(v),
                Item::Eof => { self.eof = true; return Ok(true); }
                Item::Opus(_) => {}
            }
        }
        Ok(false)
    }

    /// incoming track: decode the first seconds now (in the loader thread), drop leading silence, measure level
    fn prepare_lead(&mut self) -> Result<(), String> {
        let want = LEAD_SCAN_MS * 48;
        while self.pre.len() / 2 < want && !self.eof {
            match self.next(Mode::Decode)? {
                Item::Pcm(v) => self.pre.extend_from_slice(&v),
                Item::Eof => self.eof = true,
                Item::Opus(_) => {}
            }
        }
        let skip = leading_silence_frames(&self.pre, crossfade::SILENCE_DB, MAX_TRIM_MS * 48);
        if skip > 0 {
            self.pre.drain(..skip * 2);
            apply_fade_in(&mut self.pre, 96);   // 2 ms, hides the cut
        }
        let n = self.pre.len().min(2 * 48_000 * 2);
        self.rms_in_db = rms_db(&self.pre[..n]);
        Ok(())
    }
}

/// open + analyse the next track off the producer thread, so a slow network never starves the ring
fn spawn_prefetch(nt: NextTrack, smart: bool) -> mpsc::Receiver<Result<Track, String>> {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new().name("aw-prefetch".into()).stack_size(1 << 20).spawn(move || {
        let res = Track::open(&nt.input, nt.start_ms, nt.len_ms).and_then(|mut t| {
            if smart && nt.start_ms == 0 { t.prepare_lead()?; }
            Ok(t)
        });
        let _ = tx.send(res);
    });
    let _ = spawned; // on failure the closure (and its Sender) is dropped => receiver sees Disconnected
    rx
}

// ---------------------------------------------------------------- the fade
enum FadeEnd {
    Done(Track),   // the incoming track, ready to continue
    Stopped,
    Aborted,       // a seek arrived: abandon the fade, the current track carries on
}

fn run_fade(out: &mut Track, mut inc: Track, pl: crossfade::Plan, manual: bool, pipe: &mut Pipe, ctl: &Arc<Control>) -> Result<FadeEnd, String> {
    let total = pl.frames.max(FRAME_SAMPLES);
    let mut qo: VecDeque<f32> = VecDeque::with_capacity(FRAME_SAMPLES * 6);
    let mut qi: VecDeque<f32> = VecDeque::with_capacity(FRAME_SAMPLES * 6);
    let mut bo = vec![0f32; FRAME_SAMPLES * 2];
    let mut bi = vec![0f32; FRAME_SAMPLES * 2];
    let mut bm = vec![0f32; FRAME_SAMPLES * 2];
    let (mut o_eof, mut i_eof) = (false, false);
    let mut done = 0usize;
    let mut switched = false;
    let take = |q: &mut VecDeque<f32>, buf: &mut [f32]| { for s in buf.iter_mut() { *s = q.pop_front().unwrap_or(0.0); } };

    while done < total {
        if ctl.stop.load(Relaxed) { return Ok(FadeEnd::Stopped); }
        if ctl.seek_ms.load(Relaxed) >= 0 { return Ok(FadeEnd::Aborted); }
        let n = FRAME_SAMPLES.min(total - done);
        if !o_eof && qo.len() < n * 2 { o_eof = out.fill(&mut qo, n)?; }
        if !i_eof && qi.len() < n * 2 { i_eof = inc.fill(&mut qi, n)?; }
        take(&mut qo, &mut bo[..n * 2]);
        take(&mut qi, &mut bi[..n * 2]);
        mix_block(&bo[..n * 2], &bi[..n * 2], &mut bm[..n * 2], done, total, pl.in_gain);
        if !pipe.feed(&bm[..n * 2]) { return Ok(FadeEnd::Stopped); }
        done += n;

        // halfway: from here on the listener hears mostly the new track -> switch position / events
        if !switched && done * 2 >= total {
            switched = true;
            let ahead_ms = (qi.len() / 2) as u64 * 1000 / 48_000;
            ctl.position_ms.store(inc.pos_ms().saturating_sub(ahead_ms), Relaxed);
            ctl.cur_len_ms.store(inc.len_ms.unwrap_or(0), Relaxed);
            pipe.pos_frac = 0.0;
            ctl.track_seq.fetch_add(1, SeqCst);
            if let Some(cb) = ctl.handoff_cb() { cb(if manual { Handoff::Manual } else { Handoff::Natural }); }
        }
        // the incoming track is shorter than the fade: nothing left to fade into
        if i_eof && qi.is_empty() { break; }
    }
    if !qi.is_empty() {
        let rest: Vec<f32> = qi.drain(..).collect();
        if !pipe.feed(&rest) { return Ok(FadeEnd::Stopped); }
    }
    Ok(FadeEnd::Done(inc))
}

// ---------------------------------------------------------------- main loop
/// Blocking. Run on a dedicated thread (see `spawn`). `throttle=false` decodes as fast as possible (benchmarks).
pub fn run(input: Input, chain: Arc<Mutex<crate::dsp::FilterChain>>, tx: Producer<Packet>, ctl: Arc<Control>, start_ms: i64, throttle: bool) -> Result<Stats, String> {
    let res = run_inner(input, chain, tx, ctl.clone(), start_ms, throttle);
    if let Err(e) = &res { if let Ok(mut g) = ctl.error.lock() { *g = Some(e.clone()); } }
    ctl.finished.store(true, SeqCst);
    res
}

fn run_inner(input: Input, chain: Arc<Mutex<crate::dsp::FilterChain>>, tx: Producer<Packet>, ctl: Arc<Control>, start_ms: i64, throttle: bool) -> Result<Stats, String> {
    let hint0 = ctl.len_hint_ms.load(Relaxed);
    let mut cur = Track::open(&input, start_ms, if hint0 > 0 { Some(hint0) } else { None })?;
    ctl.cur_len_ms.store(cur.len_ms.unwrap_or(0), Relaxed);

    let mut pipe = Pipe {
        chain, enc: OpusEnc::new()?, acc: Vec::with_capacity(FRAME_SAMPLES * 8), fbuf: [0.0; FRAME_SAMPLES * 2],
        tx, ctl: ctl.clone(), throttle, stats: Stats::default(), cx: cfg().complexity, healthy: 0, tsbuf: Vec::new(), tempo: 1.0, pos_frac: 0.0,
    };
    pipe.stats.codec = cur.codec.clone();
    pipe.stats.source_rate = cur.source_rate;
    if start_ms > 0 { ctl.position_ms.store(start_ms as u64, Relaxed); }

    let hand_back = env_i("AW_XF_HANDBACK", 1) != 0;
    let mut cached_id = u32::MAX;
    let mut cached_next: Option<NextTrack> = None;
    let mut pref: Option<mpsc::Receiver<Result<Track, String>>> = None;
    let mut ready: Option<Track> = None;
    let mut level = Level::new(2 * 48_000);              // ~2 s of the current track, only kept while a next track is queued
    let mut quiet = Silence::new(crossfade::SILENCE_DB);

    loop {
        if ctl.stop.load(Relaxed) { break; }
        let s = ctl.seek_ms.swap(-1, SeqCst);
        if s >= 0 {
            cur.seek(s);
            ctl.epoch.fetch_add(1, SeqCst);
            ctl.position_ms.store(s as u64, Relaxed);
            pipe.acc.clear();
            if let Ok(mut c) = pipe.chain.lock() { c.reset_state(); }
            pref = None; ready = None; level.reset(); quiet.reset();
        }

        // ---- what is queued next?
        let nid = ctl.next_id.load(SeqCst);
        if nid != cached_id {
            cached_id = nid;
            cached_next = ctl.next.lock().ok().and_then(|g| g.clone());
            pref = None; ready = None;
        }
        let xf = ctl.xfade_ms.load(Relaxed);
        let smart = ctl.xfade_smart.load(Relaxed);
        let manual = ctl.skip_fade.load(SeqCst) && cached_next.is_some();
        let xf_eff = if manual { xf.max(crossfade::MIN_FADE_MS) } else { xf };
        let remaining_ms = cur.len_ms.map(|l| l.saturating_sub(cur.pos_ms()));

        // ---- start the loader shortly before it is needed
        if let Some(nt) = cached_next.as_ref() {
            if pref.is_none() && ready.is_none() {
                let soon = manual || remaining_ms.map(|r| r <= xf as u64 + PREFETCH_LEAD_MS).unwrap_or(false);
                if soon { pref = Some(spawn_prefetch(nt.clone(), smart)); }
            }
        }
        let polled = pref.as_ref().map(|r| r.try_recv());
        let mut failed: Option<String> = None;
        match polled {
            Some(Ok(Ok(t))) => { ready = Some(t); pref = None; }
            Some(Ok(Err(e))) => { failed = Some(e); }
            Some(Err(mpsc::TryRecvError::Disconnected)) => { failed = Some("next-track loader stopped".into()); }
            _ => {}
        }
        if let Some(e) = failed {
            pref = None;
            ctl.take_next_if(cached_id);
            cached_next = None;
            cached_id = ctl.next_id.load(SeqCst);
            if let Some(cb) = ctl.handoff_cb() { cb(Handoff::Failed(e)); }
        }

        // ---- plan the fade (cheap arithmetic) and decide whether it is time
        let tracking = xf_eff > 0 && cached_next.is_some();
        let mut pl_ms = 0u64;
        let mut warm = false;
        if tracking {
            let len_in = ready.as_ref().and_then(|r| r.len_ms).or(cached_next.as_ref().and_then(|n| n.len_ms));
            let pl = plan(&PlanIn { requested_ms: xf_eff, smart, manual: false, silence_cut: false, len_out_ms: cur.len_ms, len_in_ms: len_in,
                                    rms_out_db: level.db(), rms_in_db: ready.as_ref().map(|r| r.rms_in_db).unwrap_or(-120.0) });
            pl_ms = pl.frames as u64 / 48;
            warm = remaining_ms.map(|r| r <= pl_ms + WARM_MS).unwrap_or(false);
        }
        let mut go: Option<(bool, bool)> = None;   // (manual, silence_cut)
        if ready.is_some() && tracking {
            if manual { go = Some((true, false)); }
            else if remaining_ms.map(|r| r <= pl_ms).unwrap_or(false) { go = Some((false, false)); }
            else if smart && quiet.run_ms() >= SILENCE_CUT_MS && remaining_ms.map(|r| r <= SILENCE_WINDOW_MS).unwrap_or(false) { go = Some((false, true)); }
        }
        if let Some((man, sil)) = go {
            if let Some(inc) = ready.take() {
                let pl = plan(&PlanIn { requested_ms: xf_eff, smart, manual: man, silence_cut: sil, len_out_ms: cur.len_ms, len_in_ms: inc.len_ms,
                                        rms_out_db: level.db(), rms_in_db: inc.rms_in_db });
                ctl.skip_fade.store(false, SeqCst);
                match run_fade(&mut cur, inc, pl, man, &mut pipe, &ctl)? {
                    FadeEnd::Done(mut t) => {
                        pipe.stats.crossfades += 1;
                        t.force_decode = !hand_back;
                        cur = t;
                        ctl.take_next_if(cached_id);
                        level.reset(); quiet.reset(); pref = None;
                    }
                    FadeEnd::Stopped => break,
                    FadeEnd::Aborted => {}   // seek pending: handled at the top of the loop, the next track is loaded again later
                }
                continue;
            }
        }

        // ---- normal playback of the current track
        let mode = if cur.force_decode || !(pipe.acc.is_empty() && pipe.identity()) { Mode::Decode }
                   else if warm { Mode::Warm } else { Mode::Pass };
        match cur.next(mode)? {
            Item::Pcm(v) => {
                if tracking { level.push(&v); quiet.push(&v); }
                if !pipe.feed(&v) { break; }
            }
            Item::Opus(b) => {
                if tracking && cur.warm_frames > 0 {
                    let n = cur.warm_frames * 2;
                    level.push(&cur.pcm_opus[..n]);
                    quiet.push(&cur.pcm_opus[..n]);
                }
                if !pipe.emit(&b, true) { break; }       // PASSTHROUGH: no decode / encode
            }
            Item::Eof => {
                // end of this track: gapless hand-off if something is queued, otherwise we are done
                let Some(nt) = cached_next.clone() else { break };
                let loaded = match ready.take() {
                    Some(t) => Ok(t),
                    None => Track::open(&nt.input, nt.start_ms, nt.len_ms).and_then(|mut t| {
                        if smart && nt.start_ms == 0 { t.prepare_lead()?; }
                        Ok(t)
                    }),
                };
                match loaded {
                    Ok(t) => {
                        cur = t;
                        pipe.stats.handoffs += 1;
                        pipe.pos_frac = 0.0;
                        ctl.position_ms.store(cur.pos_ms(), Relaxed);
                        ctl.cur_len_ms.store(cur.len_ms.unwrap_or(0), Relaxed);
                        ctl.take_next_if(cached_id);
                        ctl.track_seq.fetch_add(1, SeqCst);
                        if let Some(cb) = ctl.handoff_cb() { cb(Handoff::Natural); }
                        level.reset(); quiet.reset(); pref = None;
                    }
                    Err(e) => {
                        ctl.take_next_if(cached_id);
                        if let Some(cb) = ctl.handoff_cb() { cb(Handoff::Failed(e)); }
                        break;
                    }
                }
            }
        }
    }
    if !ctl.stop.load(Relaxed) { pipe.flush_tail(); }
    Ok(pipe.stats.clone())
}

/// One OS thread per *playing* track (mostly parked while the ring is full). A bounded pool is a later optimisation.
pub fn spawn(input: Input, chain: Arc<Mutex<crate::dsp::FilterChain>>, tx: Producer<Packet>, ctl: Arc<Control>, start_ms: i64) -> std::thread::JoinHandle<Result<Stats, String>> {
    std::thread::Builder::new().name("aw-producer".into()).stack_size(1 << 20)
        .spawn(move || run(input, chain, tx, ctl, start_ms, true)).expect("spawn producer")
}
