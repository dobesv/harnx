use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use harnx_credentials::{envelope, Error, Keyring, Principal, Secret};
use serde_json::{json, Value};

use crate::common::*;

#[test]
fn envelope_round_trip_and_nonce_uniqueness() -> Result<()> {
    let keys = keyring()?;
    let plaintext = Secret::from("token-secret-marker");
    let first = envelope::seal(&keys, "bucket", "v1.server.user", &plaintext)?;
    let second = envelope::seal(&keys, "bucket", "v1.server.user", &plaintext)?;
    assert_ne!(first, second);
    assert!(!String::from_utf8_lossy(&first).contains(plaintext.expose()));
    let recovered: Secret<String> = envelope::open(&keys, "bucket", "v1.server.user", &first)?;
    assert_eq!(recovered.expose(), plaintext.expose());
    let wire: Value = serde_json::from_slice(&first)?;
    assert_eq!(wire["v"], 1);
    assert_eq!(wire["alg"], "CHACHA20-POLY1305");
    Ok(())
}

#[test]
fn envelope_rejects_wrong_key_tampering_and_relocation() -> Result<()> {
    let keys = keyring()?;
    let bytes = envelope::seal(&keys, "credentials", "v1.server.user", &"secret-marker")?;
    assert_eq!(
        envelope::open::<String>(
            keyring_byte(7)?.as_ref(),
            "credentials",
            "v1.server.user",
            &bytes
        )
        .unwrap_err(),
        Error::AuthenticationFailed
    );
    for (bucket, key) in [
        ("other", "v1.server.user"),
        ("credentials", "v1.other.user"),
        ("credentials", "v1.server.bob"),
    ] {
        assert_eq!(
            envelope::open::<String>(&keys, bucket, key, &bytes).unwrap_err(),
            Error::AuthenticationFailed
        );
    }
    let mut wire: Value = serde_json::from_slice(&bytes)?;
    let mut ciphertext = STANDARD.decode(wire["ct"].as_str().unwrap())?;
    ciphertext[0] ^= 1;
    wire["ct"] = json!(STANDARD.encode(ciphertext));
    assert_eq!(
        envelope::open::<String>(
            &keys,
            "credentials",
            "v1.server.user",
            &serde_json::to_vec(&wire)?
        )
        .unwrap_err(),
        Error::AuthenticationFailed
    );
    wire["v"] = json!(2);
    assert_eq!(
        envelope::open::<String>(
            &keys,
            "credentials",
            "v1.server.user",
            &serde_json::to_vec(&wire)?
        )
        .unwrap_err(),
        Error::InvalidEnvelope
    );
    Ok(())
}

#[test]
fn key_rotation_decrypts_old_key_and_binds_kid() -> Result<()> {
    let json = json!({"active":"old", "keys":{"old":STANDARD.encode([42;32]),"new":STANDARD.encode([42;32])}});
    let old = Keyring::from_json(&serde_json::to_vec(&json)?)?;
    let bytes = envelope::seal(&old, "bucket", "key", &"secret")?;
    let mut json = json;
    json["active"] = json!("new");
    let new = Keyring::from_json(&serde_json::to_vec(&json)?)?;
    assert_eq!(
        envelope::open::<String>(&new, "bucket", "key", &bytes)?,
        "secret"
    );
    let mut wire: Value = serde_json::from_slice(&bytes)?;
    wire["kid"] = json!("new");
    assert_eq!(
        envelope::open::<String>(&new, "bucket", "key", &serde_json::to_vec(&wire)?).unwrap_err(),
        Error::AuthenticationFailed
    );
    wire["kid"] = json!("absent");
    assert_eq!(
        envelope::open::<String>(&new, "bucket", "key", &serde_json::to_vec(&wire)?).unwrap_err(),
        Error::UnknownKey
    );
    Ok(())
}

#[test]
fn missing_and_invalid_key_files_are_errors() -> Result<()> {
    let dir = tempfile::tempdir()?;
    assert_eq!(
        Keyring::load(dir.path().join("missing")).unwrap_err(),
        Error::KeyFileIo
    );
    for json in [
        json!({}),
        json!({"active":"missing","keys":{}}),
        json!({"active":"key","keys":{"key":"not base64"}}),
        json!({"active":"key","keys":{"key":STANDARD.encode([1;31])}}),
    ] {
        assert_eq!(
            Keyring::from_json(&serde_json::to_vec(&json)?).unwrap_err(),
            Error::InvalidKeyFile
        );
    }
    let generated = Keyring::generate_key_file()?;
    std::fs::write(dir.path().join("keys.json"), generated.expose())?;
    Keyring::load(dir.path().join("keys.json"))?;
    assert!(!format!("{generated:?} {generated}").contains("keys"));
    Ok(())
}

#[test]
fn ownerless_segment_never_collides_with_user_encoding() {
    assert_eq!(Principal::Ownerless.key_segment(), "=");
    for id in ["=", "", "PQ", "PQ==", "Ownerless", "anon", "alice"] {
        assert_ne!(
            Principal::User(id.into()).key_segment(),
            Principal::Ownerless.key_segment()
        );
    }
}

#[test]
fn keygen_binary_prints_loadable_json() -> Result<()> {
    let output =
        std::process::Command::new(env!("CARGO_BIN_EXE_harnx-credentials-keygen")).output()?;
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    Keyring::from_json(&output.stdout)?;
    Ok(())
}
