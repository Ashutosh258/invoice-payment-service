use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const SECRET_PREFIX: &str = "whsec_";
pub const TIMESTAMP_TOLERANCE_SECS: i64 = 5 * 60;

pub fn generate_secret() -> String {
    let mut key = [0u8; 32];
    rand::rng().fill_bytes(&mut key);
    format!("{SECRET_PREFIX}{}", BASE64.encode(key))
}

pub fn sign(secret: &str, msg_id: &str, timestamp: i64, body: &[u8]) -> anyhow::Result<String> {
    let mac = mac_for(secret, msg_id, timestamp, body)?;
    Ok(format!("v1,{}", BASE64.encode(mac.finalize().into_bytes())))
}

pub fn verify(
    secret: &str,
    msg_id: &str,
    timestamp: i64,
    body: &[u8],
    signature_header: &str,
    now: i64,
) -> bool {
    if (now - timestamp).abs() > TIMESTAMP_TOLERANCE_SECS {
        return false;
    }

    signature_header
        .split(' ')
        .filter_map(|candidate| candidate.strip_prefix("v1,"))
        .filter_map(|encoded| BASE64.decode(encoded).ok())
        .any(|expected| {
            mac_for(secret, msg_id, timestamp, body)
                .map(|mac| mac.verify_slice(&expected).is_ok())
                .unwrap_or(false)
        })
}

fn mac_for(secret: &str, msg_id: &str, timestamp: i64, body: &[u8]) -> anyhow::Result<HmacSha256> {
    let key = BASE64.decode(secret.strip_prefix(SECRET_PREFIX).unwrap_or(secret))?;
    let mut mac = HmacSha256::new_from_slice(&key)?;
    mac.update(msg_id.as_bytes());
    mac.update(b".");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    Ok(mac)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
    const MSG_ID: &str = "msg_p5jXN8AQM9LWM0D4loKWxJek";
    const TIMESTAMP: i64 = 1_614_265_330;
    const BODY: &[u8] = br#"{"test": 2432232314}"#;
    const EXPECTED: &str = "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=";

    #[test]
    fn matches_the_standard_webhooks_test_vector() {
        assert_eq!(sign(SECRET, MSG_ID, TIMESTAMP, BODY).unwrap(), EXPECTED);
    }

    #[test]
    fn verifies_its_own_signatures() {
        let secret = generate_secret();
        let signature = sign(&secret, "evt_1", TIMESTAMP, BODY).unwrap();
        assert!(verify(
            &secret,
            "evt_1",
            TIMESTAMP,
            BODY,
            &signature,
            TIMESTAMP + 10
        ));
    }

    #[test]
    fn rejects_tampering_and_stale_timestamps() {
        let signature = sign(SECRET, MSG_ID, TIMESTAMP, BODY).unwrap();

        assert!(!verify(
            SECRET,
            MSG_ID,
            TIMESTAMP,
            br#"{"test": 1}"#,
            &signature,
            TIMESTAMP
        ));
        assert!(!verify(
            SECRET,
            "msg_other",
            TIMESTAMP,
            BODY,
            &signature,
            TIMESTAMP
        ));
        assert!(!verify(
            &generate_secret(),
            MSG_ID,
            TIMESTAMP,
            BODY,
            &signature,
            TIMESTAMP
        ));
        assert!(!verify(
            SECRET,
            MSG_ID,
            TIMESTAMP,
            BODY,
            &signature,
            TIMESTAMP + TIMESTAMP_TOLERANCE_SECS + 1
        ));
    }

    #[test]
    fn accepts_any_matching_signature_during_rotation() {
        let old = sign(&generate_secret(), MSG_ID, TIMESTAMP, BODY).unwrap();
        let current = sign(SECRET, MSG_ID, TIMESTAMP, BODY).unwrap();
        assert!(verify(
            SECRET,
            MSG_ID,
            TIMESTAMP,
            BODY,
            &format!("{old} {current}"),
            TIMESTAMP
        ));
    }
}
