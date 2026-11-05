//! Pure signing helpers for the `sign-release` tool. The bin does argument parsing,
//! credential-store access and file writing; everything testable lives here.
//! The private-key code path is only called from that bin, never from the app.

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::update::{Manifest, file_name_ok, hex_decode, hex_encode, signed_payload, verify};

/// Deterministic key from a 32-byte seed (tests pass a fixed seed). The seed copy is wiped.
pub fn keypair_from_seed(mut seed: [u8; 32]) -> SigningKey {
    let key = SigningKey::from_bytes(&seed);
    seed.zeroize();
    key
}

/// Fresh key from the OS random source.
pub fn generate_keypair() -> Result<SigningKey, String> {
    let mut seed = Zeroizing::new([0u8; 32]);
    getrandom::fill(seed.as_mut_slice())
        .map_err(|_| "Zufallsquelle des Betriebssystems nicht verfügbar".to_string())?;
    Ok(keypair_from_seed(*seed))
}

/// Builds and signs a manifest for `installer_bytes`.
pub fn sign_manifest(
    signing_key: &SigningKey,
    version: &semver::Version,
    installer_bytes: &[u8],
    file_name: &str,
) -> Manifest {
    let sha256 = hex_encode(Sha256::digest(installer_bytes).as_slice());
    let payload = signed_payload(version, file_name, &sha256);
    let signature = hex_encode(&signing_key.sign(&payload).to_bytes());
    Manifest {
        version: version.clone(),
        file: file_name.to_string(),
        sha256,
        signature,
    }
}

/// Validates the inputs, signs, and verifies the result against the derived public key
/// before the caller writes anything.
pub fn sign_checked(
    signing_key: &SigningKey,
    version: &semver::Version,
    installer_bytes: &[u8],
    file_name: &str,
) -> Result<Manifest, String> {
    if !file_name_ok(file_name) {
        return Err(format!(
            "ungültiger Installer-Dateiname (erlaubt: A-Z a-z 0-9 . _ -, endet auf .exe): {file_name}"
        ));
    }
    if !version.pre.is_empty() {
        return Err("Vorabversionen (z. B. -rc.1) dürfen nicht signiert werden".to_string());
    }
    let manifest = sign_manifest(signing_key, version, installer_bytes, file_name);
    let public = signing_key.verifying_key().to_bytes();
    verify(&manifest, installer_bytes, &semver::Version::new(0, 0, 0), &public)
        .map_err(|e| format!("Gegenprüfung der Signatur fehlgeschlagen: {e}"))?;
    Ok(manifest)
}

pub fn public_key_hex(signing_key: &SigningKey) -> String {
    hex_encode(&signing_key.verifying_key().to_bytes())
}

pub fn private_key_hex(signing_key: &SigningKey) -> Zeroizing<String> {
    let bytes = Zeroizing::new(signing_key.to_bytes());
    Zeroizing::new(hex_encode(bytes.as_slice()))
}

/// Restores a key from the hex stored in the credential store.
pub fn signing_key_from_hex(hex: &str) -> Result<SigningKey, String> {
    let bytes = Zeroizing::new(
        hex_decode(hex.trim()).ok_or("gespeicherter Schlüssel ist kein Hex".to_string())?,
    );
    let seed = Zeroizing::new(
        <[u8; 32]>::try_from(bytes.as_slice())
            .map_err(|_| "gespeicherter Schlüssel hat die falsche Länge".to_string())?,
    );
    Ok(SigningKey::from_bytes(&seed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SigningKey {
        keypair_from_seed([7; 32])
    }

    #[test]
    fn sign_checked_round_trips() {
        let v = semver::Version::new(1, 0, 1);
        let m = sign_checked(&key(), &v, b"bytes", "CerberusUsage-Setup-1.0.1.exe").unwrap();
        assert_eq!(m.version, v);
    }

    #[test]
    fn sign_checked_rejects_bad_name_and_prerelease() {
        let v = semver::Version::new(1, 0, 1);
        assert!(sign_checked(&key(), &v, b"x", "a b.exe").is_err());
        assert!(sign_checked(&key(), &v, b"x", "NUL.exe").is_err());
        let rc = semver::Version::parse("1.1.0-rc.1").unwrap();
        assert!(sign_checked(&key(), &rc, b"x", "a.exe").is_err());
    }

    #[test]
    fn key_hex_round_trips() {
        let k = key();
        let back = signing_key_from_hex(&private_key_hex(&k)).unwrap();
        assert_eq!(back.to_bytes(), k.to_bytes());
        assert!(signing_key_from_hex("zz").is_err());
    }
}
