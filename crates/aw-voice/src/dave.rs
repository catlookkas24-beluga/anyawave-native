//! DAVE (E2EE) session using the pure-Rust `davey` crate (MLS via OpenMLS), davey >= 0.2.
//!
//! Gateway flow (matches the Discord DAVE whitepaper / libdave sample):
//!   op4  version>0            -> init()                     -> send key package (op26)
//!   op24 epoch==1             -> init()                     -> send key package (op26)
//!   op25 external sender      -> set_external_sender
//!   op27 proposals            -> process_proposals          -> send commit(+welcome) (op28)
//!   op29 announce commit      -> process_commit  -> prepare_transition -> ready (op23)
//!   op30 welcome              -> process_welcome -> prepare_transition -> ready (op23)
//!   op21 prepare transition   -> prepare_transition         -> ready (op23)   (id 0 = execute now)
//!   op22 execute transition   -> execute_transition         -> keys switch / passthrough toggles
//!   op11/op13 clients connect/disconnect -> track recognized user ids
//!
//! IMPORTANT (davey 0.2): process_commit / process_welcome no longer swap keys. The encrypt key is only
//! switched by `execute_transition` (or immediately for transition id 0). Until then we keep sending with
//! the old key, or plaintext if we never had one, so audio never blocks on the handshake.
use davey::{DaveSession, ProposalsOperationType};
use std::collections::BTreeSet;
use std::num::NonZeroU16;

pub struct CommitWelcome { pub commit: Vec<u8>, pub welcome: Option<Vec<u8>> }

pub struct DaveCtx {
    session: Option<DaveSession>,
    user_id: u64,
    channel_id: u64,
    /// MLS protocol version of the current/target session (0 = no E2EE).
    pub version: u16,
    /// true => frames go out as plain Opus (no E2EE active yet, or downgraded to v0).
    pub passthrough: bool,
    /// (transition_id, protocol_version) prepared but not yet executed.
    pending: Option<(u16, u16)>,
    /// user ids learnt from op11/op13 (does not include ourselves).
    known: BTreeSet<u64>,
    /// pass the recognized user ids to davey for validation. Off by default: if the id list were ever
    /// incomplete, commits/welcomes would be rejected and E2EE would never come up. Enable with AW_DAVE_STRICT=1.
    strict: bool,
}

impl DaveCtx {
    pub fn new(user_id: u64, channel_id: u64) -> Self {
        let strict = std::env::var("AW_DAVE_STRICT").map(|v| v == "1").unwrap_or(false);
        Self { session: None, user_id, channel_id, version: 0, passthrough: true, pending: None, known: BTreeSet::new(), strict }
    }

    // ---- recognized users (op 11 / op 13) -------------------------------------------------------
    pub fn add_users(&mut self, ids: &[u64]) {
        for id in ids { if *id != self.user_id { self.known.insert(*id); } }
    }
    pub fn remove_user(&mut self, id: u64) { self.known.remove(&id); }

    fn recognized(&self) -> Option<Vec<u64>> {
        if !self.strict { return None; }
        let mut v: Vec<u64> = self.known.iter().copied().collect();
        v.push(self.user_id);
        Some(v)
    }

    // ---- session lifecycle ----------------------------------------------------------------------
    /// (Re)initialise for `version`. Returns the key package to send as op 26 (None when version == 0).
    /// With version > 0 the existing encrypt key (if any) is kept until the next transition executes.
    pub fn init(&mut self, version: u16) -> Result<Option<Vec<u8>>, String> {
        self.version = version;
        self.pending = None;
        if version == 0 {
            if let Some(s) = &mut self.session {
                let _ = s.reset();
                s.set_passthrough_mode(true, None);
            }
            self.passthrough = true;
            return Ok(None);
        }
        let v = NonZeroU16::new(version).ok_or("invalid DAVE version")?;
        match &mut self.session {
            Some(s) => s.reinit(v, self.user_id, self.channel_id, None).map_err(|e| e.to_string())?,
            None => {
                self.session = Some(DaveSession::new(v, self.user_id, self.channel_id, None).map_err(|e| e.to_string())?);
            }
        }
        // `passthrough` is deliberately NOT touched here: it flips to false only when a transition with
        // protocol_version > 0 executes (a stale key from before a v0 downgrade must never be reused).
        let kp = self.session.as_mut().unwrap().create_key_package().map_err(|e| e.to_string())?;
        Ok(Some(kp))
    }

