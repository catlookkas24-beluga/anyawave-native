//! MQA *detection* (NOT decoding).
//!
//! MQA hides a control stream in the lowest bits of a 24-bit stereo PCM file. A known 36-bit sync word
//! (0xBE0498C88) shows up in the XOR of the left and right samples at one of three bit positions.
//! This module finds that word, so the server can say "this source is MQA" instead of silently
//! treating it as ordinary 24-bit audio.
//!
//! What it does NOT do: unfold / render (the "decoder" part of MQA). That algorithm is proprietary and is
//! not implemented here. An MQA FLAC is simply played as the normal 24-bit "core" it already is.
//!
//! VERIFY (written without cargo and without a real MQA file): the sync word value and the candidate bit
//! positions come from memory of public MQA-identifier tools. Test against a known MQA file first.
//! The bits that follow the sync word (original sample rate, "Studio" flag, ...) are returned RAW, unparsed.
//!
//! Input is f32 as produced by symphonia for 24-bit sources (value = int / 2^23, exact in f32).

pub const MAGIC: u64 = 0xB_E049_8C88;
const MAGIC_BITS: u32 = 36;
const MASK: u64 = (1u64 << MAGIC_BITS) - 1;
/// bit positions in a 24-bit sample (LSB = bit 0) whose L^R stream is scanned. VERIFY
/// (the same positions are 16,17,18 when samples are left-justified in 32 bits)
const CANDIDATE_BITS: [u32; 3] = [8, 9, 10];
/// raw bits captured after the sync word
const TAIL_BITS: u32 = 32;
/// stop scanning after this many frames if nothing was found (~30 s at 48 kHz)
const MAX_FRAMES: u64 = 1_500_000;
const SCALE: f32 = 8_388_608.0; // 2^23

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub is_mqa: bool,
    /// which candidate bit position carried the sync word
    pub bit_pos: Option<u32>,
    /// frame index (per channel) where the sync word ended
    pub sync_frame: Option<u64>,
    /// the 32 bits right after the sync word, first bit = MSB. NOT interpreted.
    pub raw_after_sync: Option<u32>,
    pub frames_scanned: u64,
    /// false when the source is not stereo (MQA is stereo only)
    pub stereo: bool,
}

pub struct Detector {
    buf: [u64; 3],
    frames: u64,
    found: Option<(usize, u64)>,
    tail: u32,
    tail_n: u32,
    stereo: bool,
}

impl Default for Detector { fn default() -> Self { Self::new() } }

impl Detector {
    pub fn new() -> Self {
        Self { buf: [0; 3], frames: 0, found: None, tail: 0, tail_n: 0, stereo: true }
    }

    /// Feed interleaved f32 samples. Returns true when no more input is needed.
    pub fn push(&mut self, interleaved: &[f32], ch: usize) -> bool {
        if ch != 2 { self.stereo = false; return true; }
        if self.done() { return true; }
        for f in interleaved.chunks_exact(2) {
            let l = q24(f[0]);
            let r = q24(f[1]);
            let x = (l ^ r) as u32;
            self.frames += 1;
            match self.found {
                None => {
                    for (i, &p) in CANDIDATE_BITS.iter().enumerate() {
                        self.buf[i] = ((self.buf[i] << 1) | ((x >> p) & 1) as u64) & MASK;
                        if self.buf[i] == MAGIC && self.found.is_none() {
                            self.found = Some((i, self.frames));
                        }
                    }
                }
                Some((i, _)) => {
                    let bit = (x >> CANDIDATE_BITS[i]) & 1;
                    self.tail = (self.tail << 1) | bit;
                    self.tail_n += 1;
                }
            }
            if self.done() { return true; }
        }
        self.done()
    }

    fn done(&self) -> bool {
        (self.found.is_some() && self.tail_n >= TAIL_BITS) || self.frames >= MAX_FRAMES
    }

