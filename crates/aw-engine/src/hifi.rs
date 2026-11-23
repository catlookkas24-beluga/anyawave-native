//! Hi-Fi output path. NOT sent to Discord (Discord voice only carries Opus @ 48 kHz) - this feeds a web player / any HTTP client.
//!
//!   tier        Apple Music name                  what we produce
//!   efficient   High Efficiency (HE-AAC)          HE-AAC 64 kbps when libfdk_aac exists, otherwise AAC-LC 96 kbps (reported honestly)
//!   high        High Quality (AAC 256 kbps)       AAC-LC 256 kbps (ADTS)
//!   lossless    Lossless (ALAC, up to 24/48)      FLAC (default) | ALAC (fMP4) | WAV, 24-bit, rate = min(source, 48 kHz)
//!   hires       Hi-Res Lossless (up to 24/192)    same, rate = min(source, 192 kHz)
//!
//! The decoder keeps the SOURCE sample rate / bit depth (no forced 48 kHz). With no filter active the samples are
//! passed untouched (f32 holds every 24-bit integer exactly), so lossless tiers are bit-perfect w.r.t. the source.
//! Encoding is done by an ffmpeg subprocess fed with raw f32le on stdin.
//!
//! MQA: sources are checked for the MQA sync word (see `mqa.rs`). Detection only - an MQA file is passed through as
//! the ordinary 24-bit "core" it contains; there is no MQA unfold/render.
use crate::dsp::FilterChain;
use crate::mqa;
use crate::sources::open_url;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, OnceLock};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_NULL, CODEC_TYPE_OPUS};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier { Efficient, High, Lossless, HiRes }
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fmt { Auto, Flac, Alac, Wav }

pub struct Caps { pub ffmpeg: bool, pub version: String, pub aac: bool, pub fdk_aac: bool, pub flac: bool, pub alac: bool }

pub fn capabilities() -> &'static Caps {
    static C: OnceLock<Caps> = OnceLock::new();
    C.get_or_init(|| {
        match Command::new("ffmpeg").args(["-hide_banner", "-encoders"]).output() {
            Ok(o) if o.status.success() => {
                let t = String::from_utf8_lossy(&o.stdout).to_string();
                let has = |n: &str| t.lines().any(|l| l.split_whitespace().nth(1) == Some(n));
                let version = Command::new("ffmpeg").arg("-version").output().ok()
                    .and_then(|v| String::from_utf8_lossy(&v.stdout).lines().next().map(|s| s.to_string())).unwrap_or_default();
                Caps { ffmpeg: true, version, aac: has("aac"), fdk_aac: has("libfdk_aac"), flac: has("flac"), alac: has("alac") }
            }
            _ => Caps { ffmpeg: false, version: String::new(), aac: false, fdk_aac: false, flac: false, alac: false },
        }
    })
}

/// (Content-Type, human label) - known before the source is opened
pub fn content_type(tier: Tier, fmt: Fmt) -> (&'static str, String) {
    let c = capabilities();
    match tier {
        Tier::Efficient => ("audio/aac", if c.fdk_aac { "HE-AAC 64 kbps".to_string() } else { "AAC-LC 96 kbps (HE-AAC encoder not available on this server)".to_string() }),
        Tier::High => ("audio/aac", "AAC-LC 256 kbps".to_string()),
        Tier::Lossless | Tier::HiRes => {
            let cap = if tier == Tier::Lossless { "24-bit up to 48 kHz" } else { "24-bit up to 192 kHz" };
            match fmt {
                Fmt::Alac => ("audio/mp4", format!("ALAC {cap}")),
                Fmt::Wav => ("audio/wav", format!("PCM {cap}")),
                _ => ("audio/flac", format!("FLAC {cap}")),
            }
        }
    }
}

struct Plan { args: Vec<String>, out_rate: u32 }

fn s(v: &[&str]) -> Vec<String> { v.iter().map(|x| x.to_string()).collect() }

