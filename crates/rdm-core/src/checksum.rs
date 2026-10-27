use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Result, bail};
use md5::Md5;
use sha2::{Digest, Sha256};

/// An expected file digest, written `sha256:<hex>`, `md5:<hex>`, or bare hex
/// (64 chars = SHA-256, 32 = MD5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checksum {
    Sha256(String),
    Md5(String),
}

impl FromStr for Checksum {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        let (algo, hex) = match s.split_once(':') {
            Some((a, h)) => (a.trim().to_ascii_lowercase(), h.trim()),
            None => (String::new(), s),
        };
        let hex = hex.to_ascii_lowercase();
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("checksum must be hexadecimal");
        }
        match (algo.as_str(), hex.len()) {
            ("sha256" | "sha-256" | "", 64) => Ok(Self::Sha256(hex)),
            ("md5" | "", 32) => Ok(Self::Md5(hex)),
            _ => bail!("expected sha256:<64 hex chars> or md5:<32 hex chars>"),
        }
    }
}

impl fmt::Display for Checksum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sha256(h) => write!(f, "sha256:{h}"),
            Self::Md5(h) => write!(f, "md5:{h}"),
        }
    }
}

impl Checksum {
    /// Hashes the file (blocking) and fails if it doesn't match.
    pub fn verify(&self, path: &Path) -> Result<()> {
        let actual = match self {
            Self::Sha256(_) => digest_file::<Sha256>(path)?,
            Self::Md5(_) => digest_file::<Md5>(path)?,
        };
        let expected = match self {
            Self::Sha256(h) | Self::Md5(h) => h,
        };
        if &actual != expected {
            bail!("checksum mismatch: expected {expected}, got {actual}");
        }
        Ok(())
    }
}

fn digest_file<D: Digest>(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = D::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_forms() {
        let sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(sha.parse::<Checksum>().unwrap(), Checksum::Sha256(sha.into()));
        assert_eq!(
            format!("SHA256:{}", sha.to_uppercase()).parse::<Checksum>().unwrap(),
            Checksum::Sha256(sha.into())
        );
        assert!(matches!("md5:d41d8cd98f00b204e9800998ecf8427e".parse(), Ok(Checksum::Md5(_))));
        assert!("sha256:abc".parse::<Checksum>().is_err());
        assert!("zz".repeat(16).parse::<Checksum>().is_err());
    }

    #[test]
    fn verifies_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"").unwrap();
        let empty_sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert!(empty_sha.parse::<Checksum>().unwrap().verify(&path).is_ok());
        assert!("md5:d41d8cd98f00b204e9800998ecf8427e".parse::<Checksum>().unwrap().verify(&path).is_ok());
        assert!("0".repeat(64).parse::<Checksum>().unwrap().verify(&path).is_err());
    }
}