    pub fn finish(self) -> Report {
        let tail = if self.found.is_some() && self.tail_n >= TAIL_BITS { Some(self.tail) } else { None };
        Report {
            is_mqa: self.found.is_some(),
            bit_pos: self.found.map(|(i, _)| CANDIDATE_BITS[i]),
            sync_frame: self.found.map(|(_, f)| f),
            raw_after_sync: tail,
            frames_scanned: self.frames,
            stereo: self.stereo,
        }
    }
}

#[inline]
fn q24(x: f32) -> i32 { ((x * SCALE).round() as i32).clamp(-8_388_608, 8_388_607) }

/// One-shot helper for an already-decoded buffer.
pub fn analyze(interleaved: &[f32], ch: usize) -> Report {
    let mut d = Detector::new();
    d.push(interleaved, ch);
    d.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
    }

    /// stereo stream whose L^R bit `pos` follows `bits`; everything else is noise
    fn synth(bits: &[u8], pos: u32, rng: &mut Rng) -> Vec<f32> {
        let mut v = Vec::with_capacity(bits.len() * 2);
        for &b in bits {
            let l = ((rng.next() as i64 % 8_000_000) as i32).clamp(-8_388_608, 8_388_607);
            let mut r = ((rng.next() as i64 % 8_000_000) as i32).clamp(-8_388_608, 8_388_607);
            // force bit `pos` of (l ^ r) to equal b
            if (((l ^ r) >> pos) & 1) as u8 != b { r ^= 1 << pos; }
            v.push(l as f32 / SCALE);
            v.push(r as f32 / SCALE);
        }
        v
    }

    fn bits_of(val: u64, n: u32) -> Vec<u8> { (0..n).rev().map(|i| ((val >> i) & 1) as u8).collect() }

    fn stream(pos: u32, rng: &mut Rng) -> (Vec<f32>, u32) {
        let mut bits: Vec<u8> = (0..500).map(|_| (rng.next() & 1) as u8).collect();
        bits.extend(bits_of(MAGIC, MAGIC_BITS));
        let after = 0xA5C3_1E7Fu32;
        bits.extend(bits_of(after as u64, 32));
        bits.extend((0..100).map(|_| (rng.next() & 1) as u8));
        (synth(&bits, pos, rng), after)
    }

    #[test]
    fn detects_on_every_candidate_bit() {
        for &p in &CANDIDATE_BITS {
            let mut rng = Rng(0x1234_5678_9abc_def1);
            let (s, after) = stream(p, &mut rng);
            let r = analyze(&s, 2);
            assert!(r.is_mqa, "bit {p}");
            assert_eq!(r.bit_pos, Some(p));
            assert_eq!(r.raw_after_sync, Some(after));
            assert_eq!(r.sync_frame, Some(536));
        }
    }

    #[test]
    fn works_across_arbitrary_chunking() {
        let mut rng = Rng(0xdead_beef_1234_5671);
        let (s, after) = stream(9, &mut rng);
        let mut d = Detector::new();
        let mut i = 0;
        let mut step = 2;
        while i < s.len() {
            let end = (i + step).min(s.len());
            d.push(&s[i..end], 2);
            i = end;
            step = (step % 14) + 2; // 2,4,..,14 samples (whole frames)
        }
        let r = d.finish();
        assert!(r.is_mqa);
        assert_eq!(r.raw_after_sync, Some(after));
    }

    #[test]
    fn plain_noise_is_not_mqa() {
        let mut rng = Rng(0x0bad_cafe_0bad_cafe);
        let bits: Vec<u8> = (0..200_000).map(|_| (rng.next() & 1) as u8).collect();
        let s = synth(&bits, 9, &mut rng);
        assert!(!analyze(&s, 2).is_mqa);
    }

    #[test]
    fn mono_is_not_mqa() {
        let r = analyze(&[0.1, 0.2, 0.3], 1);
        assert!(!r.is_mqa && !r.stereo);
    }
}
