//! Lavalink v4-compatible server.
//!   GET    /v4/websocket                      (Authorization, User-Id, Client-Name)
//!   PATCH  /v4/sessions/{sid}/players/{gid}   {track{encoded|identifier}, position, volume, paused, filters, voice{...}}
//!   GET    /v4/sessions/{sid}/players[/{gid}] DELETE /v4/sessions/{sid}/players/{gid}
//!   GET    /v4/loadtracks?identifier=<http(s) audio url>   GET /v4/decodetrack?encodedTrack=
//!   GET    /v4/info  /v4/stats  /version      GET /debug/probe?url=&secs=&key=
use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, patch},
    Json, Router,
};
use axum::body::{Body, Bytes};
use aw_engine::hifi::{self, Fmt, Opts, Tier};
use aw_engine::{
    dsp::FilterChain,
    producer::{self, packet_ring, run, Control, Input},
    scheduler::{spawn_shard, Tickable},
};
use aw_voice::{
    connection::{connect, VoiceHandle, VoiceParams},
    sender::{Active, PlayerSlot, VoiceEvent},
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed}, mpsc::Sender, Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

mod filters;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// ------------------------------------------------------------------ state
struct Shards { tx: Vec<Sender<Box<dyn Tickable>>>, next: AtomicUsize }
impl Shards {
    fn pick(&self) -> Sender<Box<dyn Tickable>> { self.tx[self.next.fetch_add(1, Relaxed) % self.tx.len()].clone() }
}

struct App {
    password: String,
    shards: Shards,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    started: Instant,
    counter: AtomicUsize,
}

struct Session {
    #[allow(dead_code)]
    id: String,
    user_id: u64,
    events: UnboundedSender<String>,
    players: Mutex<HashMap<String, Player>>,
}

struct Player {
    gid: u64,
    voice: Option<VoiceHandle>,
    voice_info: Option<Value>,
    slot: Arc<PlayerSlot>,
    chain: Arc<Mutex<FilterChain>>,
    ctl: Option<Arc<Control>>,
    track: Option<Value>,
    volume: u32,
    paused: bool,
    filters: Value,
    vtx: UnboundedSender<VoiceEvent>,
}

fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }
fn emit(s: &Session, v: Value) { let _ = s.events.send(v.to_string()); }
fn authed(app: &App, h: &HeaderMap) -> bool { h.get("authorization").map(|v| v == app.password.as_str()).unwrap_or(false) }
fn unauthorized() -> (StatusCode, Json<Value>) { (StatusCode::UNAUTHORIZED, Json(json!({"error":"Unauthorized"}))) }

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let stop = Arc::new(AtomicBool::new(false));
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).clamp(1, 4);
    let shards = Shards { tx: (0..n).map(|i| spawn_shard(&format!("aw-shard-{i}"), stop.clone()).tx).collect(), next: AtomicUsize::new(0) };
    let app = Arc::new(App {
        password: std::env::var("AW_PASSWORD").unwrap_or("youshallnotpass".into()),
        shards, sessions: Mutex::new(HashMap::new()), started: Instant::now(), counter: AtomicUsize::new(0),
    });
    let r = Router::new()
        .route("/version", get(|| async { "4.0.0-anyawave" }))
        .route("/v4/info", get(info))
        .route("/v4/stats", get(stats))
        .route("/v4/loadtracks", get(load_tracks))
        .route("/v4/decodetrack", get(decode_track))
        .route("/v4/filters", get(|| async { Json(filters::schema()) }))
        .route("/v4/websocket", get(ws))
        .route("/v4/sessions/:sid", patch(patch_session))
        .route("/v4/sessions/:sid/players", get(list_players))
        .route("/v4/sessions/:sid/players/:gid", get(get_player).patch(update_player).delete(delete_player))
        .route("/hifi/tiers", get(hifi_tiers))
        .route("/hifi/stream", get(hifi_stream))
        .route("/debug/probe", get(debug_probe))
        .route("/debug/sc", get(debug_sc))
        .with_state(app);
    let port = std::env::var("PORT").unwrap_or("2333".into());
    let l = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await.unwrap();
    axum::serve(l, r).await.unwrap();
}

