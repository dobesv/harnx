use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use hudsucker::rcgen::{BasicConstraints, Certificate, CertificateParams, DnType, IsCa, KeyPair};

#[derive(Debug)]
pub struct CaTempDir(PathBuf);

impl CaTempDir {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for CaTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub struct CaSetup {
    pub cert_pem_path: PathBuf,
    pub key_pair: KeyPair,
    pub cert: Certificate,
}

pub fn setup() -> Result<(CaSetup, CaTempDir)> {
    let temp_dir_path =
        std::env::temp_dir().join(format!("harnx-auth-proxy-{}", std::process::id()));
    fs::create_dir_all(&temp_dir_path)?;

    #[cfg(unix)]
    fs::set_permissions(&temp_dir_path, fs::Permissions::from_mode(0o700))?;

    let temp_dir = CaTempDir(temp_dir_path);
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
