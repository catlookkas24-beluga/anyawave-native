//! One tick thread drives MANY players (sharded). 20 ms absolute-deadline clock (no drift).
//! Tuned for starved CPUs (e.g. a 0.1 vCPU free host):
//!   AW_SPIN_US        busy-wait before each deadline, default 0 (spinning burns ~1.5% of a core all the time;
//!                     on a 0.1 vCPU quota that is 15% of everything you own). Set 300 on a real VPS.
//!   AW_CATCHUP_FRAMES if a stall (CPU throttling / VM pause) makes us late, send the missed frames back-to-back
//!                     up to this many (default 10 = 200 ms); beyond that, resync instead of bursting.
use std::sync::mpsc::{channel, Sender};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use std::time::{Duration, Instant};

pub const FRAME: Duration = Duration::from_millis(20);

/// Implemented by a voice connection. Called once per 20 ms.
pub trait Tickable: Send {
    /// Return false to be removed from the shard.
    fn tick(&mut self, now: Instant) -> bool;
}

pub struct Shard { pub tx: Sender<Box<dyn Tickable>> }

fn env_u(k: &str, d: u64) -> u64 { std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d) }

pub fn spawn_shard(name: &str, stop: Arc<AtomicBool>) -> Shard {
    let (tx, rx) = channel::<Box<dyn Tickable>>();
    let spin = Duration::from_micros(env_u("AW_SPIN_US", 0));
    let catchup = env_u("AW_CATCHUP_FRAMES", 10) as u32;
    std::thread::Builder::new().name(name.into()).spawn(move || {
        set_realtime_priority();
        let mut players: Vec<Box<dyn Tickable>> = Vec::new();
        let mut next = Instant::now() + FRAME;
        while !stop.load(Ordering::Relaxed) {
            while let Ok(p) = rx.try_recv() { players.push(p); }
            let now = Instant::now();
            if now + spin < next { std::thread::sleep(next - now - spin); }
            if !spin.is_zero() { while Instant::now() < next { std::hint::spin_loop(); } }
            let t = Instant::now();
            players.retain_mut(|p| p.tick(t));
            next += FRAME;
            // missed deadlines are caught up by looping without sleeping (bounded burst)
            if Instant::now() > next + FRAME * catchup { next = Instant::now() + FRAME; }
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