// ------------------------------------------------------------------ simple endpoints
async fn info() -> Json<Value> {
    Json(json!({"version":{"semver":"4.0.0","major":4,"minor":0,"patch":0},
        "buildTime":0,"git":{"branch":"main","commit":"dev","commitTime":0},"jvm":"n/a","lavaplayer":"n/a",
        "sourceManagers":["http","soundcloud"],"filters":["volume","equalizer","karaoke","timescale","tremolo","vibrato","distortion","rotation","channelMix","lowPass","preamp","parametricEq","stereo","echo","reverb","compressor","limiter","preset"],"plugins":[]}))
}

fn stats_json(app: &App) -> Value {
    let sessions: Vec<Arc<Session>> = app.sessions.lock().unwrap().values().cloned().collect();
    let (mut players, mut playing, mut sent, mut nulled, mut deficit) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for s in sessions {
        let ps = s.players.lock().unwrap();
        for p in ps.values() {
            players += 1;
            if p.track.is_some() && !p.paused { playing += 1; }
            if let Some(v) = &p.voice {
                sent += v.stats.sent.load(Relaxed); nulled += v.stats.nulled.load(Relaxed); deficit += v.stats.deficit.load(Relaxed);
            }
        }
    }
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    json!({"players":players,"playingPlayers":playing,"uptime":app.started.elapsed().as_millis() as u64,
        "memory":{"free":0,"used":0,"allocated":0,"reservable":0},
        "cpu":{"cores":cores,"systemLoad":0.0,"lavalinkLoad":0.0},
        "frameStats":{"sent":sent,"nulled":nulled,"deficit":deficit}})   // deficit = underruns = stutter KPI
}
async fn stats(State(app): State<Arc<App>>) -> Json<Value> { Json(stats_json(&app)) }

fn make_info(url: &str) -> Value {
    let title = url.split('?').next().and_then(|u| u.rsplit('/').next()).filter(|s| !s.is_empty()).unwrap_or("Unknown title");
    json!({"identifier":url,"isSeekable":true,"author":"Unknown","length":0,"isStream":false,"position":0,
           "title":title,"uri":url,"artworkUrl":null,"isrc":null,"sourceName":"http"})
}
fn make_track(info: Value, user_data: Value) -> Value {
    json!({"encoded": B64.encode(info.to_string()), "info": info, "pluginInfo": {}, "userData": user_data})
}

fn sc_info(t: &aw_engine::sources::soundcloud::ScTrack) -> Value {
    let title = if t.preview_only { format!("{} (30 s preview)", t.title) } else { t.title.clone() };
    json!({"identifier": t.id.to_string(), "isSeekable": true, "author": t.author, "length": t.duration_ms,
           "isStream": false, "position": 0, "title": title,
           "uri": t.permalink, "artworkUrl": t.artwork, "isrc": null, "sourceName": "soundcloud"})
}

fn load_error(msg: String) -> Value { json!({"loadType":"error","data":{"message":msg,"severity":"common","cause":""}}) }

