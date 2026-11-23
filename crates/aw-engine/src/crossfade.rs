//! Intelligent crossfade - pure signal + planning code (no I/O, no unsafe, no extra dependencies).
//! `producer.rs` owns the threads/decoders; this file owns the maths so it can be unit-tested alone.
//!
//! What makes it "intelligent" (all of it is cheap: a few multiplies per sample, only during the fade):
//!  * equal-power curve (sin/cos table): no loudness dip or bump in the middle of the fade
//!  * skips dead air: leading silence of the incoming track is trimmed; trailing silence of the outgoing track
//!    triggers the transition early
//!  * boundary loudness match: the incoming track is nudged (max +-6 dB, eased back to 0 dB by the end of the
//!    fade) toward the level of the outgoing track, so a quiet song after a loud one does not jump
//!  * fade length adapts: never more than 40 % of the shorter track, short when the user skips manually
//!    (<= 2.5 s) or when the outgoing track is already silent (<= 0.8 s)
//!
//! All audio here is interleaved stereo f32 @ 48 kHz.
use std::collections::VecDeque;
use std::sync::OnceLock;

pub const SR: usize = 48_000;
pub const MIN_FADE_MS: u32 = 300;
pub const MAX_FADE_MS: u32 = 12_000;
/// RMS below this (dBFS) counts as silence
pub const SILENCE_DB: f32 = -60.0;

// ---------------------------------------------------------------- curve
const TAB_N: usize = 1024;

fn sin_table() -> &'static [f32; TAB_N + 1] {
    static T: OnceLock<[f32; TAB_N + 1]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0f32; TAB_N + 1];
        for (i, v) in t.iter_mut().enumerate() {
            *v = ((i as f64 / TAB_N as f64) * std::f64::consts::FRAC_PI_2).sin() as f32;
        }
        t
    })
}

/// sin(x * pi/2) for x in 0..=1 via the table + linear interpolation (error < 3e-7)
#[inline]
fn qsin(x: f32, t: &[f32; TAB_N + 1]) -> f32 {
    let p = x.clamp(0.0, 1.0) * TAB_N as f32;
    let i = (p as usize).min(TAB_N - 1);
    let f = p - i as f32;
    t[i] + (t[i + 1] - t[i]) * f
}

/// equal-power gains at fade position t (0 = start, 1 = end): (outgoing, incoming).  out^2 + in^2 = 1
pub fn gains(t: f32) -> (f32, f32) {
    let tab = sin_table();
    (qsin(1.0 - t, tab), qsin(t, tab))
}

/// dst = out * g_out(t) + inn * g_in(t) * trim(t)   for frames f0.. of a fade that is `total` frames long.
/// `norm` is the loudness trim for the incoming track at the START of the fade; it eases to 1.0 at the end.
/// `out`, `inn`, `dst` are equal-length interleaved stereo slices.
pub fn mix_block(out: &[f32], inn: &[f32], dst: &mut [f32], f0: usize, total: usize, norm: f32) {
    let tab = sin_table();
    let frames = dst.len() / 2;
    let inv = 1.0 / total.max(1) as f32;
    for i in 0..frames {
        let t = (f0 + i) as f32 * inv;
        let go = qsin(1.0 - t, tab);
        let trim = 1.0 + (norm - 1.0) * (1.0 - t.min(1.0));
        let gi = qsin(t, tab) * trim;
        dst[2 * i] = out[2 * i] * go + inn[2 * i] * gi;
        dst[2 * i + 1] = out[2 * i + 1] * go + inn[2 * i + 1] * gi;
    }
}

// ---------------------------------------------------------------- level / silence
pub fn rms_db(pcm: &[f32]) -> f32 {
    if pcm.is_empty() { return -120.0; }
    let s: f64 = pcm.iter().map(|&x| (x as f64) * (x as f64)).sum();
    let ms = s / pcm.len() as f64;
    if ms <= 1e-12 { -120.0 } else { (10.0 * ms.log10()) as f32 }
}

/// frames of leading silence (10 ms resolution), at most `max_frames`
pub fn leading_silence_frames(pcm: &[f32], thresh_db: f32, max_frames: usize) -> usize {
    const W: usize = 480; // 10 ms
    let mut skipped = 0usize;
    for w in pcm.chunks_exact(W * 2) {
        if skipped >= max_frames || rms_db(w) > thresh_db { break; }
        skipped += W;
    }
    skipped.min(max_frames)
}

