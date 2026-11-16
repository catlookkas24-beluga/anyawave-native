//! Lavalink v4-compatible surface so existing clients (wavelink, lavalink.py, Lavalink4NET...) just connect.
//!   GET  /v4/websocket   headers: Authorization, User-Id, Client-Name, Session-Id(resume)
//!   PATCH /v4/sessions/{sid}/players/{gid}   {track, position, volume, paused, filters, voice{token,endpoint,sessionId,channelId}}
//!   GET  /v4/loadtracks?identifier=   GET /v4/decodetrack   GET /v4/info   GET /v4/stats   GET /version
use axum::{routing::{get, patch}, Router, extract::{Path, Query, State, ws::{WebSocketUpgrade, Message}}, Json, response::IntoResponse};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::{Arc, atomic::AtomicBool}};

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
    // 2. body.track.encoded / identifier -> start producer; position/seek flush ring
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