/// blocking (network): runs on the blocking pool
fn load_identifier(id: &str) -> Value {
    use aw_engine::sources::soundcloud as sc;
    if let Some(q) = id.strip_prefix("scsearch:") {
        return match sc::search(q.trim(), 15) {
            Ok(v) if v.is_empty() => json!({"loadType":"empty","data":{}}),
            Ok(v) => json!({"loadType":"search","data": v.iter().map(|t| make_track(sc_info(t), json!({}))).collect::<Vec<_>>()}),
            Err(e) => load_error(e),
        };
    }
    if sc::is_soundcloud(id) {
        return match sc::resolve(id) {
            Ok(sc::Resolved::Track(t)) => json!({"loadType":"track","data": make_track(sc_info(&t), json!({}))}),
            Ok(sc::Resolved::Playlist { name, tracks }) => json!({"loadType":"playlist","data":{
                "info":{"name":name,"selectedTrack":-1},"pluginInfo":{},
                "tracks": tracks.iter().map(|t| make_track(sc_info(t), json!({}))).collect::<Vec<_>>()}}),
            Err(e) => load_error(e),
        };
    }
    if id.starts_with("http://") || id.starts_with("https://") {
        return json!({"loadType":"track","data": make_track(make_info(id), json!({}))});
    }
    json!({"loadType":"empty","data":{}})   // ytsearch: etc. are not implemented
}