/// short linear fade-in on the first `frames` frames (hides the cut after trimming silence)
pub fn apply_fade_in(pcm: &mut [f32], frames: usize) {
    let n = frames.min(pcm.len() / 2);
    for i in 0..n {
        let g = i as f32 / n as f32;
        pcm[2 * i] *= g;
        pcm[2 * i + 1] *= g;
    }
}

/// running RMS over the most recent `cap` frames
pub struct Level { win: VecDeque<(f64, usize)>, sum: f64, frames: usize, cap: usize }
impl Level {
    pub fn new(cap_frames: usize) -> Self { Self { win: VecDeque::new(), sum: 0.0, frames: 0, cap: cap_frames } }
    pub fn reset(&mut self) { self.win.clear(); self.sum = 0.0; self.frames = 0; }
    pub fn push(&mut self, pcm: &[f32]) {
        let frames = pcm.len() / 2;
        if frames == 0 { return; }
        let e: f64 = pcm.iter().map(|&x| (x as f64) * (x as f64)).sum();
        self.win.push_back((e, frames));
        self.sum += e;
        self.frames += frames;
        while self.frames > self.cap {
            match self.win.pop_front() {
                Some((e0, f0)) => { self.sum -= e0; self.frames -= f0; }
                None => break,
            }
        }
    }
    pub fn db(&self) -> f32 {
        if self.frames == 0 { return -120.0; }
        let ms = self.sum / (self.frames * 2) as f64;
        if ms <= 1e-12 { -120.0 } else { (10.0 * ms.log10()) as f32 }
    }
}

/// how long the signal has been continuously below `thresh_db`
pub struct Silence { run: usize, thresh_db: f32 }
impl Silence {
    pub fn new(thresh_db: f32) -> Self { Self { run: 0, thresh_db } }
    pub fn reset(&mut self) { self.run = 0; }
    pub fn push(&mut self, pcm: &[f32]) {
        let f = pcm.len() / 2;
        if f == 0 { return; }
        if rms_db(pcm) < self.thresh_db { self.run += f; } else { self.run = 0; }
    }
    pub fn run_ms(&self) -> u64 { (self.run * 1000 / SR) as u64 }
}

