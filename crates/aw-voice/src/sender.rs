//! Runs on a scheduler shard thread, exactly once per 20 ms per voice connection:
//! pop Opus packet -> DAVE E2EE -> RTP header -> AEAD transport -> UDP.
use crate::{crypto::Transport, dave::DaveCtx, rtp::Rtp};
use aw_engine::player::OPUS_SILENCE;
use aw_engine::producer::{Control, Packet};
use aw_engine::scheduler::Tickable;
use rtrb::Consumer;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::{Acquire, Relaxed}};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;

pub enum VoiceEvent {
    Connected,
    TrackEnded { error: Option<String> },
    Closed { code: u16, reason: String },
}

pub struct Active { pub cons: Consumer<Packet>, pub ctl: Arc<Control> }

/// Hand-over point between the player (control plane) and the voice sender (tick thread).
#[derive(Default)]
pub struct PlayerSlot { pub inner: Mutex<Option<Active>> }

/// `deficit` = ticks where the buffer was empty mid-track (your stutter KPI). `nulled` = silence frames sent.
#[derive(Default)]
pub struct FrameStats { pub sent: AtomicU64, pub nulled: AtomicU64, pub deficit: AtomicU64 }

pub struct VoiceSender {
    sock: UdpSocket,
    transport: Transport,
    rtp: Rtp,
    dave: Arc<Mutex<DaveCtx>>,
    slot: Arc<PlayerSlot>,
    events: UnboundedSender<VoiceEvent>,
    stats: Arc<FrameStats>,
    closed: Arc<AtomicBool>,
    buf: Vec<u8>,
    enc: Vec<u8>,
    playing: bool,
    silence_left: u8,
}

impl VoiceSender {
    #[allow(clippy::too_many_arguments)]
    pub fn new(sock: UdpSocket, transport: Transport, rtp: Rtp, dave: Arc<Mutex<DaveCtx>>, slot: Arc<PlayerSlot>,
               events: UnboundedSender<VoiceEvent>, stats: Arc<FrameStats>, closed: Arc<AtomicBool>) -> Self {
        Self { sock, transport, rtp, dave, slot, events, stats, closed,
               buf: Vec::with_capacity(1500), enc: Vec::with_capacity(1500), playing: false, silence_left: 0 }
    }

    fn send_opus(&mut self, opus: &[u8]) {
        let ok = match self.dave.lock() {
            Ok(mut d) => d.encrypt(opus, &mut self.enc).is_ok(),
            Err(_) => false,
        };
        if !ok { return; }
        let header = self.rtp.header();
        self.transport.seal(&header, &self.enc, &mut self.buf);
        let _ = self.sock.send(&self.buf);
        self.stats.sent.fetch_add(1, Relaxed);
    }
}

impl Tickable for VoiceSender {
    fn tick(&mut self, _now: Instant) -> bool {
        if self.closed.load(Relaxed) { return false; }

        let mut frame: Option<Packet> = None;
        let mut ended: Option<Option<String>> = None;
        let mut underrun = false;
        {
            let Ok(mut g) = self.slot.inner.lock() else { return true };
            if let Some(a) = g.as_mut() {
                if !a.ctl.paused.load(Relaxed) {
                    let ep = a.ctl.epoch.load(Relaxed);
                    while let Ok(p) = a.cons.pop() {
                        if p.epoch == ep {
                            a.ctl.played_ms.store(p.pos_ms, Relaxed);
                            frame = Some(p);
                            break;
                        } // else: stale packet from before a seek -> drop
                    }
                    if frame.is_none() {
                        if a.ctl.finished.load(Acquire) && a.cons.is_empty() {
                            ended = Some(a.ctl.error.lock().ok().and_then(|e| e.clone()));
                        } else {
                            underrun = true;
                        }
                    }
                }
            }
            if ended.is_some() { *g = None; }
        }

        if let Some(p) = frame {
            self.playing = true;
            self.silence_left = 0;
            let n = p.len as usize;
            self.send_opus(&p.data[..n]);
        } else {
            // track ended / paused / stopped: 5 silence frames so Discord does not interpolate on resume
            if self.playing && !underrun {
                self.playing = false;
                self.silence_left = 5;
            }
            if self.silence_left > 0 {
                self.silence_left -= 1;
                self.send_opus(&OPUS_SILENCE);
                self.stats.nulled.fetch_add(1, Relaxed);
            } else if underrun {
                self.rtp.skip();
                self.stats.deficit.fetch_add(1, Relaxed);
            }
        }

        if let Some(err) = ended { let _ = self.events.send(VoiceEvent::TrackEnded { error: err }); }
        true
    }
}
