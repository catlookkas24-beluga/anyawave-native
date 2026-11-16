//! Discord Voice Gateway v8 client with DAVE (E2EE) handshake.
//! Text frames = JSON {op,d,seq?}. Binary frames: server->client [seq u16 BE][op u8][payload],
//! client->server [op u8][payload].  VERIFY layout against docs.discord.com (voice gateway).
use serde_json::{json, Value};

pub const OP_IDENTIFY: u8 = 0;   pub const OP_SELECT_PROTOCOL: u8 = 1;
pub const OP_READY: u8 = 2;      pub const OP_HEARTBEAT: u8 = 3;
pub const OP_SESSION_DESC: u8 = 4; pub const OP_SPEAKING: u8 = 5;
pub const OP_HEARTBEAT_ACK: u8 = 6; pub const OP_RESUME: u8 = 7; pub const OP_HELLO: u8 = 8;
// DAVE
pub const OP_DAVE_PREPARE_TRANSITION: u8 = 21;
pub const OP_DAVE_EXECUTE_TRANSITION: u8 = 22;
pub const OP_DAVE_TRANSITION_READY: u8 = 23;
pub const OP_DAVE_PREPARE_EPOCH: u8 = 24;
pub const OP_MLS_EXTERNAL_SENDER: u8 = 25;
pub const OP_MLS_KEY_PACKAGE: u8 = 26;
pub const OP_MLS_PROPOSALS: u8 = 27;
pub const OP_MLS_COMMIT_WELCOME: u8 = 28;
pub const OP_MLS_ANNOUNCE_COMMIT: u8 = 29;
pub const OP_MLS_WELCOME: u8 = 30;
pub const OP_MLS_INVALID_COMMIT: u8 = 31;

pub const MAX_DAVE_VERSION: u16 = 1;

pub fn identify(server_id: u64, user_id: u64, session_id: &str, token: &str) -> Value {
    json!({"op":0,"d":{
        "server_id": server_id.to_string(), "user_id": user_id.to_string(),
        "session_id": session_id, "token": token,
        "max_dave_protocol_version": MAX_DAVE_VERSION }})
}

pub fn select_protocol(ip: &str, port: u16, mode: &str) -> Value {
    json!({"op":1,"d":{"protocol":"udp","data":{"address":ip,"port":port,"mode":mode}}})
}

/// Encrypted-media state machine. Voice may keep sending *passthrough* until the first epoch is ready;
/// never block the audio loop on the handshake.
pub struct DaveState { pub pending_transition: Option<(u16, u16)> /* (id, version) */ }

pub enum Out { Json(Value), Binary(Vec<u8>) }

impl DaveState {
    pub fn on_json(&mut self, op: u8, d: &Value, dave: &mut crate::dave::Dave) -> Vec<Out> {
        match op {
            OP_DAVE_PREPARE_TRANSITION => {
                let id = d["transition_id"].as_u64().unwrap_or(0) as u16;
                let ver = d["protocol_version"].as_u64().unwrap_or(0) as u16;
                if id == 0 { dave.execute_transition(0); vec![] }       // initial / immediate
                else { self.pending_transition = Some((id, ver));
                       vec![Out::Json(json!({"op":23,"d":{"transition_id":id}}))] }
            }
            OP_DAVE_EXECUTE_TRANSITION => {
                let id = d["transition_id"].as_u64().unwrap_or(0) as u16;
                if let Some((pid, ver)) = self.pending_transition.take() {
                    if pid == id { dave.execute_transition(id); dave.version = ver; }
                } vec![]
            }
            OP_DAVE_PREPARE_EPOCH => {
                let ver = d["protocol_version"].as_u64().unwrap_or(0) as u16;
                if d["epoch"].as_u64() == Some(1) { dave.init(ver); return vec![Out::Binary(key_package_msg(dave))]; }
                vec![]
            }
            _ => vec![],
        }
    }

    pub fn on_binary(&mut self, op: u8, payload: &[u8], dave: &mut crate::dave::Dave) -> Vec<Out> {
        match op {
            OP_MLS_EXTERNAL_SENDER => { dave.set_external_sender(payload); vec![Out::Binary(key_package_msg(dave))] }
            OP_MLS_PROPOSALS => { /* dave.process_proposals -> optional [28][commit][welcome] */ vec![] }
            OP_MLS_ANNOUNCE_COMMIT => {
                let id = u16::from_be_bytes([payload[0], payload[1]]);
                if dave.process_commit(&payload[2..]) {
                    self.pending_transition = Some((id, dave.version));
                    vec![Out::Json(json!({"op":23,"d":{"transition_id":id}}))]
                } else { vec![Out::Json(json!({"op":31,"d":{"transition_id":id}})), Out::Binary(key_package_msg(dave))] }
            }
            OP_MLS_WELCOME => { /* [transition_id u16][welcome] -> process_welcome -> op23 */ vec![] }
            _ => vec![],
        }
    }
}

fn key_package_msg(dave: &mut crate::dave::Dave) -> Vec<u8> {
    let mut v = vec![OP_MLS_KEY_PACKAGE]; v.extend(dave.key_package()); v
}
