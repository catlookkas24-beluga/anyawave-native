pub const PT_OPUS: u8 = 0x78;
pub const SAMPLES_PER_FRAME: u32 = 960; // 20 ms @ 48 kHz

pub struct Rtp { pub seq: u16, pub ts: u32, pub ssrc: u32 }
impl Rtp {
    pub fn header(&mut self) -> [u8; 12] {
        let mut h = [0u8; 12];
        h[0] = 0x80; h[1] = PT_OPUS;
        h[2..4].copy_from_slice(&self.seq.to_be_bytes());
        h[4..8].copy_from_slice(&self.ts.to_be_bytes());
        h[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
        self.seq = self.seq.wrapping_add(1);
        self.ts = self.ts.wrapping_add(SAMPLES_PER_FRAME);
        h
    }
    /// underrun: time keeps moving so Discord's jitter buffer does not glitch on resume
    pub fn skip(&mut self) { self.ts = self.ts.wrapping_add(SAMPLES_PER_FRAME); self.seq = self.seq.wrapping_add(1); }
}
