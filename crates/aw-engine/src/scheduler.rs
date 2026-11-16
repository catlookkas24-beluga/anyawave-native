//! One tick thread drives MANY players (sharded). 20 ms absolute-deadline clock
//! (no drift), sleep-then-spin for the last ~300 us. 1000 guilds != 1000 threads.
use std::sync::mpsc::{channel, Sender};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use std::time::{Duration, Instant};

pub const FRAME: Duration = Duration::from_millis(20);

/// Implemented by a voice connection. Called exactly once per 20 ms.
pub trait Tickable: Send {
    /// Return false to be removed from the shard.
    fn tick(&mut self, now: Instant) -> bool;
}

pub struct Shard { pub tx: Sender<Box<dyn Tickable>> }

pub fn spawn_shard(name: &str, stop: Arc<AtomicBool>) -> Shard {
    let (tx, rx) = channel::<Box<dyn Tickable>>();
    std::thread::Builder::new().name(name.into()).spawn(move || {
        set_realtime_priority();
        let mut players: Vec<Box<dyn Tickable>> = Vec::new();
        let mut next = Instant::now() + FRAME;
        while !stop.load(Ordering::Relaxed) {
            while let Ok(p) = rx.try_recv() { players.push(p); }
            let now = Instant::now();
            if now + Duration::from_micros(300) < next {
                std::thread::sleep(next - now - Duration::from_micros(300));
            }
            while Instant::now() < next { std::hint::spin_loop(); }
            let t = Instant::now();
            players.retain_mut(|p| p.tick(t));
            next += FRAME;
            // fell badly behind (VM pause etc.): resync instead of bursting packets
            if Instant::now() > next + FRAME * 5 { next = Instant::now() + FRAME; }
        }
    }).unwrap();
    Shard { tx }
}

#[cfg(target_os = "linux")]
fn set_realtime_priority() {
    // best effort: SCHED_FIFO needs CAP_SYS_NICE; ignore failure.
    unsafe {
        let p = libc::sched_param { sched_priority: 20 };
        libc::sched_setscheduler(0, libc::SCHED_FIFO, &p);
    }
}
#[cfg(not(target_os = "linux"))]
fn set_realtime_priority() {}
