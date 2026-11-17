//! DAVE (E2EE) session using the pure-Rust `davey` crate (MLS via OpenMLS).
//! Call order mirrors the Discord docs / @snazzah/davey usage:
//!   op4 version>0 -> init() -> send key package (op26)
//!   op25 -> set_external_sender
//!   op27 -> process_proposals -> send commit(+welcome) (op28)
//!   op29 -> process_commit   -> send transition ready (op23)
//!   op30 -> process_welcome  -> send transition ready (op23)
//! Until the group is ready, frames go out unencrypted (passthrough) so audio never blocks on the handshake.
use davey::{DaveSession, ProposalsOperationType};
use std::num::NonZeroU16;

pub struct CommitWelcome { pub commit: Vec<u8>, pub welcome: Option<Vec<u8>> }

pub struct DaveCtx {
    session: Option<DaveSession>,
    user_id: u64,
    channel_id: u64,
    pub version: u16,
    pub passthrough: bool,
}

impl DaveCtx {
    pub fn new(user_id: u64, channel_id: u64) -> Self {
        Self { session: None, user_id, channel_id, version: 0, passthrough: true }
    }

    /// (Re)initialise for `version`. Returns the key package to send as op 26 (None when version == 0).
    pub fn init(&mut self, version: u16) -> Result<Option<Vec<u8>>, String> {
        self.version = version;
        if version == 0 {
            if let Some(s) = &mut self.session {
                s.reset().map_err(|e| e.to_string())?;
                s.set_passthrough_mode(true, Some(10));
            }
            self.passthrough = true;
            return Ok(None);
        }
        let v = NonZeroU16::new(version).ok_or("invalid DAVE version")?;
        if let Some(s) = &mut self.session {
            s.reinit(v, self.user_id, self.channel_id, None).map_err(|e| e.to_string())?;
        } else {
            self.session = Some(DaveSession::new(v, self.user_id, self.channel_id, None).map_err(|e| e.to_string())?);
        }
        self.passthrough = false;
        let kp = self.session.as_mut().unwrap().create_key_package().map_err(|e| e.to_string())?;
        Ok(Some(kp))
    }

    pub fn set_external_sender(&mut self, d: &[u8]) -> Result<(), String> {
        match &mut self.session { Some(s) => s.set_external_sender(d).map_err(|e| e.to_string()), None => Ok(()) }
    }

    pub fn process_proposals(&mut self, op_type: u8, proposals: &[u8]) -> Result<Option<CommitWelcome>, String> {
        let Some(s) = &mut self.session else { return Ok(None) };
        let op = if op_type == 1 { ProposalsOperationType::REVOKE } else { ProposalsOperationType::APPEND };
        let r = s.process_proposals(op, proposals, None).map_err(|e| e.to_string())?;
        Ok(r.map(|cw| CommitWelcome { commit: cw.commit.to_vec(), welcome: cw.welcome.map(|w| w.to_vec()) }))
    }

    pub fn process_commit(&mut self, d: &[u8]) -> Result<(), String> {
        match &mut self.session { Some(s) => s.process_commit(d).map_err(|e| e.to_string()), None => Ok(()) }
    }

    pub fn process_welcome(&mut self, d: &[u8]) -> Result<(), String> {
        match &mut self.session { Some(s) => s.process_welcome(d).map_err(|e| e.to_string()), None => Ok(()) }
    }

    pub fn is_ready(&self) -> bool { self.session.as_ref().map_or(false, |s| s.is_ready()) }

    /// hot path (every 20 ms per player). Plain copy when E2EE is not active/ready.
    pub fn encrypt(&mut self, frame: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
        out.clear();
        if !self.passthrough {
            if let Some(s) = &mut self.session {
                if s.is_ready() {
                    let enc = s.encrypt_opus(frame).map_err(|e| e.to_string())?;
                    out.extend_from_slice(&enc[..]);
                    return Ok(());
                }
            }
        }
        out.extend_from_slice(frame);
        Ok(())
    }
}
