//! Crossfade glue for the Lavalink-style API (all custom, optional fields - normal Lavalink clients are unaffected).
//!
//!   PATCH /v4/sessions/{sid}/players/{gid}
//!     "crossfade": {"durationMs": 6000, "mode": "smart" | "fixed" | "off"}   per player, stays until changed
//!     "nextTrack": {"encoded": "<track>"}  |  null                          queue / clear the track that follows
//!   and: a normal "track" replace while something is playing becomes a quick crossfade (<= 2.5 s in smart
//!   mode) when crossfade is on and the player is not paused.
//!
//! Events (TrackEndEvent / TrackStartEvent) are sent when the producer hands over to the next track.
//! The producer runs ~1 s ahead of what is audible (ring buffer), so the events can be early by about that much.
use super::{emit, make_track, Player, Session, B64};
use aw_engine::producer::{Handoff, HandoffCb, Input, NextTrack};
use base64::Engine;
use serde_json::{json, Value};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

/// default for new players: env AW_CROSSFADE_MS (0 = off)
pub fn default_ms() -> u32 {
    std::env::var("AW_CROSSFADE_MS").ok().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0).min(12_000)
}

pub fn default_smart() -> bool {
    std::env::var("AW_CROSSFADE_MODE").map(|v| v != "fixed").unwrap_or(true)
}

/// body["crossfade"] -> (milliseconds, smart)
pub fn parse(v: &Value) -> Option<(u32, bool)> {
    if !v.is_object() { return None; }
    let mode = v["mode"].as_str().unwrap_or("smart");
    let ms = if mode == "off" { 0 } else { v["durationMs"].as_u64().unwrap_or(6000).min(12_000) as u32 };
    Some((ms, mode != "fixed"))
}

pub fn mode_name(p: &Player) -> &'static str {
    if p.xfade_ms == 0 { "off" } else if p.xfade_smart { "smart" } else { "fixed" }
}

/// push the player's crossfade settings into the running producer
pub fn apply(p: &Player) {
    if let Some(c) = &p.ctl { c.set_crossfade(p.xfade_ms, p.xfade_smart); }
}

fn len_of(track: &Value) -> Option<u64> { track["info"]["length"].as_u64().filter(|&l| l > 0) }

fn next_of(track: &Value, start_ms: i64) -> Option<NextTrack> {
    let url = track["info"]["uri"].as_str().unwrap_or("").to_string();
    if url.is_empty() { return None; }
    Some(NextTrack { input: Input::Url(url), start_ms: start_ms.max(0), len_ms: len_of(track) })
}

/// called by start_track: wire the producer's hand-off notifications to Lavalink events
/// (call BEFORE the producer thread is spawned: it reads the length hint when it starts)
pub fn attach(p: &Player, sess: &Arc<Session>, ctl: &Arc<aw_engine::producer::Control>, track: &Value) {
    ctl.set_crossfade(p.xfade_ms, p.xfade_smart);
    ctl.set_on_handoff(Some(make_cb(sess, p.gid.to_string())));
    if let Some(l) = len_of(track) { ctl.len_hint_ms.store(l, Relaxed); }
}

/// A "replace track" request while music plays. true = handled as a crossfade, false = caller should hard-replace.
pub fn try_crossfade(p: &mut Player, track: &Value, start_ms: i64) -> bool {
    if p.xfade_ms == 0 || p.paused || p.track.is_none() { return false; }
    let Some(ctl) = p.ctl.clone() else { return false };
    if ctl.finished.load(Relaxed) { return false; }
    let Some(nt) = next_of(track, start_ms) else { return false };
    ctl.set_next(Some(nt));
    p.next_track = Some(track.clone());
    p.next_manual = true;
    ctl.request_skip();
    true
}

/// body["nextTrack"]: {"encoded": ...} queues, null / anything else clears
pub fn queue_next(p: &mut Player, v: &Value) {
    if p.next_manual { return; } // a skip is in flight; do not disturb it
    let info = v.get("encoded").and_then(|e| e.as_str())
        .and_then(|enc| B64.decode(enc.as_bytes()).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    match info {
        Some(info) => {
            let tv = make_track(info, v.get("userData").cloned().unwrap_or(json!({})));
            match next_of(&tv, 0) {
                Some(nt) => {
                    p.next_track = Some(tv);
                    if let Some(c) = &p.ctl { c.set_next(Some(nt)); }
                }
                None => {}
            }
        }
        None => {
            p.next_track = None;
            if let Some(c) = &p.ctl { c.set_next(None); }
        }
    }
}

fn make_cb(sess: &Arc<Session>, gid: String) -> HandoffCb {
    let weak = Arc::downgrade(sess);
    Arc::new(move |h: Handoff| {
        let Some(sess) = weak.upgrade() else { return };
        let mut ps = match sess.players.lock() { Ok(g) => g, Err(_) => return };
        let Some(p) = ps.get_mut(&gid) else { return };
        match &h {
            Handoff::Natural | Handoff::Manual => {
                let reason = if matches!(&h, Handoff::Manual) { "replaced" } else { "finished" };
                let old = p.track.take();
                p.track = p.next_track.take();
                p.next_manual = false;
                if let Some(t) = old {
                    emit(&sess, json!({"op":"event","type":"TrackEndEvent","guildId":gid,"track":t,"reason":reason}));
                }
                if let Some(t) = p.track.clone() {
                    emit(&sess, json!({"op":"event","type":"TrackStartEvent","guildId":gid,"track":t}));
                }
            }
            Handoff::Failed(msg) => {
                let manual = p.next_manual;
                p.next_manual = false;
                if let Some(t) = p.next_track.take() {
                    emit(&sess, json!({"op":"event","type":"TrackExceptionEvent","guildId":gid,"track":t,
                        "exception":{"message":msg,"severity":"common","cause":msg}}));
                    if manual {
                        // the client asked for this track, so tell it the load failed; the old track keeps playing
                        emit(&sess, json!({"op":"event","type":"TrackEndEvent","guildId":gid,"track":t,"reason":"loadFailed"}));
                    }
                }
            }
        }
    })
}