async fn load_tracks(State(app): State<Arc<App>>, h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> (StatusCode, Json<Value>) {
    if !authed(&app, &h) { return unauthorized(); }
    let id = q.get("identifier").cloned().unwrap_or_default();
    let v = tokio::task::spawn_blocking(move || load_identifier(&id)).await.unwrap_or_else(|e| load_error(e.to_string()));
    (StatusCode::OK, Json(v))
}

async fn decode_track(State(app): State<Arc<App>>, h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> (StatusCode, Json<Value>) {
    if !authed(&app, &h) { return unauthorized(); }
    let enc = q.get("encodedTrack").cloned().unwrap_or_default();
    match B64.decode(enc.as_bytes()).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
        Some(info) => (StatusCode::OK, Json(make_track(info, json!({})))),
        None => (StatusCode::BAD_REQUEST, Json(json!({"error":"invalid encodedTrack"}))),
    }
}

async fn patch_session(State(app): State<Arc<App>>, h: HeaderMap) -> (StatusCode, Json<Value>) {
    if !authed(&app, &h) { return unauthorized(); }
    (StatusCode::OK, Json(json!({"resuming":false,"timeout":60})))
}

// ------------------------------------------------------------------ players
fn player_json(p: &Player) -> Value {
    let (connected, ping) = p.voice.as_ref().map(|v| (v.connected.load(Relaxed), v.ping_ms.load(Relaxed))).unwrap_or((false, -1));
    let pos = p.ctl.as_ref().map(|c| c.played_ms.load(Relaxed)).unwrap_or(0);
    let vi = p.voice_info.clone().unwrap_or(json!({}));
    json!({"guildId":p.gid.to_string(),"track":p.track,"volume":p.volume,"paused":p.paused,
        "state":{"time":now_ms(),"position":pos,"connected":connected,"ping":ping},
        "voice":{"token":vi["token"],"endpoint":vi["endpoint"],"sessionId":vi["sessionId"]},"filters":p.filters})
}

fn new_player(sess: &Arc<Session>, gid: u64) -> Player {
    let (vtx, vrx) = unbounded_channel::<VoiceEvent>();
    spawn_forwarder(sess.clone(), gid.to_string(), vrx);
    Player { gid, voice: None, voice_info: None, slot: Arc::new(PlayerSlot::default()),
        chain: Arc::new(Mutex::new(FilterChain::new(48000))), ctl: None, track: None, volume: 100, paused: false,
        filters: json!({}), vtx }
}

/// voice-thread events -> Lavalink WebSocket events
fn spawn_forwarder(sess: Arc<Session>, gid: String, mut vrx: UnboundedReceiver<VoiceEvent>) {
    tokio::spawn(async move {
        while let Some(ev) = vrx.recv().await {
            match ev {
                VoiceEvent::Connected => {}
                VoiceEvent::TrackEnded { error } => {
                    let track = { let mut ps = sess.players.lock().unwrap(); ps.get_mut(&gid).and_then(|p| { p.ctl = None; p.track.take() }) };
                    if let Some(t) = track {
                        let reason = if error.is_some() { "loadFailed" } else { "finished" };
                        if let Some(e) = &error {
                            emit(&sess, json!({"op":"event","type":"TrackExceptionEvent","guildId":gid,"track":t,
                                "exception":{"message":e,"severity":"common","cause":e}}));
                        }
                        emit(&sess, json!({"op":"event","type":"TrackEndEvent","guildId":gid,"track":t,"reason":reason}));
                    }
                }
                VoiceEvent::Closed { code, reason } => {
                    emit(&sess, json!({"op":"event","type":"WebSocketClosedEvent","guildId":gid,"code":code,"reason":reason,"byRemote":true}));
                }
            }
        }
    });
}

fn stop_track(p: &mut Player, sess: &Session, reason: &str) {
    if let Some(c) = p.ctl.take() { c.stop.store(true, Relaxed); }
    *p.slot.inner.lock().unwrap() = None;
    if let Some(t) = p.track.take() {
        emit(sess, json!({"op":"event","type":"TrackEndEvent","guildId":p.gid.to_string(),"track":t,"reason":reason}));
    }
}

fn start_track(p: &mut Player, sess: &Session, url: String, track: Value, start_ms: i64) {
    stop_track(p, sess, "replaced");
    let (tx, rx) = packet_ring();
    let ctl = Control::new();
    ctl.paused.store(p.paused, Relaxed);
    producer::spawn(Input::Url(url), p.chain.clone(), tx, ctl.clone(), start_ms);
    *p.slot.inner.lock().unwrap() = Some(Active { cons: rx, ctl: ctl.clone() });
    p.ctl = Some(ctl);
    p.track = Some(track.clone());
    emit(sess, json!({"op":"event","type":"TrackStartEvent","guildId":p.gid.to_string(),"track":track}));
}

fn refresh_chain(p: &Player) {
    if let Ok(mut c) = p.chain.lock() { filters::apply(&mut c, &p.filters, p.volume); }
}

fn get_session(app: &App, sid: &str) -> Option<Arc<Session>> { app.sessions.lock().unwrap().get(sid).cloned() }

async fn list_players(State(app): State<Arc<App>>, h: HeaderMap, Path(sid): Path<String>) -> (StatusCode, Json<Value>) {
    if !authed(&app, &h) { return unauthorized(); }
    let Some(s) = get_session(&app, &sid) else { return (StatusCode::NOT_FOUND, Json(json!({"error":"session not found"}))) };
    let ps = s.players.lock().unwrap();
    (StatusCode::OK, Json(Value::Array(ps.values().map(player_json).collect())))
}

async fn get_player(State(app): State<Arc<App>>, h: HeaderMap, Path((sid, gid)): Path<(String, String)>) -> (StatusCode, Json<Value>) {
    if !authed(&app, &h) { return unauthorized(); }
    let Some(s) = get_session(&app, &sid) else { return (StatusCode::NOT_FOUND, Json(json!({"error":"session not found"}))) };
    let ps = s.players.lock().unwrap();
    match ps.get(&gid) { Some(p) => (StatusCode::OK, Json(player_json(p))), None => (StatusCode::NOT_FOUND, Json(json!({"error":"player not found"}))) }
}

async fn delete_player(State(app): State<Arc<App>>, h: HeaderMap, Path((sid, gid)): Path<(String, String)>) -> StatusCode {
    if !authed(&app, &h) { return StatusCode::UNAUTHORIZED; }
    let Some(s) = get_session(&app, &sid) else { return StatusCode::NOT_FOUND };
    let removed = s.players.lock().unwrap().remove(&gid);
    if let Some(mut p) = removed {
        stop_track(&mut p, &s, "cleanup");
        if let Some(v) = p.voice.take() { v.close(); }
    }
    StatusCode::NO_CONTENT
}

async fn update_player(State(app): State<Arc<App>>, h: HeaderMap, Path((sid, gid)): Path<(String, String)>,
                       Query(q): Query<HashMap<String, String>>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    if !authed(&app, &h) { return unauthorized(); }
    let Some(sess) = get_session(&app, &sid) else { return (StatusCode::NOT_FOUND, Json(json!({"error":"session not found"}))) };
    let Ok(gid_n) = gid.parse::<u64>() else { return (StatusCode::BAD_REQUEST, Json(json!({"error":"bad guildId"}))) };
    let no_replace = q.get("noReplace").map(|v| v == "true").unwrap_or(false);

    let mut ps = sess.players.lock().unwrap();
    let p = ps.entry(gid.clone()).or_insert_with(|| new_player(&sess, gid_n));

    // 1) voice server info -> (re)connect voice gateway
    if let Some(v) = body.get("voice") {
        let token = v["token"].as_str().unwrap_or("");
        let endpoint = v["endpoint"].as_str().unwrap_or("");
        let session_id = v["sessionId"].as_str().unwrap_or("");
        let channel = v["channelId"].as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| v["channelId"].as_u64()).unwrap_or(0);
        if !token.is_empty() && !endpoint.is_empty() && !session_id.is_empty() && p.voice_info.as_ref() != Some(v) {
            if let Some(old) = p.voice.take() { old.close(); }
            let params = VoiceParams { guild_id: gid_n, user_id: sess.user_id, channel_id: channel,
                session_id: session_id.into(), token: token.into(), endpoint: endpoint.into() };
            p.voice = Some(connect(params, app.shards.pick(), p.slot.clone(), p.vtx.clone()));
            p.voice_info = Some(v.clone());
        }
    }
    // 2) volume / filters (before the track starts so the chain is configured)
    if let Some(v) = body["volume"].as_u64() { p.volume = v.min(1000) as u32; }
    if let Some(f) = body.get("filters") { if f.is_object() { p.filters = f.clone(); } }
    if body.get("volume").is_some() || body.get("filters").is_some() { refresh_chain(p); }
    if let Some(b) = body["paused"].as_bool() { p.paused = b; if let Some(c) = &p.ctl { c.paused.store(b, Relaxed); } }

    // 3) track
    let mut started = false;
    if let Some(t) = body.get("track") {
        let user_data = t.get("userData").cloned().unwrap_or(json!({}));
        let start_ms = body["position"].as_i64().unwrap_or(0);
        match t.get("encoded") {
            Some(Value::Null) => stop_track(p, &sess, "stopped"),
            Some(Value::String(enc)) => {
                if !(no_replace && p.track.is_some()) {
                    match B64.decode(enc.as_bytes()).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
                        Some(info) => {
                            let url = info["uri"].as_str().unwrap_or("").to_string();
                            start_track(p, &sess, url, make_track(info, user_data), start_ms);
                            started = true;
                        }
                        None => return (StatusCode::BAD_REQUEST, Json(json!({"error":"invalid track.encoded"}))),
                    }
                }
            }
            _ => {
                if let Some(id) = t["identifier"].as_str() {
                    if !(no_replace && p.track.is_some()) {
                        start_track(p, &sess, id.to_string(), make_track(make_info(id), user_data), start_ms);
                        started = true;
                    }
                }
            }
        }
    }
    // 4) seek (only if no new track was started in this request)
    if !started {
        if let (Some(pos), Some(c)) = (body["position"].as_i64(), &p.ctl) { c.seek_ms.store(pos.max(0), Relaxed); }
    }
    (StatusCode::OK, Json(player_json(p)))
}

