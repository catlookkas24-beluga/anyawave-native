//! Lavalink password check.
//!
//! The server never stores the plaintext password. It stores only
//!     AW_PASSWORD_DIGEST = HMAC-SHA256(key = AW_PEPPER, msg = password)   (64 hex chars)
//! The pepper is a separate secret, so a leaked digest alone is not enough to test guesses offline.
//! Lavalink clients still send the password in plaintext over the wire: run the server over HTTPS/WSS.
//!
//! Setup:
//!   AW_PEPPER=<random, >= 16 chars>  printf '%s' '<password>' | AW_PEPPER=... aw-server --make-digest
//!   -> put the printed hex in AW_PASSWORD_DIGEST, put the same password in your Lavalink client.
//!
//! The server refuses to start without both variables. There is no default password.
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

const MIN_PEPPER: usize = 16;
const MIN_PASSWORD: usize = 20;

pub struct Auth {
    pepper: Vec<u8>,
    digest: [u8; 32],
}

fn digest(pepper: &[u8], password: &[u8]) -> [u8; 32] {
    // HMAC accepts keys of any length, so new_from_slice cannot fail here.
    let mut m = HmacSha256::new_from_slice(pepper).expect("hmac accepts any key length");
    m.update(password);
    m.finalize().into_bytes().into()
}

fn decode_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn encode_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn read_pepper() -> Result<Vec<u8>, String> {
    let p = std::env::var("AW_PEPPER").map_err(|_| "AW_PEPPER is not set".to_string())?;
    if p.len() < MIN_PEPPER {
        return Err(format!("AW_PEPPER must be at least {MIN_PEPPER} characters"));
    }
    Ok(p.into_bytes())
}

impl Auth {
    /// Load from AW_PEPPER and AW_PASSWORD_DIGEST. Err => the server must not start.
    pub fn from_env() -> Result<Self, String> {
        let pepper = read_pepper()?;
        let hex = std::env::var("AW_PASSWORD_DIGEST").map_err(|_| {
            "AW_PASSWORD_DIGEST is not set (create it with: aw-server --make-digest)".to_string()
        })?;
        let digest = decode_hex32(hex.trim()).ok_or("AW_PASSWORD_DIGEST must be 64 hex characters")?;
        Ok(Self { pepper, digest })
    }

    /// Constant-time check of a presented password.
    pub fn check(&self, presented: &str) -> bool {
        let d = digest(&self.pepper, presented.as_bytes());
        d.as_slice().ct_eq(self.digest.as_slice()).into()
    }
}

/// `aw-server --make-digest`: reads the password from stdin, prints the digest to put in AW_PASSWORD_DIGEST.
pub fn make_digest_from_stdin() -> Result<String, String> {
    let pepper = read_pepper()?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("could not read password from stdin: {e}"))?;
    let pw = line.trim_end_matches(['\r', '\n']);
    if pw.len() < MIN_PASSWORD {
        return Err(format!("password must be at least {MIN_PASSWORD} characters (use a random one)"));
    }
    Ok(encode_hex(&digest(&pepper, pw.as_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(pepper: &str, pw: &str) -> Auth {
        Auth { pepper: pepper.as_bytes().to_vec(), digest: digest(pepper.as_bytes(), pw.as_bytes()) }
    }

    #[test]
    fn accepts_right_password_only() {
        let a = make("0123456789abcdef", "correct horse battery staple!");
        assert!(a.check("correct horse battery staple!"));
        assert!(!a.check("correct horse battery staple?"));
        assert!(!a.check(""));
    }

    #[test]
    fn different_pepper_rejects() {
        let a = make("pepper-one-0123456", "correct horse battery staple!");
        let b = Auth { pepper: b"pepper-two-0123456".to_vec(), digest: a.digest };
        assert!(!b.check("correct horse battery staple!"));
    }

    #[test]
    fn hex_roundtrip() {
        let d = digest(b"0123456789abcdef", b"x");
        assert_eq!(decode_hex32(&encode_hex(&d)), Some(d));
        assert_eq!(decode_hex32("zz"), None);
    }
}
