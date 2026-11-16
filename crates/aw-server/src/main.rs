//! Lavalink v4-compatible surface so existing clients (wavelink, lavalink.py, Lavalink4NET...) just connect.
//!   GET  /v4/websocket   headers: Authorization, User-Id, Client-Name, Session-Id(resume)
//!   PATCH /v4/sessions/{sid}/players/{gid}   {track, position, volume, paused, filters, voice{token,endpoint,sessionId,channelId}}
//!   GET  /v4/loadtracks?identifier=   GET /v4/decodetrack   GET /v4/info   GET /v4/stats   GET /version
//!   GET  /debug/probe?url=<audio url>&secs=10&key=<AW_PASSWORD>   (pipeline self-test + benchmark)
use axum::{routing::{get, patch}, Router, extract::{Path, Query, State, ws::{WebSocketUpgrade, Message}}, Json, response::IntoResponse};
use aw_engine::dsp::FilterChain;
use aw_engine::producer::{packet_ring, run, Control, Input};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

struct App {
    password: String,
    #[allow(dead_code)]
    stop: Arc<AtomicBool>,
    // shards: Vec<aw_engine::scheduler::Shard>,   // N = min(cores, 4), players hashed by guild_id
    // sessions: DashMap<String, Session>,          // resume support
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let app = Arc::new(App {
        password: std::env::var("AW_PASSWORD").unwrap_or("youshallnotpass".into()),
        stop: Arc::new(AtomicBool::new(false)),
    });
    let r = Router::new()
        .route("/version", get(|| async { "4.0.0-anyawave" }))
        .route("/v4/info", get(info))
        .route("/v4/stats", get(stats))
        .route("/v4/loadtracks", get(load_tracks))
        .route("/v4/websocket", get(ws))
        .route("/v4/sessions/:sid/players/:gid", patch(update_player))
        .route("/debug/probe", get(debug_probe))
        .with_state(app);

    // Render (and most hosts) tell us which port to use via $PORT
    let port = std::env::var("PORT").unwrap_or("2333".into());
    let l = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await.unwrap();
    axum::serve(l, r).await.unwrap();
}

async fn info() -> Json<Value> {
    Json(json!({"version":{"semver":"4.0.0","major":4,"minor":0,"patch":0},
        "sourceManagers":["http","local","soundcloud"],"filters":["volume","equalizer","tremolo"],
        "plugins":[],"git":{"branch":"main","commit":"dev","commitTime":0},"jvm":"n/a","lavaplayer":"n/a"}))
}

async fn stats() -> Json<Value> {
    Json(json!({"players":0,"playingPlayers":0,"uptime":0,
        "memory":{"free":0,"used":0,"allocated":0,"reservable":0},
        "cpu":{"cores":0,"systemLoad":0.0,"lavalinkLoad":0.0},
        "frameStats":{"sent":0,"nulled":0,"deficit":0}}))   // deficit/nulled = your stutter KPI
}

async fn load_tracks(Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let _id = q.get("identifier");
    // TODO: resolve via SourceManager trait; encode track in Lavaplayer-compatible base64 blob
    Json(json!({"loadType":"empty","data":{}}))
}

async fn update_player(Path((sid, gid)): Path<(String, String)>, Json(body): Json<Value>) -> impl IntoResponse {
    // 1. body.voice -> aw_voice::gateway connect (v8, DAVE) -> spawn into shard
    // 2. body.track.encoded / identifier -> aw_engine::producer::spawn; position -> ctl.seek_ms
    // 3. body.filters -> FilterChain setters; identity => opus passthrough
    tracing::debug!(%sid, %gid, ?body);
    Json(json!({"guildId":gid,"volume":100,"paused":false,"state":{"time":0,"position":0,"connected":false,"ping":-1},"voice":{},"filters":{}}))
}

async fn ws(State(app): State<Arc<App>>, up: WebSocketUpgrade, h: axum::http::HeaderMap) -> impl IntoResponse {
    let ok = h.get("authorization").map(|v| v == app.password.as_str()).unwrap_or(false);
    up.on_upgrade(move |mut s| async move {
        if !ok { let _ = s.close().await; return; }
        let _ = s.send(Message::Text(json!({"op":"ready","resumed":false,"sessionId":"aw-0001"}).to_string())).await;
        // loop: emit stats every 60s, playerUpdate every 5s, TrackStart/End/Exception/Stuck + WebSocketClosed events
        while let Some(Ok(_)) = s.recv().await {}
    })
}

// ------------------------------------------------------------------ pipeline self-test
// Decodes `secs` seconds of audio as fast as possible and reports how many 20 ms Opus packets came out,
// how many were zero-cost passthrough, and the real-time factor (audio seconds produced per wall second).
async fn debug_probe(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    if q.get("key").map(|k| k.as_str()) != Some(app.password.as_str()) {
        return Json(json!({"error":"unauthorized: add &key=<AW_PASSWORD>"}));
    }
    let Some(url) = q.get("url").cloned() else { return Json(json!({"error":"missing &url="})) };
    let secs: u64 = q.get("secs").and_then(|s| s.parse().ok()).unwrap_or(10).clamp(1, 60);
    let v = tokio::task::spawn_blocking(move || probe(url, secs)).await
        .unwrap_or_else(|e| json!({"error": e.to_string()}));
    Json(v)
}

fn probe(url: String, secs: u64) -> Value {
    let chain = Arc::new(Mutex::new(FilterChain::new(48000)));
    let (tx, mut rx) = packet_ring();
    let ctl = Control::new();
    let c2 = ctl.clone();
    let t0 = Instant::now();
    let h = std::thread::spawn(move || run(Input::Url(url), chain, tx, c2, 0, false));
    let want = secs * 50;
    let (mut frames, mut pass, mut bytes) = (0u64, 0u64, 0u64);
    loop {
        match rx.pop() {
            Ok(p) => {
                frames += 1; bytes += p.len as u64;
                if p.passthrough { pass += 1; }
                if frames >= want { ctl.stop.store(true, Ordering::Relaxed); }
            }
            Err(_) => {
                if h.is_finished() { break; }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
    let elapsed = t0.elapsed().as_secs_f64().max(0.001);
    let audio_s = frames as f64 * 0.02;
    match h.join() {
        Ok(Ok(st)) => json!({
            "ok": true, "codec": st.codec, "source_rate": st.source_rate,
            "frames": frames, "audio_seconds": audio_s, "passthrough_frames": pass,
            "avg_kbps": if audio_s > 0.0 { bytes as f64 * 8.0 / audio_s / 1000.0 } else { 0.0 },
            "wall_seconds": elapsed, "realtime_factor": audio_s / elapsed
        }),
        Ok(Err(e)) => json!({"ok": false, "error": e, "frames": frames}),
        Err(_) => json!({"ok": false, "error": "producer thread panicked"}),
    }
}
