//! Voice gateway v8 connection (async, one tokio task per voice connection) + DAVE handshake.
//! Audio itself is NOT sent from here: after SESSION_DESCRIPTION a `VoiceSender` is handed to a scheduler shard.
use crate::dave::DaveCtx;
use crate::gateway::*;
use crate::{crypto::Transport, rtp::Rtp, sender::*, udp};
use aw_engine::scheduler::Tickable;
use futures_util::{stream::SplitSink, SinkExt, StreamExt};
use serde_json::{json, Value};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering::Relaxed};
use std::sync::{mpsc::Sender, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::TcpStream;
use tokio::sync::{mpsc::UnboundedSender, Notify};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

type Tx = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
type Fail = (u16, String);

#[derive(Clone)]
pub struct VoiceParams {
    pub guild_id: u64,
    pub user_id: u64,
    pub channel_id: u64,
    pub session_id: String,
    pub token: String,
    pub endpoint: String,
}

#[derive(Clone)]
pub struct VoiceHandle {
    pub connected: Arc<AtomicBool>,
    pub ping_ms: Arc<AtomicI64>,
    pub stats: Arc<FrameStats>,
    closed: Arc<AtomicBool>,
    notify: Arc<Notify>,
}
impl VoiceHandle {
    pub fn close(&self) {
        self.closed.store(true, Relaxed);
        self.notify.notify_one();
    }
}

/// Must be called from inside a tokio runtime.
pub fn connect(p: VoiceParams, shard: Sender<Box<dyn Tickable>>, slot: Arc<PlayerSlot>, events: UnboundedSender<VoiceEvent>) -> VoiceHandle {
    let h = VoiceHandle {
        connected: Arc::new(AtomicBool::new(false)),
        ping_ms: Arc::new(AtomicI64::new(-1)),
        stats: Arc::new(FrameStats::default()),
        closed: Arc::new(AtomicBool::new(false)),
        notify: Arc::new(Notify::new()),
    };
    let hh = h.clone();
    tokio::spawn(async move {
        let r = run(p, shard, slot, events.clone(), hh.clone()).await;
        hh.closed.store(true, Relaxed);
        hh.connected.store(false, Relaxed);
        if let Err((code, reason)) = r {
            tracing::warn!("voice closed: code={code} reason={reason}");
            let _ = events.send(VoiceEvent::Closed { code, reason });
        }
    });
    h
}

struct Conn {
    tx: Tx,
    p: VoiceParams,
    shard: Sender<Box<dyn Tickable>>,
    slot: Arc<PlayerSlot>,
    events: UnboundedSender<VoiceEvent>,
    h: VoiceHandle,
    seq: i64,
    ssrc: u32,
    sock: Option<UdpSocket>,
    hb_sent: Option<Instant>,
    hb_ms: Option<u64>,
    dave: Arc<Mutex<DaveCtx>>,
    pending: Option<(u16, u16)>, // (transition_id, protocol_version)
}

enum Ev { Stop, Beat, Msg(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>) }

async fn run(p: VoiceParams, shard: Sender<Box<dyn Tickable>>, slot: Arc<PlayerSlot>, events: UnboundedSender<VoiceEvent>, h: VoiceHandle) -> Result<(), Fail> {
    let host = p.endpoint.trim_start_matches("wss://").split(':').next().unwrap_or("").to_string();
    let (ws, _) = connect_async(format!("wss://{host}/?v=8")).await.map_err(|e| (1006u16, format!("connect: {e}")))?;
    tracing::info!("voice[{}] websocket connected to {host}", p.guild_id);
    let (tx, mut rx) = ws.split();
    let notify = h.notify.clone();
    let dave = Arc::new(Mutex::new(DaveCtx::new(p.user_id, p.channel_id)));
    let mut conn = Conn { tx, p: p.clone(), shard, slot, events, h, seq: -1, ssrc: 0, sock: None, hb_sent: None, hb_ms: None, dave, pending: None };
    conn.send_json(identify(p.guild_id, p.user_id, &p.session_id, &p.token)).await?;

    let mut hb: Option<tokio::time::Interval> = None;
    loop {
        if conn.h.closed.load(Relaxed) { break; }
        let ev = tokio::select! {
            _ = notify.notified() => Ev::Stop,
            _ = async { match hb.as_mut() { Some(i) => { i.tick().await; } None => std::future::pending::<()>().await } } => Ev::Beat,
            m = rx.next() => Ev::Msg(m),
        };
        match ev {
            Ev::Stop => break,
            Ev::Beat => conn.heartbeat().await?,
            Ev::Msg(None) => return Err((1006, "websocket stream ended".into())),
            Ev::Msg(Some(Err(e))) => return Err((1006, e.to_string())),
            Ev::Msg(Some(Ok(m))) => match m {
                Message::Text(t) => conn.on_text(&t).await?,
                Message::Binary(b) => conn.on_binary(&b).await?,
                Message::Close(cf) => {
                    let (c, r) = cf.map(|c| (u16::from(c.code), c.reason.to_string())).unwrap_or((1005, String::new()));
                    return Err((c, r));
                }
                _ => {}
            },
        }
        if let Some(ms) = conn.hb_ms.take() { hb = Some(tokio::time::interval(Duration::from_millis(ms.max(1000)))); }
    }
    Ok(())
}

impl Conn {
    async fn send_json(&mut self, v: Value) -> Result<(), Fail> {
        self.tx.send(Message::Text(v.to_string())).await.map_err(|e| (1006u16, e.to_string()))
    }
    async fn send_bin(&mut self, op: u8, payload: &[u8]) -> Result<(), Fail> {
        let mut m = Vec::with_capacity(1 + payload.len());
        m.push(op);
        m.extend_from_slice(payload);
        self.tx.send(Message::Binary(m)).await.map_err(|e| (1006u16, e.to_string()))
    }
    async fn heartbeat(&mut self) -> Result<(), Fail> {
        let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        self.hb_sent = Some(Instant::now());
        self.send_json(json!({"op":3,"d":{"t":t,"seq_ack":self.seq}})).await
    }

    fn execute(&mut self, id: u16) {
        if let Some((pid, ver)) = self.pending {
            if pid == id {
                self.pending = None;
                if let Ok(mut d) = self.dave.lock() { d.passthrough = ver == 0; }
            }
        }
    }

    /// unprocessable commit/welcome: tell the gateway, reset local group, send a fresh key package
    async fn recover(&mut self, id: Option<u16>) -> Result<(), Fail> {
        if let Some(id) = id { self.send_json(invalid_commit(id)).await?; }
        let kp = { let mut d = self.dave.lock().map_err(|_| (1011u16, "dave lock".to_string()))?; let v = d.version.max(1); d.init(v) };
        if let Some(kp) = kp.map_err(|e| (1011u16, e))? { self.send_bin(OP_MLS_KEY_PACKAGE, &kp).await?; }
        Ok(())
    }

    async fn on_text(&mut self, t: &str) -> Result<(), Fail> {
        let v: Value = serde_json::from_str(t).map_err(|e| (1007u16, e.to_string()))?;
        if let Some(s) = v["seq"].as_i64() { self.seq = s; }
        let d = &v["d"];
        match v["op"].as_u64().unwrap_or(999) as u8 {
            OP_HELLO => { self.hb_ms = d["heartbeat_interval"].as_f64().map(|x| x as u64); }
            OP_HEARTBEAT_ACK => {
                if let Some(t0) = self.hb_sent.take() { self.h.ping_ms.store(t0.elapsed().as_millis() as i64, Relaxed); }
            }
            OP_READY => {
                self.ssrc = d["ssrc"].as_u64().unwrap_or(0) as u32;
                let ip = d["ip"].as_str().unwrap_or("").to_string();
                let port = d["port"].as_u64().unwrap_or(0) as u16;
                let modes: Vec<String> = d["modes"].as_array()
                    .map(|a| a.iter().filter_map(|m| m.as_str().map(String::from)).collect()).unwrap_or_default();
                let mode = if modes.iter().any(|m| m == "aead_aes256_gcm_rtpsize") { "aead_aes256_gcm_rtpsize" } else { "aead_xchacha20_poly1305_rtpsize" };
                let addr: SocketAddr = format!("{ip}:{port}").parse().map_err(|e| (1011u16, format!("addr: {e}")))?;
                let ssrc = self.ssrc;
                let (sock, my_ip, my_port) = tokio::task::spawn_blocking(move || -> std::io::Result<(UdpSocket, String, u16)> {
                    let s = udp::bind()?;
                    s.set_read_timeout(Some(Duration::from_secs(5)))?;
                    let (ip, port) = udp::discover(&s, addr, ssrc)?;
                    s.connect(addr)?;
                    s.set_nonblocking(true)?;
                    Ok((s, ip, port))
                }).await.map_err(|e| (1011u16, e.to_string()))?.map_err(|e| (1011u16, format!("udp: {e}")))?;
                tracing::info!("voice[{}] ready: ssrc={} mode={mode} external={my_ip}:{my_port}", self.p.guild_id, self.ssrc);
                self.sock = Some(sock);
                self.send_json(select_protocol(&my_ip, my_port, mode)).await?;
            }
            OP_SESSION_DESC => {
                let key: Vec<u8> = d["secret_key"].as_array()
                    .map(|a| a.iter().map(|x| x.as_u64().unwrap_or(0) as u8).collect()).unwrap_or_default();
                let key: [u8; 32] = key.try_into().map_err(|_| (1002u16, "bad secret_key".to_string()))?;
                let mode = d["mode"].as_str().unwrap_or("aead_xchacha20_poly1305_rtpsize").to_string();
                let transport = Transport::new(&mode, &key).ok_or_else(|| (1002u16, format!("unsupported mode {mode}")))?;
                let ver = d["dave_protocol_version"].as_u64().unwrap_or(0) as u16;

                tracing::info!("voice[{}] session description: mode={mode} dave_protocol_version={ver}", self.p.guild_id);
                self.send_json(speaking(self.ssrc)).await?;
                let kp = { let mut g = self.dave.lock().map_err(|_| (1011u16, "dave lock".to_string()))?; g.init(ver) };
                if let Some(kp) = kp.map_err(|e| (1011u16, e))? { self.send_bin(OP_MLS_KEY_PACKAGE, &kp).await?; }

                let sock = self.sock.take().ok_or_else(|| (1011u16, "udp not ready".to_string()))?;
                let seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(1);
                let rtp = Rtp { seq: (seed & 0xffff) as u16, ts: seed, ssrc: self.ssrc };
                let sender = VoiceSender::new(sock, transport, rtp, self.dave.clone(), self.slot.clone(),
                                              self.events.clone(), self.h.stats.clone(), self.h.closed.clone());
                self.shard.send(Box::new(sender)).map_err(|_| (1011u16, "scheduler down".to_string()))?;
                self.h.connected.store(true, Relaxed);
                let _ = self.events.send(VoiceEvent::Connected);
            }
            OP_DAVE_PREPARE_TRANSITION => {
                let id = d["transition_id"].as_u64().unwrap_or(0) as u16;
                let ver = d["protocol_version"].as_u64().unwrap_or(0) as u16;
                self.pending = Some((id, ver));
                if id == 0 { self.execute(0); } else { self.send_json(transition_ready(id)).await?; }
            }
            OP_DAVE_EXECUTE_TRANSITION => {
                let id = d["transition_id"].as_u64().unwrap_or(0) as u16;
                self.execute(id);
            }
            OP_DAVE_PREPARE_EPOCH => {
                let ver = d["protocol_version"].as_u64().unwrap_or(0) as u16;
                let epoch = d["epoch"].as_u64().unwrap_or(0);
                let id = d["transition_id"].as_u64().unwrap_or(0) as u16;
                if epoch == 1 && ver > 0 {
                    let kp = { let mut g = self.dave.lock().map_err(|_| (1011u16, "dave lock".to_string()))?; g.init(ver) };
                    if let Some(kp) = kp.map_err(|e| (1011u16, e))? { self.send_bin(OP_MLS_KEY_PACKAGE, &kp).await?; }
                } else if id != 0 {
                    self.pending = Some((id, ver));
                    self.send_json(transition_ready(id)).await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_binary(&mut self, b: &[u8]) -> Result<(), Fail> {
        if b.len() < 3 { return Ok(()); }
        self.seq = u16::from_be_bytes([b[0], b[1]]) as i64;
        let op = b[2];
        let pl = &b[3..];
        match op {
            OP_MLS_EXTERNAL_SENDER => {
                let r = { let mut d = self.dave.lock().map_err(|_| (1011u16, "dave lock".to_string()))?; d.set_external_sender(pl) };
                if let Err(e) = r { tracing::warn!("dave external sender: {e}"); }
            }
            OP_MLS_PROPOSALS => {
                if pl.is_empty() { return Ok(()); }
                let r = { let mut d = self.dave.lock().map_err(|_| (1011u16, "dave lock".to_string()))?; d.process_proposals(pl[0], &pl[1..]) };
                match r {
                    Ok(Some(cw)) => {
                        tracing::info!("voice[{}] dave: sending commit{}", self.p.guild_id, if cw.welcome.is_some() { "+welcome" } else { "" });
                        let mut out = cw.commit;
                        if let Some(w) = cw.welcome { out.extend_from_slice(&w); }
                        self.send_bin(OP_MLS_COMMIT_WELCOME, &out).await?;
                    }
                    Ok(None) => {}
                    Err(e) => { tracing::warn!("dave proposals: {e}"); self.recover(None).await?; }
                }
            }
            OP_MLS_ANNOUNCE_COMMIT | OP_MLS_WELCOME => {
                if pl.len() < 2 { return Ok(()); }
                let id = u16::from_be_bytes([pl[0], pl[1]]);
                let (r, ver) = {
                    let mut d = self.dave.lock().map_err(|_| (1011u16, "dave lock".to_string()))?;
                    let r = if op == OP_MLS_ANNOUNCE_COMMIT { d.process_commit(&pl[2..]) } else { d.process_welcome(&pl[2..]) };
                    (r, d.version)
                };
                match r {
                    Ok(()) => {
                        tracing::info!("voice[{}] dave: op{op} processed, transition {id} ready (v{ver})", self.p.guild_id);
                        self.pending = Some((id, ver));
                        self.send_json(transition_ready(id)).await?;
                    }
                    Err(e) => { tracing::warn!("dave commit/welcome: {e}"); self.recover(Some(id)).await?; }
                }
            }
            _ => {}
        }
        Ok(())
    }
}