// ------------------------------------------------------------------ websocket
async fn ws(State(app): State<Arc<App>>, up: WebSocketUpgrade, h: HeaderMap) -> impl IntoResponse {
    if !authed(&app, &h) { return StatusCode::UNAUTHORIZED.into_response(); }
    let Some(uid) = h.get("user-id").and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<u64>().ok()) else {
        return (StatusCode::BAD_REQUEST, "User-Id header required").into_response();
    };
    up.on_upgrade(move |sock| ws_loop(app, uid, sock)).into_response()
}

async fn ws_loop(app: Arc<App>, uid: u64, mut s: WebSocket) {
    let id = format!("{:016x}", (now_ms() << 8) ^ app.counter.fetch_add(1, Relaxed) as u64);
    let (etx, mut erx) = unbounded_channel::<String>();
    let sess = Arc::new(Session { id: id.clone(), user_id: uid, events: etx, players: Mutex::new(HashMap::new()) });
    app.sessions.lock().unwrap().insert(id.clone(), sess.clone());
    let _ = s.send(Message::Text(json!({"op":"ready","resumed":false,"sessionId":id}).to_string())).await;

    let mut pu = tokio::time::interval(Duration::from_secs(5));
    let mut st = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            m = s.recv() => match m { Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break, _ => {} },
            e = erx.recv() => match e { Some(t) => { if s.send(Message::Text(t)).await.is_err() { break; } } None => break },
            _ = pu.tick() => {
                let msgs: Vec<String> = {
                    let ps = sess.players.lock().unwrap();
                    ps.values().filter(|p| p.voice.is_some()).map(|p| {
                        let pj = player_json(p);
                        json!({"op":"playerUpdate","guildId":p.gid.to_string(),"state":pj["state"]}).to_string()
                    }).collect()
                };
                for m in msgs { if s.send(Message::Text(m)).await.is_err() { break; } }
            }
            _ = st.tick() => {
                let mut v = stats_json(&app);
                v["op"] = json!("stats");
                if s.send(Message::Text(v.to_string())).await.is_err() { break; }
            }
        }
    }
    // cleanup: no resume support yet
    app.sessions.lock().unwrap().remove(&id);
    let players: Vec<Player> = sess.players.lock().unwrap().drain().map(|(_, p)| p).collect();
    for mut p in players {
        if let Some(c) = p.ctl.take() { c.stop.store(true, Relaxed); }
        *p.slot.inner.lock().unwrap() = None;
        if let Some(v) = p.voice.take() { v.close(); }
    }
}