    pub fn set_external_sender(&mut self, d: &[u8]) -> Result<(), String> {
        match &mut self.session { Some(s) => s.set_external_sender(d).map_err(|e| e.to_string()), None => Ok(()) }
    }

    pub fn process_proposals(&mut self, op_type: u8, proposals: &[u8]) -> Result<Option<CommitWelcome>, String> {
        let ids = self.recognized();
        let Some(s) = &mut self.session else { return Ok(None) };
        let op = if op_type == 1 { ProposalsOperationType::REVOKE } else { ProposalsOperationType::APPEND };
        let r = s.process_proposals(op, proposals, ids.as_deref()).map_err(|e| e.to_string())?;
        Ok(r.map(|cw| CommitWelcome { commit: cw.commit.to_vec(), welcome: cw.welcome.map(|w| w.to_vec()) }))
    }

    /// Ok(true)  => commit applied, caller must now call `prepare_transition`.
    /// Ok(false) => commit ignored by davey (no group / nothing queued): do NOT prepare a transition.
    pub fn process_commit(&mut self, d: &[u8]) -> Result<bool, String> {
        match &mut self.session { Some(s) => s.process_commit(d).map_err(|e| e.to_string()), None => Ok(false) }
    }

    /// Ok(()) => welcome applied, caller must now call `prepare_transition`.
    pub fn process_welcome(&mut self, d: &[u8]) -> Result<(), String> {
        let ids = self.recognized();
        match &mut self.session { Some(s) => s.process_welcome(d, ids.as_deref()).map_err(|e| e.to_string()), None => Ok(()) }
    }

    // ---- transitions ------------------------------------------------------------------------------
    pub fn prepare_transition(&mut self, id: u16, version: u16) -> Result<(), String> {
        if let Some(s) = &mut self.session { s.prepare_transition(id, version).map_err(|e| e.to_string())?; }
        self.pending = Some((id, version));
        Ok(())
    }

    /// Ok(true) if a prepared transition with this id was executed.
    pub fn execute_transition(&mut self, id: u16) -> Result<bool, String> {
        let Some((pid, ver)) = self.pending else { return Ok(false) };
        if pid != id { return Ok(false); }
        self.pending = None;
        let mut executed = true;
        if let Some(s) = &mut self.session {
            executed = s.execute_transition(id).map_err(|e| e.to_string())?;
            if ver == 0 {
                // downgrade: drop the MLS group, stay in passthrough
                let _ = s.reset();
                s.set_passthrough_mode(true, None);
            }
        }
        self.version = ver;
        self.passthrough = ver == 0;
        Ok(executed)
    }

    // ---- status / hot path ------------------------------------------------------------------------
    pub fn is_ready(&self) -> bool { self.session.as_ref().map_or(false, |s| s.is_ready()) }

    /// true while frames are actually being end-to-end encrypted.
    pub fn is_encrypting(&self) -> bool {
        !self.passthrough && self.session.as_ref().map_or(false, |s| s.can_encrypt())
    }

    /// 6-digit-group voice privacy code of the current epoch (for display), if any.
    pub fn privacy_code(&self) -> Option<String> {
        self.session.as_ref().and_then(|s| s.voice_privacy_code().map(|c| c.to_string()))
    }

    /// hot path (every 20 ms per player). Plain copy when E2EE is not active / has no key yet.
    /// davey leaves the standard Opus silence frame unencrypted on its own.
    pub fn encrypt(&mut self, frame: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
        out.clear();
        if !self.passthrough {
            if let Some(s) = &mut self.session {
                if s.can_encrypt() {
                    let enc = s.encrypt_opus(frame).map_err(|e| e.to_string())?;
                    out.extend_from_slice(&enc);
                    return Ok(());
                }
            }
        }
        out.extend_from_slice(frame);
        Ok(())
    }
}
