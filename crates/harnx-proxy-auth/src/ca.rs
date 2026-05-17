use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::Result;
use hudsucker::rcgen::{BasicConstraints, Certificate, CertificateParams, DnType, IsCa, KeyPair};
use tempfile::TempDir;

pub struct CaSetup {
    pub cert_pem_path: PathBuf,
    pub key_pair: KeyPair,
    pub cert: Certificate,
}

pub fn setup() -> Result<(CaSetup, TempDir)> {
    let temp_dir = tempfile::Builder::new()
        .prefix("harnx-auth-proxy-")
        .tempdir()?;

    #[cfg(unix)]
    fs::set_permissions(temp_dir.path(), fs::Permissions::from_mode(0o700))?;

    let cert_pem_path = temp_dir.path().join("ca.pem");

    let key_pair = KeyPair::generate()?;
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "harnx auth proxy CA");
    let cert = params.self_signed(&key_pair)?;

    fs::write(&cert_pem_path, cert.pem())?;

    Ok((
        CaSetup {
            cert_pem_path,
            key_pair,
            cert,
        },
        temp_dir,
    ))
}
