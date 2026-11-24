//! Explicit JSON key-file loading and generation. Never derive keys from NATS tokens.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use ring::{
    aead,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

use crate::{Error, Result, Secret};

const KEY_BYTES: usize = 32;

#[derive(Clone, Debug)]
pub struct Keyring {
    active: String,
    keys: BTreeMap<String, Secret<[u8; KEY_BYTES]>>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    active: String,
    keys: BTreeMap<String, Secret<String>>,
}

impl Keyring {
    /// Read the configured file. Missing files are startup errors, never auto-created.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|_| Error::KeyFileIo)?;
        Self::from_json(&bytes)
    }

    /// Parse active and decrypt-only keys. All keys must be exactly 32 bytes.
    pub fn from_json(json: &[u8]) -> Result<Self> {
        let file: KeyFile = serde_json::from_slice(json).map_err(|_| Error::InvalidKeyFile)?;
        validate_file(&file)?;
        let keys = file
            .keys
            .into_iter()
            .map(decode_key)
            .collect::<Result<_>>()?;
        Ok(Self {
            active: file.active,
            keys,
        })
    }

    /// Generate JSON for an operator to save with restrictive permissions.
    /// Returned contents are secret; this method does not write a file.
    pub fn generate_key_file() -> Result<Secret<String>> {
        let mut bytes = [0; KEY_BYTES];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| Error::Crypto)?;
        let active = uuid::Uuid::new_v4().to_string();
        let file = KeyFile {
            active: active.clone(),
            keys: BTreeMap::from([(active, Secret::new(STANDARD.encode(bytes)))]),
        };
        serde_json::to_string_pretty(&file)
            .map(Secret::new)
            .map_err(|_| Error::InvalidKeyFile)
    }

    pub fn active_key_id(&self) -> &str {
        &self.active
    }

    pub(crate) fn key(&self, id: &str) -> Result<aead::LessSafeKey> {
        let key = self.keys.get(id).ok_or(Error::UnknownKey)?;
        aead::UnboundKey::new(&aead::CHACHA20_POLY1305, key.expose())
            .map(aead::LessSafeKey::new)
            .map_err(|_| Error::Crypto)
    }
}

fn validate_file(file: &KeyFile) -> Result<()> {
    if file.active.is_empty()
        || !file.keys.contains_key(&file.active)
        || file.keys.keys().any(String::is_empty)
    {
        return Err(Error::InvalidKeyFile);
    }
    Ok(())
}

fn decode_key(
    (id, encoded): (String, Secret<String>),
) -> Result<(String, Secret<[u8; KEY_BYTES]>)> {
    let bytes = STANDARD
        .decode(encoded.expose())
        .map_err(|_| Error::InvalidKeyFile)?;
    let key = bytes.try_into().map_err(|_| Error::InvalidKeyFile)?;
    Ok((id, Secret::new(key)))
}
