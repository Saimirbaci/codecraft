use crate::{Error, Result, decode_key, invalid};
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// A fresh signing key (base64 of the 32-byte seed).
pub fn generate_secret() -> Result<String> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| invalid(format!("no randomness: {e}")))?;
    Ok(B64.encode(seed))
}

fn signing_key(secret: &str) -> Result<SigningKey> {
    let seed = decode_key(secret).ok_or_else(|| invalid("signing key is not 32 bytes of base64"))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// The public key (base64) for a signing key.
pub fn public_key(secret: &str) -> Result<String> {
    Ok(B64.encode(signing_key(secret)?.verifying_key().to_bytes()))
}

/// Detached signature (base64) over `body`.
pub fn sign(secret: &str, body: &[u8]) -> Result<String> {
    Ok(B64.encode(signing_key(secret)?.sign(body).to_bytes()))
}

pub(crate) fn verify(public: &[u8; 32], body: &[u8], sig: &[u8]) -> Result<()> {
    let key = VerifyingKey::from_bytes(public).map_err(|e| Error::Signature(format!("bad public key: {e}")))?;
    let raw = B64
        .decode(String::from_utf8_lossy(sig).trim())
        .map_err(|_| Error::Signature("signature is not base64".into()))?;
    let sig = Signature::from_slice(&raw).map_err(|_| Error::Signature("signature has the wrong length".into()))?;
    key.verify(body, &sig).map_err(|_| Error::Signature("wrong key or the manifest was changed".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signs_and_verifies() {
        let secret = generate_secret().unwrap();
        let public = decode_key(&public_key(&secret).unwrap()).unwrap();
        let sig = sign(&secret, b"hello").unwrap();
        assert!(verify(&public, b"hello", sig.as_bytes()).is_ok());
        assert!(verify(&public, b"hellp", sig.as_bytes()).is_err());
        let other = decode_key(&public_key(&generate_secret().unwrap()).unwrap()).unwrap();
        assert!(verify(&other, b"hello", sig.as_bytes()).is_err());
        assert!(verify(&public, b"hello", b"not base64!").is_err());
        assert!(verify(&public, b"hello", b"").is_err());
    }
}
