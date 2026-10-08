use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use rsa::BigUint;
use rsa::RsaPrivateKey;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

pub const ACCESS_TTL_SECS: i64 = 3600;
pub const REFRESH_TTL_SECS: i64 = 30 * 24 * 3600;
pub const CODE_TTL_SECS: i64 = 120;
pub const DEVICE_TTL_SECS: i64 = 600;
pub const DEVICE_INTERVAL_SECS: i32 = 5;
pub const KEY_ROTATE_AFTER_DAYS: i64 = 30;

#[derive(Debug, Error)]
pub enum JwtError {
    #[error("token is not a signed JWT")]
    Malformed,
    #[error("token signature was rejected")]
    Signature,
    #[error("token is expired")]
    Expired,
    #[error("token claims are incomplete")]
    Claims,
}

#[derive(Clone)]
pub struct PublicJwk {
    pub kid: String,
    pub n: String,
    pub e: String,
}

pub struct SigningKey {
    pub kid: String,
    pub private: RsaPrivateKey,
    pub created_at: DateTime<Utc>,
    pub retired_at: Option<DateTime<Utc>>,
}

pub struct KeySet {
    pub active: SigningKey,
    pub published: Vec<PublicJwk>,
}

pub fn generate_key() -> Result<SigningKey, JwtError> {
    let mut rng = rand_core::OsRng;
    let private = RsaPrivateKey::new(&mut rng, 2048).map_err(|_| JwtError::Signature)?;
    let kid = Uuid::new_v4().to_string();
    Ok(SigningKey {
        kid,
        private,
        created_at: Utc::now(),
        retired_at: None,
    })
}

pub fn private_der(key: &RsaPrivateKey) -> Result<Vec<u8>, JwtError> {
    let der = key.to_pkcs8_der().map_err(|_| JwtError::Signature)?;
    Ok(der.as_bytes().to_vec())
}

pub fn private_from_der(der: &[u8]) -> Result<RsaPrivateKey, JwtError> {
    RsaPrivateKey::from_pkcs8_der(der).map_err(|_| JwtError::Signature)
}

pub fn public_jwk(kid: &str, key: &RsaPrivateKey) -> PublicJwk {
    let public = key.to_public_key();
    PublicJwk {
        kid: kid.to_owned(),
        n: URL_SAFE_NO_PAD.encode(public.n().to_bytes_be()),
        e: URL_SAFE_NO_PAD.encode(public.e().to_bytes_be()),
    }
}

pub fn jwks_document(keys: &[PublicJwk]) -> Value {
    json!({
        "keys": keys.iter().map(|key| json!({
            "kty": "RSA",
            "use": "sig",
            "alg": "RS256",
            "kid": key.kid,
            "n": key.n,
            "e": key.e,
        })).collect::<Vec<_>>()
    })
}

pub fn sign(key: &RsaPrivateKey, kid: &str, claims: &Value) -> Result<String, JwtError> {
    let header = json!({"alg": "RS256", "kid": kid, "typ": "JWT"});
    let head = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).map_err(|_| JwtError::Claims)?);
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).map_err(|_| JwtError::Claims)?);
    let input = format!("{head}.{body}");
    let sig = rs256_sign(key, input.as_bytes())?;
    Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig)))
}

#[derive(Debug)]
pub struct AccessClaims {
    pub sub: Uuid,
    pub scope: String,
}

pub fn verify_access(token: &str, keys: &[PublicJwk]) -> Result<AccessClaims, JwtError> {
    let payload = verify_payload(token, keys)?;
    let sub = payload
        .get("sub")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or(JwtError::Claims)?;
    let scope = payload
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    Ok(AccessClaims { sub, scope })
}

pub fn verify_payload(token: &str, keys: &[PublicJwk]) -> Result<Value, JwtError> {
    let (input, header, sig, body) = split_token(token)?;
    let kid = header
        .get("kid")
        .and_then(|v| v.as_str())
        .ok_or(JwtError::Malformed)?;
    let jwk = keys
        .iter()
        .find(|k| k.kid == kid)
        .ok_or(JwtError::Signature)?;
    verify_with_components(&input, &sig, &jwk.n, &jwk.e)?;
    claims_from_body(&body)
}

pub fn verify_with_jwk(token: &str, n_b64: &str, e_b64: &str) -> Result<Value, JwtError> {
    let (input, _header, sig, body) = split_token(token)?;
    verify_with_components(&input, &sig, n_b64, e_b64)?;
    claims_from_body(&body)
}

fn split_token(token: &str) -> Result<(String, Value, Vec<u8>, Vec<u8>), JwtError> {
    let mut parts = token.split('.');
    let head_b64 = parts.next().ok_or(JwtError::Malformed)?;
    let body_b64 = parts.next().ok_or(JwtError::Malformed)?;
    let sig_b64 = parts.next().ok_or(JwtError::Malformed)?;
    if parts.next().is_some() || head_b64.is_empty() || body_b64.is_empty() || sig_b64.is_empty() {
        return Err(JwtError::Malformed);
    }
    let head_bytes = URL_SAFE_NO_PAD
        .decode(head_b64)
        .map_err(|_| JwtError::Malformed)?;
    let header: Value = serde_json::from_slice(&head_bytes).map_err(|_| JwtError::Malformed)?;
    if header.get("alg").and_then(|v| v.as_str()) != Some("RS256") {
        return Err(JwtError::Signature);
    }
    let sig = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| JwtError::Malformed)?;
    let body = URL_SAFE_NO_PAD
        .decode(body_b64)
        .map_err(|_| JwtError::Malformed)?;
    Ok((format!("{head_b64}.{body_b64}"), header, sig, body))
}