// ------------------------------------------------------------------ pipeline self-test
async fn debug_probe(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    if q.get("key").map(|k| k.as_str()) != Some(app.password.as_str()) {
        return Json(json!({"error":"unauthorized: add &key=<AW_PASSWORD>"}));
    }
    let Some(url) = q.get("url").cloned() else { return Json(json!({"error":"missing &url="})) };
    let secs: u64 = q.get("secs").and_then(|s| s.parse().ok()).unwrap_or(10).clamp(1, 60);
    Json(tokio::task::spawn_blocking(move || probe(url, secs)).await.unwrap_or_else(|e| json!({"error": e.to_string()})))
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
            Ok(p) => { frames += 1; bytes += p.len as u64; if p.passthrough { pass += 1; } if frames >= want { ctl.stop.store(true, Relaxed); } }
            Err(_) => { if h.is_finished() { break; } std::thread::sleep(Duration::from_millis(1)); }
        }
    }
    let elapsed = t0.elapsed().as_secs_f64().max(0.001);
    let audio_s = frames as f64 * 0.02;
    match h.join() {
        Ok(Ok(st)) => json!({"ok":true,"codec":st.codec,"source_rate":st.source_rate,"frames":frames,"audio_seconds":audio_s,
            "passthrough_frames":pass,"avg_kbps": if audio_s > 0.0 { bytes as f64 * 8.0 / audio_s / 1000.0 } else { 0.0 },
            "wall_seconds":elapsed,"realtime_factor":audio_s / elapsed}),
        Ok(Err(e)) => json!({"ok":false,"error":e,"frames":frames}),
        Err(_) => json!({"ok":false,"error":"producer thread panicked"}),
    }
}

