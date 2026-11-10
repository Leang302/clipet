//! Pairing invitations and verification codes (docs/SYNC.md §4).
//!
//! The credential is a 256-bit one-time secret delivered together with the
//! host's public key; the joiner dials that key, so the secret only ever goes
//! to the host. The 6-digit code is a human cross-check, not a credential.

use base64::Engine;
use iroh::{EndpointAddr, EndpointId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PREFIX: &str = "clipet-invite:";
pub const INVITE_TTL_MS: i64 = 10 * 60 * 1000;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Invite {
    pub v: u32,
    pub addr: EndpointAddr,
    pub name: String,
    /// Hex-encoded 32 random bytes.
    pub secret: String,
    /// Epoch ms after which the host refuses it.
    pub exp: i64,
}

pub fn new_secret() -> Result<[u8; 32], String> {
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).map_err(|e| format!("no secure random source: {e}"))?;
    Ok(secret)
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

pub fn encode(invite: &Invite) -> String {
    let json = serde_json::to_vec(invite).unwrap_or_default();
    format!("{PREFIX}{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json))
}

/// Accepts the invitation as pasted: surrounding whitespace, line breaks
/// added by chat apps, and a missing prefix are all fine.
pub fn decode(text: &str) -> Result<Invite, String> {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let body = compact.strip_prefix(PREFIX).unwrap_or(&compact);
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body)
        .map_err(|_| "That isn't a Clipet invitation.".to_string())?;
    let invite: Invite =
        serde_json::from_slice(&json).map_err(|_| "That isn't a Clipet invitation.".to_string())?;
    if invite.v != 1 {
        return Err("This invitation is from a newer version of Clipet.".to_string());
    }
    if from_hex32(&invite.secret).is_none() {
        return Err("This invitation is damaged.".to_string());
    }
    Ok(invite)
}

/// Six digits both screens show, derived from both keys and the secret.
pub fn verification_code(host: &EndpointId, joiner: &EndpointId, secret: &[u8; 32]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"clipet-pair-v1");
    hasher.update(host.as_bytes());
    hasher.update(joiner.as_bytes());
    hasher.update(secret);
    let digest = hasher.finalize();
    let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

/// Compares secrets without leaking how many leading bytes matched.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    fn invite() -> Invite {
        let id = SecretKey::generate().public();
        Invite { v: 1, addr: EndpointAddr::new(id), name: "Mac".into(), secret: to_hex(&new_secret().unwrap()), exp: 5 }
    }

    #[test]
    fn invites_round_trip_even_when_wrapped_by_a_chat_app() {
        let original = invite();
        let text = encode(&original);
        assert!(text.starts_with(PREFIX));
        let wrapped = format!("  {}\n{}  ", &text[..30], &text[30..]);
        assert_eq!(decode(&wrapped).unwrap(), original);
        assert_eq!(decode(&text[PREFIX.len()..]).unwrap(), original);
    }

    #[test]
    fn garbage_and_damaged_invites_are_rejected() {
        assert!(decode("hello").is_err());
        assert!(decode("clipet-invite:!!!").is_err());
        let mut damaged = invite();
        damaged.secret = "abcd".into();
        assert!(decode(&encode(&damaged)).is_err());
        let mut future = invite();
        future.v = 2;
        assert!(decode(&encode(&future)).is_err());
    }

    #[test]
    fn verification_codes_depend_on_both_keys_and_the_secret() {
        let (h, j, x) = (SecretKey::generate().public(), SecretKey::generate().public(), SecretKey::generate().public());
        let s = new_secret().unwrap();
        let code = verification_code(&h, &j, &s);
        assert_eq!(code.len(), 7);
        assert_eq!(code, verification_code(&h, &j, &s));
        assert_ne!(code, verification_code(&h, &x, &s));
        assert_ne!(code, verification_code(&h, &j, &new_secret().unwrap()));
    }

    #[test]
    fn secrets_are_random_and_hex_round_trips() {
        let a = new_secret().unwrap();
        assert_ne!(a, new_secret().unwrap());
        assert_eq!(from_hex32(&to_hex(&a)), Some(a));
        assert!(ct_eq(&a, &a));
        assert!(!ct_eq(&a, &new_secret().unwrap()));
        assert!(!ct_eq(&a, &a[..31]));
    }
}
