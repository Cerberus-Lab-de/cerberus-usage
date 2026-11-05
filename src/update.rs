//! Self-update security core: signed release manifests (Ed25519) and version rules.
//! Network and UI live elsewhere; this module only decides "trust this installer or not".

use std::fmt;

use ed25519_dalek::{Signature, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Release manifest as published next to the installer. Unknown fields are rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: Version,
    pub file: String,
    /// Lowercase hex SHA-256 of the installer.
    pub sha256: String,
    /// Lowercase hex Ed25519 signature (128 chars) over [`signed_payload`].
    pub signature: String,
}

impl Manifest {
    /// Parses manifest JSON. The error never echoes the input.
    pub fn parse(json: &str) -> Result<Manifest, UpdateError> {
        serde_json::from_str(json)
            .map_err(|_| UpdateError::Malformed("Manifest ist ungueltig".to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateError {
    BadSignature,
    ChecksumMismatch,
    NotNewer,
    BadFileName,
    /// Short fixed text, never a response body.
    Malformed(String),
    /// Short fixed text, never a response body.
    Network(String),
    NoKey,
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadSignature => write!(f, "Die Signatur des Updates ist ungültig."),
            Self::ChecksumMismatch => write!(f, "Die Prüfsumme des Installers stimmt nicht."),
            Self::NotNewer => {
                write!(f, "Das Update ist nicht neuer als die installierte Version.")
            }
            Self::BadFileName => write!(f, "Der Dateiname des Updates ist ungültig."),
            Self::Malformed(m) => write!(f, "Update-Daten fehlerhaft: {m}"),
            Self::Network(m) => write!(f, "Netzwerkfehler beim Update: {m}"),
            Self::NoKey => write!(f, "In dieser Version ist kein Update-Schlüssel hinterlegt."),
        }
    }
}

impl std::error::Error for UpdateError {}

/// The exact bytes that are signed.
pub fn signed_payload(version: &Version, file: &str, sha256: &str) -> Vec<u8> {
    format!("cerberus-usage\n{version}\n{file}\n{sha256}\n").into_bytes()
}

/// Semver precedence, ignoring build metadata. Pre-releases sort below their release.
pub fn is_newer(candidate: &Version, current: &Version) -> bool {
    let strip = |v: &Version| {
        let mut v = v.clone();
        v.build = semver::BuildMetadata::EMPTY;
        v
    };
    strip(candidate) > strip(current)
}

fn file_name_ok(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['/', '\\', ':'])
        && !name.contains("..")
        && name.to_ascii_lowercase().ends_with(".exe")
}

/// Checks, in this order: file name, version, signature, installer hash.
pub fn verify(
    m: &Manifest,
    installer: &[u8],
    current: &Version,
    public_key: &[u8; 32],
) -> Result<(), UpdateError> {
    if !file_name_ok(&m.file) {
        return Err(UpdateError::BadFileName);
    }
    if !is_newer(&m.version, current) {
        return Err(UpdateError::NotNewer);
    }
    let key = VerifyingKey::from_bytes(public_key)
        .map_err(|_| UpdateError::Malformed("Update-Schlüssel ungültig".to_string()))?;
    let sig_bytes = hex_decode(&m.signature)
        .and_then(|b| <[u8; 64]>::try_from(b.as_slice()).ok())
        .ok_or(UpdateError::BadSignature)?;
    let signature = Signature::from_bytes(&sig_bytes);
    let payload = signed_payload(&m.version, &m.file, &m.sha256);
    key.verify_strict(&payload, &signature)
        .map_err(|_| UpdateError::BadSignature)?;
    let actual = hex_encode(Sha256::digest(installer).as_slice());
    if actual != m.sha256 {
        return Err(UpdateError::ChecksumMismatch);
    }
    Ok(())
}

/// [`verify`] against the key compiled into the app; `NoKey` while none is set.
pub fn verify_with_compiled_key(
    m: &Manifest,
    installer: &[u8],
    current: &Version,
) -> Result<(), UpdateError> {
    match crate::update_key::RELEASE_PUBLIC_KEY {
        Some(key) => verify(m, installer, current, &key),
        None => Err(UpdateError::NoKey),
    }
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

pub(crate) fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    s.as_bytes()
        .chunks(2)
        .map(|p| Some((nibble(p[0])? << 4) | nibble(p[1])?))
        .collect()
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release_sign::{keypair_from_seed, sign_manifest};

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }
    fn key(seed: u8) -> ed25519_dalek::SigningKey {
        keypair_from_seed([seed; 32])
    }
    fn pk(k: &ed25519_dalek::SigningKey) -> [u8; 32] {
        k.verifying_key().to_bytes()
    }
    const INSTALLER: &[u8] = b"fake installer bytes";

    fn signed(version: &str, file: &str) -> Manifest {
        sign_manifest(&key(1), &v(version), INSTALLER, file)
    }