fn plan(tier: Tier, fmt: Fmt, in_rate: u32) -> Result<Plan, String> {
    let c = capabilities();
    match tier {
        Tier::Efficient | Tier::High => {
            let out = if in_rate == 44100 { 44100 } else { 48000 };
            let mut a = if tier == Tier::Efficient && c.fdk_aac {
                s(&["-c:a", "libfdk_aac", "-profile:a", "aac_he", "-b:a", "64k"])
            } else if c.aac {
                s(&["-c:a", "aac", "-b:a", if tier == Tier::High { "256k" } else { "96k" }])
            } else {
                return Err("this server's ffmpeg has no AAC encoder".into());
            };
            a.extend(s(&["-ar", &out.to_string(), "-f", "adts"]));
            Ok(Plan { args: a, out_rate: out })
        }
        Tier::Lossless | Tier::HiRes => {
            let cap = if tier == Tier::Lossless { 48000 } else { 192000 };
            let out = in_rate.min(cap);
            let o = out.to_string();
            let a = match fmt {
                Fmt::Wav => s(&["-c:a", "pcm_s24le", "-ar", &o, "-f", "wav"]),
                Fmt::Alac => {
                    if !c.alac { return Err("this server's ffmpeg has no ALAC encoder".into()); }
                    s(&["-c:a", "alac", "-sample_fmt", "s32p", "-bits_per_raw_sample", "24", "-ar", &o, "-f", "mp4", "-movflags", "frag_keyframe+empty_moov+default_base_moof"])
                }
                _ => {
                    if !c.flac { return Err("this server's ffmpeg has no FLAC encoder".into()); }
                    s(&["-c:a", "flac", "-compression_level", "4", "-sample_fmt", "s32", "-bits_per_raw_sample", "24", "-ar", &o, "-f", "flac"])
                }
            };
            Ok(Plan { args: a, out_rate: out })
        }
    }
}

pub struct Opts {
    pub tier: Tier,
    pub fmt: Fmt,
    pub start_ms: i64,
    /// called once the real sample rate is known; set filters here (None = bit-perfect passthrough)
    pub configure: Option<Box<dyn Fn(&mut FilterChain) + Send>>,
}

#[derive(Debug, Clone)]
pub struct Info {
    pub in_rate: u32,
    pub out_rate: u32,
    pub channels: usize,
    pub bytes_out: u64,
    pub filtered: bool,
    /// MQA sync-word scan of the source (None when the source was not stereo / was not scanned)
    pub mqa: Option<mqa::Report>,
}

type Block = (Vec<f32>, u32, usize);

fn next_block(format: &mut Box<dyn FormatReader>, dec: &mut Box<dyn Decoder>, track_id: u32, sbuf: &mut Option<SampleBuffer<f32>>) -> Result<Option<Block>, String> {
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(SymError::ResetRequired) => return Ok(None),
            Err(e) => return Err(format!("demux: {e}")),
        };
        if packet.track_id() != track_id { continue; }
        match dec.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                let ch = spec.channels.count();
                let need = decoded.frames() * ch;
                if sbuf.as_ref().map(|b| b.capacity() < need).unwrap_or(true) {
                    *sbuf = Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
                }
                let sb = sbuf.as_mut().unwrap();
                sb.copy_interleaved_ref(decoded);
                return Ok(Some((sb.samples().to_vec(), spec.rate, ch)));
            }
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("decode: {e}")),
        }
    }
}

fn to_stereo(inter: &[f32], ch: usize, out: &mut Vec<f32>) {
    match ch {
        0 => {}
        1 => for &x in inter { out.push(x); out.push(x); },
        2 => out.extend_from_slice(inter),
        n => for f in inter.chunks_exact(n) { out.push(f[0]); out.push(f[1]); },
    }
}

fn process_block(samples: &[f32], ch: usize, filtered: bool, chain: &mut FilterChain, tmp: &mut Vec<f32>) -> Vec<f32> {
    if !filtered {
        if ch <= 2 { return samples.to_vec(); }
        let mut v = Vec::with_capacity(samples.len() / ch * 2);
        to_stereo(samples, ch, &mut v);
        return v;
    }
    let mut st = Vec::with_capacity(samples.len() + 8);
    to_stereo(samples, ch, &mut st);
    tmp.clear();
    if chain.timescale(&st, tmp) { chain.process(tmp); tmp.clone() } else { chain.process(&mut st); st }
}

