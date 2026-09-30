//! Session-cookie signing, PKCE, and small auth helpers.
//!
//! Sessions are server-side rows in SQLite. The cookie carries only the
//! session ID plus an HMAC-SHA256 signature over it, so a database leak does
//! not mint cookies and a cookie leak reveals nothing but a random ID.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// Name of the session cookie.
pub const SESSION_COOKIE: &str = "chatstronomy_session";

/// Maximum age of an unconsumed OAuth state row, in seconds.
pub const OAUTH_STATE_MAX_AGE_SECONDS: i64 = 15 * 60;

/// Discord permission bits that grant hub management of a guild, derived
/// from serenity so the OAuth-snapshot check and the live REST check can
/// never disagree on the policy.
pub const PERMISSION_ADMINISTRATOR: i64 =
    poise::serenity_prelude::Permissions::ADMINISTRATOR.bits() as i64;
pub const PERMISSION_MANAGE_GUILD: i64 =
    poise::serenity_prelude::Permissions::MANAGE_GUILD.bits() as i64;

/// The same policy as a serenity mask, for the live guild check.
pub fn serenity_manage_mask() -> poise::serenity_prelude::Permissions {
    poise::serenity_prelude::Permissions::ADMINISTRATOR
        | poise::serenity_prelude::Permissions::MANAGE_GUILD
}

/// A fresh high-entropy secret: 64 hex chars from two UUIDv4s (~244 bits of
/// randomness). Used for PKCE verifiers, pairing tokens, and rig
/// credentials.
pub fn random_secret() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// SHA-256 of the input, base64url without padding. The one digest used for
/// PKCE challenges and for hashing stored tokens/credentials — changing it
/// invalidates every stored hash, so don't.
pub fn sha256_b64url(input: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(input.as_bytes()))
}

fn sign(signing_key: &str, session_id: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(signing_key.as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(session_id.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

/// Build the cookie value `<session_id>.<signature>`.
pub fn signed_cookie_value(signing_key: &str, session_id: &str) -> String {
    format!("{session_id}.{}", sign(signing_key, session_id))
}

/// Verify a cookie value and return the session ID it carries.
pub fn verify_signed_cookie_value(signing_key: &str, value: &str) -> Option<String> {
    let (session_id, signature) = value.split_once('.')?;
    let expected = sign(signing_key, session_id);
    if expected.as_bytes().ct_eq(signature.as_bytes()).into() {
        Some(session_id.to_string())
    } else {
        None
    }
}

/// Pull a named cookie out of a Cookie header value.
pub fn cookie_from_header(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|pair| {
        let (k, v) = pair.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

/// A fresh high-entropy PKCE code verifier (RFC 7636 minimum comfortably
/// exceeded).
pub fn pkce_verifier() -> String {
    random_secret()
}

/// The S256 code challenge for a verifier.
pub fn pkce_challenge(verifier: &str) -> String {
    sha256_b64url(verifier)
}

/// Clamp a post-login redirect target to a safe local path. Anything odd
/// falls back to "/".
pub fn sanitize_next_path(next: &str) -> String {
    let ok = next.starts_with('/')
        && !next.starts_with("//")
        && !next.contains('\\')
        && next.len() <= 256
        && next
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/-_.~?=&%".contains(c));
    if ok {
        next.to_string()
    } else {
        "/".to_string()
    }
}

/// True when a user with these guild permission bits may manage the guild's
/// hub configuration.
pub fn can_manage_guild(permissions: i64, is_owner: bool) -> bool {
    is_owner
        || permissions & PERMISSION_ADMINISTRATOR != 0
        || permissions & PERMISSION_MANAGE_GUILD != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn cookie_roundtrip() {
        let value = signed_cookie_value(KEY, "session-123");
        assert_eq!(
            verify_signed_cookie_value(KEY, &value).as_deref(),
            Some("session-123")
        );
    }

    #[test]
    fn tampered_cookie_rejected() {
        let value = signed_cookie_value(KEY, "session-123");
        let tampered = value.replace("session-123", "session-456");
        assert_eq!(verify_signed_cookie_value(KEY, &tampered), None);
    }

    #[test]
    fn wrong_key_rejected() {
        let value = signed_cookie_value(KEY, "session-123");
        assert_eq!(
            verify_signed_cookie_value("another-key-entirely", &value),
            None
        );
    }

    #[test]
    fn malformed_cookie_rejected() {
        assert_eq!(verify_signed_cookie_value(KEY, "no-dot-here"), None);
        assert_eq!(verify_signed_cookie_value(KEY, ""), None);
    }

    #[test]
    fn cookie_header_parsing() {
        let header = "a=1; chatstronomy_session=abc.def; b=2";
        assert_eq!(
            cookie_from_header(header, SESSION_COOKIE).as_deref(),
            Some("abc.def")
        );
        assert_eq!(cookie_from_header(header, "missing"), None);
    }

    #[test]
    fn pkce_challenge_matches_rfc_example() {
        // RFC 7636 appendix B test vector.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn pkce_verifier_length_and_uniqueness() {
        let a = pkce_verifier();
        let b = pkce_verifier();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }

    #[test]
    fn next_path_sanitizing() {
        assert_eq!(sanitize_next_path("/guilds/123"), "/guilds/123");
        assert_eq!(sanitize_next_path("/a?b=c&d=e"), "/a?b=c&d=e");
        assert_eq!(sanitize_next_path("//evil.com"), "/");
        assert_eq!(sanitize_next_path("https://evil.com"), "/");
        assert_eq!(sanitize_next_path(""), "/");
        assert_eq!(sanitize_next_path("/a\\b"), "/");
        assert_eq!(sanitize_next_path(&format!("/{}", "x".repeat(300))), "/");
    }

    #[test]
    fn guild_management_permissions() {
        assert!(can_manage_guild(PERMISSION_MANAGE_GUILD, false));
        assert!(can_manage_guild(PERMISSION_ADMINISTRATOR, false));
        assert!(can_manage_guild(0, true));
        // SEND_MESSAGES alone is not management.
        assert!(!can_manage_guild(1 << 11, false));
        assert!(!can_manage_guild(0, false));
    }
}
