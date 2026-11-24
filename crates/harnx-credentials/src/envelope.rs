//! Versioned AEAD envelope. AAD binds ciphertext to its exact storage location.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use ring::{
    aead,
    rand::{SecureRandom, SystemRandom},
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{Error, Keyring, Result};

pub const ALGORITHM: &str = "CHACHA20-POLY1305";
pub const ENVELOPE_VERSION: u32 = 1;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    v: u32,
    alg: String,
    kid: String,
    nonce: String,
    ct: String,
}

/// Encrypt a serializable value. The caller must supply the actual bucket and full key.
pub fn seal<T: Serialize>(keys: &Keyring, bucket: &str, key: &str, value: &T) -> Result<Vec<u8>> {
    let kid = keys.active_key_id();
    let mut nonce = [0; aead::NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| Error::Crypto)?;
    let mut plaintext = serde_json::to_vec(value).map_err(|_| Error::InvalidRecord)?;
    keys.key(kid)?
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aad(bucket, key, ENVELOPE_VERSION, kid)?,
            &mut plaintext,
        )
        .map_err(|_| Error::Crypto)?;
    let envelope = Envelope {
        v: ENVELOPE_VERSION,
        alg: ALGORITHM.into(),
        kid: kid.into(),
        nonce: STANDARD.encode(nonce),
        ct: STANDARD.encode(plaintext),
    };
    serde_json::to_vec(&envelope).map_err(|_| Error::InvalidEnvelope)
}

/// Authenticate before deserializing. A wrong key or location never returns plaintext.
pub fn open<T: DeserializeOwned>(
    keys: &Keyring,
    bucket: &str,
    key: &str,
    bytes: &[u8],
) -> Result<T> {
    let envelope: Envelope = serde_json::from_slice(bytes).map_err(|_| Error::InvalidEnvelope)?;
    if envelope.v != ENVELOPE_VERSION || envelope.alg != ALGORITHM {
        return Err(Error::InvalidEnvelope);
    }
    let nonce: [u8; aead::NONCE_LEN] = STANDARD
        .decode(&envelope.nonce)
        .map_err(|_| Error::InvalidEnvelope)?
        .try_into()
        .map_err(|_| Error::InvalidEnvelope)?;
    let mut ciphertext = STANDARD
        .decode(&envelope.ct)
        .map_err(|_| Error::InvalidEnvelope)?;
    let plaintext = keys
        .key(&envelope.kid)?
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aad(bucket, key, envelope.v, &envelope.kid)?,
            &mut ciphertext,
        )
        .map_err(|_| Error::AuthenticationFailed)?;
    serde_json::from_slice(plaintext).map_err(|_| Error::InvalidRecord)
}

fn aad(bucket: &str, key: &str, version: u32, kid: &str) -> Result<aead::Aad<Vec<u8>>> {
    // A tuple gives unambiguous boundaries even when identity strings contain delimiters.
    serde_json::to_vec(&(bucket, key, version, kid))
        .map(aead::Aad::from)
        .map_err(|_| Error::InvalidEnvelope)
}