/// Blocking. Decodes `url` at its native rate, optionally filters, encodes with ffmpeg, writes the encoded bytes to `sink`.
/// Returns when the source ends or the sink fails (client disconnected).
pub fn stream(url: &str, opts: Opts, sink: Box<dyn Write + Send>) -> Result<Info, String> {
    if !capabilities().ffmpeg { return Err("ffmpeg is not installed on this server".into()); }
    let (src, ext) = open_url(url)?;
    let mut hint = Hint::new();
    if let Some(e) = ext { hint.with_extension(&e); }
    let mss = MediaSourceStream::new(src, MediaSourceStreamOptions::default());
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("probe: {e}"))?;
    let mut format = probed.format;
    let track = format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL).ok_or("no audio track")?.clone();
    if track.codec_params.codec == CODEC_TYPE_OPUS { return Err("Opus sources are not supported by the hi-fi path yet".into()); }
    let mut dec = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;
    let track_id = track.id;
    if opts.start_ms > 0 {
        let time = Time::new((opts.start_ms / 1000) as u64, (opts.start_ms % 1000) as f64 / 1000.0);
        let _ = format.seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(track_id) });
        dec.reset();
    }

    // decode one block first: gives the real sample rate / channel count before ffmpeg is configured
    let mut sbuf: Option<SampleBuffer<f32>> = None;
    let (first, rate, ch) = next_block(&mut format, &mut dec, track_id, &mut sbuf)?.ok_or("empty audio")?;
    let mut chain = FilterChain::new(rate);
    if let Some(cfg) = &opts.configure { cfg(&mut chain); }
    let filtered = !chain.is_identity();
    let ch_out = if filtered { 2 } else { ch.clamp(1, 2) };
    let plan = plan(opts.tier, opts.fmt, rate)?;

    // MQA sync-word scan on the RAW decoded samples (before any filter). Only meaningful for stereo sources.
    // Skipped when seeking (the sync word is near the start of the file). Finishes as soon as it has an answer.
    let mut det = (ch == 2 && opts.start_ms <= 0).then(mqa::Detector::new);
    let mut mqa_report: Option<mqa::Report> = None;
    if let Some(d) = det.as_mut() {
        if d.push(&first, ch) { mqa_report = det.take().map(|d| d.finish()); }
    }

    let mut args = s(&["-hide_banner", "-loglevel", "error", "-f", "f32le"]);
    args.extend(s(&["-ar", &rate.to_string(), "-ac", &ch_out.to_string(), "-i", "pipe:0"]));
    args.extend(plan.args.iter().cloned());
    args.extend(s(&["-flush_packets", "1", "pipe:1"]));
    let mut child = Command::new("ffmpeg").args(&args)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().map_err(|e| format!("ffmpeg: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("ffmpeg stdin")?;
    let mut stdout = child.stdout.take().ok_or("ffmpeg stdout")?;
    let stderr = child.stderr.take();

    // pump ffmpeg -> sink. After a sink error keep DRAINING (never block ffmpeg => no deadlock with the stdin writer).
    let dead = Arc::new(AtomicBool::new(false));
    let dead2 = dead.clone();
    let mut sink = sink;
    let pump = std::thread::spawn(move || -> u64 {
        let mut buf = vec![0u8; 16 * 1024];
        let mut total = 0u64;
        loop {
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if !dead2.load(Relaxed) {
                        if sink.write_all(&buf[..n]).is_err() { dead2.store(true, Relaxed); } else { total += n as u64; }
                    }
                }
            }
        }
        total
    });
    let err_thread = stderr.map(|mut e| std::thread::spawn(move || { let mut t = String::new(); let _ = e.read_to_string(&mut t); t }));

    let mut bytes: Vec<u8> = Vec::with_capacity(1 << 16);
    let mut tmp: Vec<f32> = Vec::new();
    let mut pending = Some(first);
    let mut failure: Option<String> = None;
    let mut ffmpeg_died = false;
    loop {
        let samples = match pending.take() {
            Some(x) => x,
            None => match next_block(&mut format, &mut dec, track_id, &mut sbuf) {
                Ok(Some((x, _, _))) => {
                    if let Some(d) = det.as_mut() {
                        if d.push(&x, ch) { mqa_report = det.take().map(|d| d.finish()); }
                    }
                    x
                }
                Ok(None) => break,
                Err(e) => { failure = Some(e); break; }
            },
        };
        if dead.load(Relaxed) { break; }
        let out = process_block(&samples, ch, filtered, &mut chain, &mut tmp);
        bytes.clear();
        for f in &out { bytes.extend_from_slice(&f.to_le_bytes()); }
        if stdin.write_all(&bytes).is_err() { ffmpeg_died = true; break; }
    }
    drop(stdin);
    if dead.load(Relaxed) || failure.is_some() { let _ = child.kill(); }
    let total = pump.join().unwrap_or(0);
    let _ = child.wait();
    let errtxt = err_thread.and_then(|t| t.join().ok()).unwrap_or_default();
    if let Some(e) = failure { return Err(e); }
    if ffmpeg_died && !dead.load(Relaxed) { return Err(format!("ffmpeg stopped: {}", errtxt.trim())); }
    // short tracks: the scan may not have reached a verdict yet - report what it saw
    let mqa_report = mqa_report.or_else(|| det.take().map(|d| d.finish()));
    Ok(Info { in_rate: rate, out_rate: plan.out_rate, channels: ch_out, bytes_out: total, filtered, mqa: mqa_report })
}

