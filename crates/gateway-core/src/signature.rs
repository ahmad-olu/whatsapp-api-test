//! `X-Hub-Signature-256` verification.
//!
//! Meta signs the *exact raw request body* with HMAC-SHA256 using your app
//! secret. Always verify against the raw bytes: parsing the JSON and
//! re-serializing it changes whitespace/ordering and breaks the signature.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SignatureError {
    #[error("missing X-Hub-Signature-256 header")]
    Missing,
    #[error("malformed X-Hub-Signature-256 header")]
    Malformed,
    #[error("signature mismatch")]
    Mismatch,
}

/// Produce a header value (`sha256=<hex>`) for `body`.
pub fn sign(app_secret: &str, body: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(app_secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Verify a header value against the raw body. Comparison is constant-time.
pub fn verify(app_secret: &str, body: &[u8], header: Option<&str>) -> Result<(), SignatureError> {
    let header = header.ok_or(SignatureError::Missing)?;
    let hex_part = header
        .trim()
        .strip_prefix("sha256=")
        .ok_or(SignatureError::Malformed)?;
    let expected = hex::decode(hex_part).map_err(|_| SignatureError::Malformed)?;

    let mut mac =
        HmacSha256::new_from_slice(app_secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    mac.verify_slice(&expected)
        .map_err(|_| SignatureError::Mismatch)
}

/// Constant-time string equality (for verify tokens, admin tokens).
pub fn ct_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "app-secret";
    const BODY: &[u8] = br#"{"object":"whatsapp_business_account"}"#;

    #[test]
    fn roundtrip_verifies() {
        let header = sign(SECRET, BODY);
        assert!(header.starts_with("sha256="));
        assert_eq!(verify(SECRET, BODY, Some(&header)), Ok(()));
    }

    #[test]
    fn rejects_missing_header() {
        assert_eq!(verify(SECRET, BODY, None), Err(SignatureError::Missing));
    }

    #[test]
    fn rejects_malformed_header() {
        assert_eq!(
            verify(SECRET, BODY, Some("md5=abc")),
            Err(SignatureError::Malformed)
        );
        assert_eq!(
            verify(SECRET, BODY, Some("sha256=not-hex")),
            Err(SignatureError::Malformed)
        );
    }

    #[test]
    fn rejects_wrong_secret_and_tampered_body() {
        let header = sign("other-secret", BODY);
        assert_eq!(
            verify(SECRET, BODY, Some(&header)),
            Err(SignatureError::Mismatch)
        );

        let header = sign(SECRET, BODY);
        let tampered = br#"{"object":"whatsapp_business_account" }"#;
        assert_eq!(
            verify(SECRET, tampered, Some(&header)),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn ct_eq_works() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "abcd"));
    }
}