    #[test]
    fn current_version_parses_as_semver() {
        assert!(Version::parse(CURRENT_VERSION).is_ok());
    }

    #[test]
    fn verify_accepts_valid_manifest() {
        let m = signed("1.0.1", "Cerberus-Usage-Setup.exe");
        assert_eq!(verify(&m, INSTALLER, &v("1.0.0"), &pk(&key(1))), Ok(()));
    }

    #[test]
    fn verify_rejects_tampered_installer() {
        let m = signed("1.0.1", "setup.exe");
        assert_eq!(
            verify(&m, b"other bytes", &v("1.0.0"), &pk(&key(1))),
            Err(UpdateError::ChecksumMismatch)
        );
    }

    #[test]
    fn verify_rejects_wrong_signature() {
        let m = signed("1.0.1", "setup.exe");
        assert_eq!(
            verify(&m, INSTALLER, &v("1.0.0"), &pk(&key(2))),
            Err(UpdateError::BadSignature)
        );
        // Altered hash with the old signature.
        let mut forged = m.clone();
        forged.sha256 = hex_encode(Sha256::digest(b"evil").as_slice());
        assert_eq!(
            verify(&forged, b"evil", &v("1.0.0"), &pk(&key(1))),
            Err(UpdateError::BadSignature)
        );
        // Malformed, empty, all-zero and short signatures.
        for bad in ["zz", "", &"0".repeat(128), &m.signature[..126]] {
            let mut x = m.clone();
            x.signature = bad.to_string();
            assert_eq!(
                verify(&x, INSTALLER, &v("1.0.0"), &pk(&key(1))),
                Err(UpdateError::BadSignature)
            );
        }
    }

    #[test]
    fn verify_rejects_same_or_older_version() {
        let m = signed("1.0.0", "setup.exe");
        assert_eq!(verify(&m, INSTALLER, &v("1.0.0"), &pk(&key(1))), Err(UpdateError::NotNewer));
        assert_eq!(verify(&m, INSTALLER, &v("1.2.0"), &pk(&key(1))), Err(UpdateError::NotNewer));
    }

    #[test]
    fn verify_rejects_path_in_file_name() {
        for name in [
            "..\\x.exe", "a/b.exe", "C:x.exe", "a\\b.exe", "x..exe", "", "setup.msi", "setup",
        ] {
            let m = signed("1.0.1", name);
            assert_eq!(
                verify(&m, INSTALLER, &v("1.0.0"), &pk(&key(1))),
                Err(UpdateError::BadFileName),
                "{name}"
            );
        }
    }

    #[test]
    fn verify_without_compiled_key_is_no_key() {
        // Holds while the release process has not set the key.
        assert!(crate::update_key::RELEASE_PUBLIC_KEY.is_none());
        let m = signed("1.0.1", "setup.exe");
        assert_eq!(verify_with_compiled_key(&m, INSTALLER, &v("1.0.0")), Err(UpdateError::NoKey));
    }

    #[test]
    fn manifest_parse_rejects_garbage() {
        for bad in [
            "",
            "not json",
            "{}",
            r#"{"version":"x","file":"a.exe","sha256":"","signature":""}"#,
            r#"{"version":"1.0.1","file":"a.exe","sha256":"","signature":"","extra":1}"#,
        ] {
            assert!(matches!(Manifest::parse(bad), Err(UpdateError::Malformed(_))), "{bad}");
        }
        let m = signed("1.0.1", "setup.exe");
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(Manifest::parse(&json), Ok(m));
    }

    #[test]
    fn malformed_error_does_not_echo_input() {
        let e = Manifest::parse("SECRET-BODY-123").unwrap_err();
        assert!(!e.to_string().contains("SECRET-BODY"));
    }

    #[test]
    fn signed_payload_is_stable() {
        assert_eq!(
            signed_payload(&v("1.2.3"), "a.exe", "ab12"),
            b"cerberus-usage\n1.2.3\na.exe\nab12\n".to_vec()
        );
    }

    #[test]
    fn is_newer_handles_prerelease_and_patch() {
        assert!(is_newer(&v("1.0.1"), &v("1.0.0")));
        assert!(!is_newer(&v("1.1.0-rc.1"), &v("1.1.0")));
        assert!(is_newer(&v("1.1.0"), &v("1.1.0-rc.1")));
        assert!(!is_newer(&v("1.0.0"), &v("1.0.0")));
        assert!(!is_newer(&v("1.0.0+build.5"), &v("1.0.0")));
    }

    #[test]
    fn hex_roundtrip_and_rejects_bad_input() {
        assert_eq!(hex_decode(&hex_encode(&[0, 15, 255])), Some(vec![0, 15, 255]));
        assert_eq!(hex_decode("abc"), None);
        assert_eq!(hex_decode("zz"), None);
        assert_eq!(hex_decode("é"), None);
    }
}