// ------------------------------------------------------------------ hi-fi output (HTTP, not Discord)
struct ChanWriter(tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>);
impl std::io::Write for ChanWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.blocking_send(Ok(Bytes::copy_from_slice(buf)))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client closed"))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

async fn hifi_tiers() -> Json<Value> {
    Json(tokio::task::spawn_blocking(hifi::describe).await.unwrap_or_else(|e| json!({"error": e.to_string()})))
}

/// GET /hifi/stream?key=<AW_PASSWORD>&url=<audio url>&tier=efficient|high|lossless|hires
///                 [&format=flac|alac|wav] [&t=<start seconds>] [&filters=<url-encoded filters JSON>]
async fn hifi_stream(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> Response {
    if q.get("key").map(|k| k.as_str()) != Some(app.password.as_str()) {
        return (StatusCode::UNAUTHORIZED, "add &key=<AW_PASSWORD>").into_response();
    }
    let Some(url) = q.get("url").cloned() else { return (StatusCode::BAD_REQUEST, "missing &url=").into_response() };
    let tier = match q.get("tier").map(|s| s.as_str()).unwrap_or("lossless") {
        "efficient" => Tier::Efficient, "high" => Tier::High, "lossless" => Tier::Lossless, "hires" => Tier::HiRes,
        _ => return (StatusCode::BAD_REQUEST, "tier must be efficient|high|lossless|hires").into_response(),
    };
    let fmt = match q.get("format").map(|s| s.as_str()).unwrap_or("flac") {
        "alac" => Fmt::Alac, "wav" => Fmt::Wav, _ => Fmt::Flac,
    };
    let start_ms = q.get("t").and_then(|s| s.parse::<f64>().ok()).map(|s| (s * 1000.0) as i64).unwrap_or(0);
    let filters: Option<Value> = q.get("filters").and_then(|s| serde_json::from_str(s).ok());

    let ffmpeg_ok = tokio::task::spawn_blocking(|| hifi::capabilities().ffmpeg).await.unwrap_or(false);
    if !ffmpeg_ok { return (StatusCode::SERVICE_UNAVAILABLE, "ffmpeg is not installed on this server").into_response(); }
    let (ct, label) = hifi::content_type(tier, fmt);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    let tx2 = tx.clone();
    tokio::task::spawn_blocking(move || {
        let configure: Option<Box<dyn Fn(&mut FilterChain) + Send>> = filters.map(|f| {
            Box::new(move |c: &mut FilterChain| filters::apply(c, &f, 100)) as Box<dyn Fn(&mut FilterChain) + Send>
        });
        let opts = Opts { tier, fmt, start_ms, configure };
        match hifi::stream(&url, opts, Box::new(ChanWriter(tx2.clone()))) {
            Ok(i) => tracing::info!("hifi done: {}->{} Hz, {} ch, {} bytes, filtered={}", i.in_rate, i.out_rate, i.channels, i.bytes_out, i.filtered),
            Err(e) => { tracing::warn!("hifi failed: {e}"); let _ = tx2.blocking_send(Err(std::io::Error::new(std::io::ErrorKind::Other, e))); }
        }
    });
    drop(tx);
    let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) }));
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", ct)
        .header("X-AW-Codec", label)
        .header("Cache-Control", "no-store")
        .body(body)
        .unwrap()
}

// ------------------------------------------------------------------ SoundCloud diagnostics
/// GET /debug/sc?key=<AW_PASSWORD>&url=<soundcloud link>  -> which step works / fails
async fn debug_sc(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    if q.get("key").map(|k| k.as_str()) != Some(app.password.as_str()) {
        return Json(json!({"error": "add &key=<AW_PASSWORD>"}));
    }
    let Some(url) = q.get("url").cloned() else { return Json(json!({"error": "missing &url="})) };
    Json(tokio::task::spawn_blocking(move || aw_engine::sources::soundcloud::diagnose(&url)).await
        .unwrap_or_else(|e| json!({"error": e.to_string()})))
}
