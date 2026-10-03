// Webhook signing, as in DESIGN.md section 4.
//
//   Dodo-Signature: t=<unix seconds>,v1=<hex>
//   v1 = HMAC-SHA256(secret, "<t>.<raw body>")
//
// The timestamp is part of what is signed, so a captured request cannot be replayed later or
// given a new timestamp. Receivers recompute the value, compare it in constant time, and
// reject a timestamp more than 300 seconds away from their clock.
use hmac::{Hmac, Mac}; // `Mac` is a trait: importing it is what makes `.update()` and `.finalize()` usable
use sha2::Sha256;
use subtle::ConstantTimeEq;

#[cfg_attr(not(test), allow(dead_code))] // receiver-side; the service itself only signs
pub const TOLERANCE_SECS: i64 = 300;

type HmacSha256 = Hmac<Sha256>;

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The hex HMAC for one (timestamp, body) pair.
pub fn sign(secret: &str, timestamp: i64, body: &str) -> String {
    // HMAC accepts keys of any length, so new_from_slice cannot fail here.
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(format!("{timestamp}.{body}").as_bytes());
    to_hex(&mac.finalize().into_bytes())
}

/// The full header value, ready to send.
pub fn header_value(secret: &str, timestamp: i64, body: &str) -> String {
    format!("t={timestamp},v1={}", sign(secret, timestamp, body))
}

#[cfg_attr(not(test), allow(dead_code))] // receiver-side; the service itself only signs
/// What a receiver does. `now` is the receiver's current unix time (a parameter so tests can fix it).
pub fn verify(secret: &str, header: &str, body: &str, now: i64) -> bool {
    let mut timestamp: Option<i64> = None;
    let mut signature: Option<&str> = None;
    for part in header.split(',') {
        if let Some(value) = part.trim().strip_prefix("t=") {
            timestamp = value.parse().ok();
        } else if let Some(value) = part.trim().strip_prefix("v1=") {
            signature = Some(value);
        }
    }
    let (Some(timestamp), Some(signature)) = (timestamp, signature) else {
        return false;
    };
    if (now - timestamp).abs() > TOLERANCE_SECS {
        return false;
    }
    let expected = sign(secret, timestamp, body);
    // Constant-time: the comparison time does not reveal how many leading characters matched.
    expected.as_bytes().ct_eq(signature.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Vector computed independently with Python's hmac module.
    const SECRET: &str = "whsec_testsecret";
    const BODY: &str = r#"{"id":"evt_1","type":"invoice.created"}"#;
    const T: i64 = 1_700_000_000;
    const EXPECTED: &str = "2dd2e93d9ab619f8a0945d9dace2b98c750088b125e3669630c65b40feb37615";

    #[test]
    fn matches_the_fixed_vector() {
        assert_eq!(sign(SECRET, T, BODY), EXPECTED);
        assert_eq!(
            header_value(SECRET, T, BODY),
            format!("t={T},v1={EXPECTED}")
        );
    }

    #[test]
    fn verify_accepts_a_good_signature() {
        let header = header_value(SECRET, T, BODY);
        assert!(verify(SECRET, &header, BODY, T));
        assert!(verify(SECRET, &header, BODY, T + 300)); // exactly at the limit
        assert!(verify(SECRET, &header, BODY, T - 300));
    }

    #[test]
    fn verify_rejects_old_or_future_timestamps() {
        let header = header_value(SECRET, T, BODY);
        assert!(!verify(SECRET, &header, BODY, T + 301));
        assert!(!verify(SECRET, &header, BODY, T - 301));
    }

    #[test]
    fn verify_rejects_tampering() {
        let header = header_value(SECRET, T, BODY);
        assert!(!verify(SECRET, &header, r#"{"id":"evt_2"}"#, T)); // body changed
        assert!(!verify("whsec_other", &header, BODY, T)); // wrong secret
        // Re-stamping a captured signature with a fresh time must fail: the time is signed.
        let restamped = format!("t={},v1={EXPECTED}", T + 100);
        assert!(!verify(SECRET, &restamped, BODY, T + 100));
    }

    #[test]
    fn verify_rejects_malformed_headers() {
        for bad in [
            "",
            "t=1700000000",
            "v1=abc",
            "t=abc,v1=def",
            "garbage",
            &format!("t={T},v1="),
        ] {
            assert!(!verify(SECRET, bad, BODY, T), "accepted {bad:?}");
        }
    }
}
