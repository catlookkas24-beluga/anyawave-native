//! Producer (decode -> resample -> filters -> opus) fills an SPSC ring with ready
//! 20 ms Opus packets; the shard thread only pops + encrypts + sends.
//! => the network clock never waits on decode/IO => no stutter.
use rtrb::{Consumer, Producer, RingBuffer};

pub const TARGET_BUFFER_FRAMES: usize = 15; // ~300 ms ahead; flush on seek / filter change
pub const RING_FRAMES: usize = 64;
pub const OPUS_SILENCE: [u8; 3] = [0xF8, 0xFF, 0xFE];

#[derive(Clone)]
pub struct OpusFrame { pub len: u16, pub data: [u8; 1275] }

pub fn frame_ring() -> (Producer<OpusFrame>, Consumer<OpusFrame>) { RingBuffer::new(RING_FRAMES) }

// Producer loop (runs on a bounded blocking pool, NOT on the tick thread):
// 1. chain.is_identity() && source is Opus -> forward packets unchanged (passthrough)
// 2. else decode -> f32 stereo 48k (symphonia / rubato) -> chain.process -> libopus encode
//    (complexity 5-6, 96-128 kbps VBR, 20 ms frames)
// 3. push only while the ring holds < TARGET_BUFFER_FRAMES; else park ~5 ms.