fn claims_from_body(body: &[u8]) -> Result<Value, JwtError> {
    let claims: Value = serde_json::from_slice(body).map_err(|_| JwtError::Malformed)?;
    let exp = claims
        .get("exp")
        .and_then(|v| v.as_i64())
        .ok_or(JwtError::Claims)?;
    if exp <= Utc::now().timestamp() {
        return Err(JwtError::Expired);
    }
    Ok(claims)
}

fn verify_with_components(
    input: &str,
    sig: &[u8],
    n_b64: &str,
    e_b64: &str,
) -> Result<(), JwtError> {
    let n = jwk_int(n_b64)?;
    let e = jwk_int(e_b64)?;
    rs256_verify(&n, &e, input.as_bytes(), sig)
}

fn jwk_int(b64: &str) -> Result<BigUint, JwtError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(b64)
        .map_err(|_| JwtError::Signature)?;
    if bytes.is_empty() {
        return Err(JwtError::Signature);
    }
    Ok(BigUint::from_bytes_be(&bytes))
}

const SHA256_DIGESTINFO_PREFIX: [u8; 19] = [
    0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05,
    0x00, 0x04, 0x20,
];

fn emsa_pkcs1_sha256(digest: &[u8], k: usize) -> Result<Vec<u8>, JwtError> {
    if digest.len() != 32 {
        return Err(JwtError::Signature);
    }
    let t_len = SHA256_DIGESTINFO_PREFIX.len() + digest.len();
    if k < t_len + 11 {
        return Err(JwtError::Signature);
    }
    let mut em = vec![0u8; k];
    em[1] = 0x01;
    let separator = k - t_len - 1;
    em[2..separator].fill(0xff);
    em[separator] = 0x00;
    let t_at = separator + 1;
    em[t_at..t_at + SHA256_DIGESTINFO_PREFIX.len()].copy_from_slice(&SHA256_DIGESTINFO_PREFIX);
    em[t_at + SHA256_DIGESTINFO_PREFIX.len()..].copy_from_slice(digest);
    Ok(em)
}

fn i2osp(value: &BigUint, k: usize) -> Result<Vec<u8>, JwtError> {
    let bytes = value.to_bytes_be();
    if bytes.len() > k {
        return Err(JwtError::Signature);
    }
    let mut out = vec![0u8; k - bytes.len()];
    out.extend_from_slice(&bytes);
    Ok(out)
}

fn modulus_len(n: &BigUint) -> usize {
    n.bits().div_ceil(8)
}

fn rs256_sign(key: &RsaPrivateKey, message: &[u8]) -> Result<Vec<u8>, JwtError> {
    let digest = Sha256::digest(message);
    let k = modulus_len(key.n());
    let em = emsa_pkcs1_sha256(&digest, k)?;
    let m = BigUint::from_bytes_be(&em);
    let s = m.modpow(key.d(), key.n());
    i2osp(&s, k)
}

fn rs256_verify(n: &BigUint, e: &BigUint, message: &[u8], sig: &[u8]) -> Result<(), JwtError> {
    let k = modulus_len(n);
    if sig.len() != k {
        return Err(JwtError::Signature);
    }
    let s = BigUint::from_bytes_be(sig);
    if &s >= n {
        return Err(JwtError::Signature);
    }
    let m = s.modpow(e, n);
    let em = i2osp(&m, k)?;
    let digest = Sha256::digest(message);
    let expected = emsa_pkcs1_sha256(&digest, k)?;
    if em != expected {
        return Err(JwtError::Signature);
    }
    Ok(())
}

pub fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::Rng::fill(&mut rand::rng(), &mut bytes);
    b64url(&bytes)
}

pub fn user_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::rng();
    let mut raw = String::with_capacity(9);
    for i in 0..8 {
        if i == 4 {
            raw.push('-');
        }
        let idx = rand::Rng::random_range(&mut rng, 0..ALPHABET.len());
        raw.push(ALPHABET[idx] as char);
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_access_token() {
        let key = generate_key().expect("key");
        let sub = Uuid::new_v4();
        let exp = Utc::now().timestamp() + 60;
        let token = sign(
            &key.private,
            &key.kid,
            &json!({"sub": sub.to_string(), "exp": exp, "scope": "openid"}),
        )
        .expect("sign");
        let jwk = public_jwk(&key.kid, &key.private);
        let claims = verify_access(&token, std::slice::from_ref(&jwk)).expect("verify");
        assert_eq!(claims.sub, sub);
        assert_eq!(claims.scope, "openid");
        let again = verify_with_jwk(&token, &jwk.n, &jwk.e).expect("jwk");
        assert_eq!(again["sub"], sub.to_string());
        let mut broken = token.into_bytes();
        let last = broken.len() - 1;
        broken[last] ^= 1;
        let broken = String::from_utf8(broken).expect("ascii");
        assert!(verify_with_jwk(&broken, &jwk.n, &jwk.e).is_err());
    }

    #[test]
    fn expired_token_is_rejected() {
        let key = generate_key().expect("key");
        let sub = Uuid::new_v4();
        let token = sign(
            &key.private,
            &key.kid,
            &json!({"sub": sub.to_string(), "exp": Utc::now().timestamp() - 30, "scope": "openid"}),
        )
        .expect("sign");
        let jwk = public_jwk(&key.kid, &key.private);
        assert!(verify_access(&token, &[jwk]).is_err());
    }
}