// ---------------------------------------------------------------- planning
pub struct PlanIn {
    pub requested_ms: u32,
    pub smart: bool,
    pub manual: bool,       // user pressed skip / replaced the track
    pub silence_cut: bool,  // outgoing track already went silent
    pub len_out_ms: Option<u64>,
    pub len_in_ms: Option<u64>,
    pub rms_out_db: f32,    // recent level of the outgoing track
    pub rms_in_db: f32,     // level of the first seconds of the incoming track
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plan { pub frames: usize, pub in_gain: f32 }

pub fn plan(p: &PlanIn) -> Plan {
    let mut ms = p.requested_ms.clamp(MIN_FADE_MS, MAX_FADE_MS) as f64;
    let mut gain = 1.0f32;
    if p.smart {
        if p.manual { ms = ms.min(2500.0); }
        if p.silence_cut { ms = ms.min(800.0); }
        if let Some(l) = p.len_out_ms { ms = ms.min(l as f64 * 0.4); }
        if let Some(l) = p.len_in_ms { ms = ms.min(l as f64 * 0.4); }
        if p.rms_out_db > SILENCE_DB && p.rms_in_db > SILENCE_DB {
            let d = (p.rms_out_db - p.rms_in_db).clamp(-6.0, 6.0) * 0.7;
            gain = 10f32.powf(d / 20.0);
        }
    }
    let ms = ms.max(MIN_FADE_MS as f64);
    Plan { frames: (ms * 48.0) as usize, in_gain: gain }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_power_everywhere() {
        for i in 0..=1000 {
            let t = i as f32 / 1000.0;
            let (o, n) = gains(t);
            assert!((o * o + n * n - 1.0).abs() < 1e-5, "t={t}");
        }
        let (o, n) = gains(0.0);
        assert!((o - 1.0).abs() < 1e-6 && n.abs() < 1e-6);
        let (o, n) = gains(1.0);
        assert!(o.abs() < 1e-6 && (n - 1.0).abs() < 1e-6);
    }

    #[test]
    fn mix_keeps_constant_power_for_uncorrelated_input() {
        // two orthogonal square-ish signals of equal level: output power must stay ~ constant
        let total = 48_000;
        let mut worst = 0f32;
        let blk = 960;
        for b in 0..(total / blk) {
            let out: Vec<f32> = (0..blk * 2).map(|i| if (b * blk + i / 2) % 2 == 0 { 0.5 } else { -0.5 }).collect();
            let inn: Vec<f32> = (0..blk * 2).map(|i| if ((b * blk + i / 2) / 2) % 2 == 0 { 0.5 } else { -0.5 }).collect();
            let mut dst = vec![0f32; blk * 2];
            mix_block(&out, &inn, &mut dst, b * blk, total, 1.0);
            let p = dst.iter().map(|x| x * x).sum::<f32>() / dst.len() as f32;
            worst = worst.max((p - 0.25).abs());
        }
        assert!(worst < 0.05, "power drift {worst}");
    }

    #[test]
    fn loudness_trim_eases_to_unity() {
        let one = vec![1.0f32; 2];
        let mut d = vec![0f32; 2];
        // at the very end of the fade the trim must be 1.0 -> gain of incoming = 1
        mix_block(&[0.0, 0.0], &one, &mut d, 959, 960, 2.0);
        assert!((d[0] - 1.0).abs() < 0.01);
    }

    #[test]
    fn trims_leading_silence_only() {
        let mut pcm = vec![0f32; 480 * 2 * 20];             // 200 ms of silence
        pcm.extend(std::iter::repeat(0.3).take(480 * 2 * 10)); // then audio
        assert_eq!(leading_silence_frames(&pcm, SILENCE_DB, 48_000), 480 * 20);
        assert_eq!(leading_silence_frames(&pcm, SILENCE_DB, 480 * 5), 480 * 5); // capped
        let loud = vec![0.3f32; 480 * 2 * 5];
        assert_eq!(leading_silence_frames(&loud, SILENCE_DB, 48_000), 0);
    }

    #[test]
    fn level_window_forgets_old_audio() {
        let mut l = Level::new(1000);
        l.push(&vec![0.5f32; 2000]); // 1000 frames loud
        let loud = l.db();
        l.push(&vec![0.0f32; 2000]); // 1000 frames silent -> loud block falls out of the window
        assert!(loud > -7.0 && loud < -5.9);
        assert!(l.db() <= -100.0);
    }

    #[test]
    fn silence_run_counts_and_resets() {
        let mut s = Silence::new(SILENCE_DB);
        s.push(&vec![0.0f32; 2 * 9600]);
        assert_eq!(s.run_ms(), 200);
        s.push(&vec![0.2f32; 2 * 960]);
        assert_eq!(s.run_ms(), 0);
    }

    fn base() -> PlanIn {
        PlanIn { requested_ms: 6000, smart: true, manual: false, silence_cut: false,
                 len_out_ms: Some(200_000), len_in_ms: Some(200_000), rms_out_db: -14.0, rms_in_db: -14.0 }
    }

    #[test]
    fn plan_adapts_length() {
        assert_eq!(plan(&base()).frames, 6000 * 48);
        let mut p = base(); p.manual = true;
        assert_eq!(plan(&p).frames, 2500 * 48);
        let mut p = base(); p.silence_cut = true;
        assert_eq!(plan(&p).frames, 800 * 48);
        let mut p = base(); p.len_in_ms = Some(10_000);
        assert_eq!(plan(&p).frames, 4000 * 48);
        let mut p = base(); p.smart = false; p.manual = true; p.len_in_ms = Some(10_000);
        assert_eq!(plan(&p).frames, 6000 * 48); // fixed mode ignores all of it
        let mut p = base(); p.requested_ms = 50;
        assert_eq!(plan(&p).frames, 300 * 48);  // floor
    }

    #[test]
    fn plan_matches_loudness_within_limits() {
        let mut p = base(); p.rms_out_db = -10.0; p.rms_in_db = -20.0; // incoming 10 dB quieter
        let g = plan(&p).in_gain;
        let db = 20.0 * g.log10();
        assert!((db - 4.2).abs() < 0.05, "{db}");                      // 6 dB cap * 0.7
        let mut p = base(); p.rms_out_db = -80.0; p.rms_in_db = -20.0; // outgoing is silent -> no trim
        assert_eq!(plan(&p).in_gain, 1.0);
        let mut p = base(); p.smart = false; p.rms_out_db = -10.0; p.rms_in_db = -20.0;
        assert_eq!(plan(&p).in_gain, 1.0);
    }
}
