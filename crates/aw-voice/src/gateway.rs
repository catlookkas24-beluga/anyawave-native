//! Voice gateway v8 opcodes + payload builders.
//! Binary frames: server->client = [seq u16 BE][op u8][payload]; client->server = [op u8][payload].
use serde_json::{json, Value};

pub const OP_IDENTIFY: u8 = 0;
pub const OP_SELECT_PROTOCOL: u8 = 1;
pub const OP_READY: u8 = 2;
pub const OP_HEARTBEAT: u8 = 3;
pub const OP_SESSION_DESC: u8 = 4;
pub const OP_SPEAKING: u8 = 5;
pub const OP_HEARTBEAT_ACK: u8 = 6;
pub const OP_HELLO: u8 = 8;
pub const OP_CLIENTS_CONNECT: u8 = 11;   // {"user_ids":["..."]}
pub const OP_CLIENT_DISCONNECT: u8 = 13; // {"user_id":"..."}
// DAVE (JSON)
pub const OP_DAVE_PREPARE_TRANSITION: u8 = 21;
pub const OP_DAVE_EXECUTE_TRANSITION: u8 = 22;
pub const OP_DAVE_TRANSITION_READY: u8 = 23;
pub const OP_DAVE_PREPARE_EPOCH: u8 = 24;
// DAVE (binary)
pub const OP_MLS_EXTERNAL_SENDER: u8 = 25;
pub const OP_MLS_KEY_PACKAGE: u8 = 26;
pub const OP_MLS_PROPOSALS: u8 = 27;
pub const OP_MLS_COMMIT_WELCOME: u8 = 28;
pub const OP_MLS_ANNOUNCE_COMMIT: u8 = 29;
pub const OP_MLS_WELCOME: u8 = 30;
pub const OP_MLS_INVALID_COMMIT: u8 = 31; // sent by the client as JSON

pub const MAX_DAVE_VERSION: u16 = 1;

pub fn identify(guild: u64, user: u64, session_id: &str, token: &str) -> Value {
    json!({"op":0,"d":{
        "server_id": guild.to_string(), "user_id": user.to_string(),
        "session_id": session_id, "token": token,
        "max_dave_protocol_version": MAX_DAVE_VERSION }})
}
pub fn select_protocol(ip: &str, port: u16, mode: &str) -> Value {
    json!({"op":1,"d":{"protocol":"udp","data":{"address":ip,"port":port,"mode":mode}}})
}
pub fn speaking(ssrc: u32) -> Value { json!({"op":5,"d":{"speaking":1,"delay":0,"ssrc":ssrc}}) }
pub fn transition_ready(id: u16) -> Value { json!({"op":23,"d":{"transition_id":id}}) }
pub fn invalid_commit(id: u16) -> Value { json!({"op":31,"d":{"transition_id":id}}) }