/// Decode just enough of `url` (at most ~30 s of audio) to look for the MQA sync word. Blocking.
/// Use for a cheap "is this file MQA?" check without encoding anything.
pub fn probe_mqa(url: &str) -> Result<mqa::Report, String> {
    let (src, ext) = open_url(url)?;
    let mut hint = Hint::new();
    if let Some(e) = ext { hint.with_extension(&e); }
    let mss = MediaSourceStream::new(src, MediaSourceStreamOptions::default());
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("probe: {e}"))?;
    let mut format = probed.format;
    let track = format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL).ok_or("no audio track")?.clone();
    let mut dec = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;
    let track_id = track.id;
    let mut sbuf: Option<SampleBuffer<f32>> = None;
    let mut det = mqa::Detector::new();
    while let Some((x, _rate, ch)) = next_block(&mut format, &mut dec, track_id, &mut sbuf)? {
        if det.push(&x, ch) { break; }
    }
    Ok(det.finish())
}

/// JSON view of a Report for the HTTP API
pub fn mqa_json(r: &mqa::Report) -> Value {
    json!({
        "mqa": r.is_mqa,
        "stereo": r.stereo,
        "syncBit": r.bit_pos,
        "syncFrame": r.sync_frame,
        "rawAfterSync": r.raw_after_sync.map(|v| format!("{v:#010x}")),
        "framesScanned": r.frames_scanned,
        "note": "detection only: MQA unfold/render is not implemented; the file plays as its normal 24-bit core"
    })
}

/// capability report for GET /hifi/tiers
pub fn describe() -> Value {
    let c = capabilities();
    let l = |t, f| content_type(t, f).1;
    json!({
      "ffmpeg": c.ffmpeg, "ffmpegVersion": c.version,
      "tiers": [
        {"id":"efficient","appleName":"High Efficiency (HE-AAC)","output": l(Tier::Efficient, Fmt::Auto),"container":"ADTS (audio/aac)"},
        {"id":"high","appleName":"High Quality (AAC 256 kbps)","output": l(Tier::High, Fmt::Auto),"container":"ADTS (audio/aac)"},
        {"id":"lossless","appleName":"Lossless (ALAC up to 24-bit/48 kHz)","output": l(Tier::Lossless, Fmt::Auto),"formats":["flac","alac","wav"],"bitPerfect":"yes when no filter is active and the source is lossless"},
        {"id":"hires","appleName":"Hi-Res Lossless (ALAC up to 24-bit/192 kHz)","output": l(Tier::HiRes, Fmt::Auto),"formats":["flac","alac","wav"],"bitPerfect":"yes when no filter is active and the source is lossless"}
      ],
      "mqa": {"detect": true, "decode": false},
      "notes": [
        "Not carried by Discord voice (Opus only); this is the web/HTTP output.",
        "A lossy source (MP3, SoundCloud, ...) cannot become better than it is; lossless tiers only preserve what the source has.",
        "Opus sources are not supported on this path yet.",
        "MQA: sources are scanned for the MQA sync word and reported; there is no MQA unfold, the file is passed through as its 24-bit core."
      ]
    })
}
