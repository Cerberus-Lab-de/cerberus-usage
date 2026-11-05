//! Pure signing helpers for the `sign-release` tool. The bin does argument parsing,
//! credential-store access and file writing; everything testable lives here.
//! The private-key code path is only called from that bin, never from the app.

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::update::{Manifest, hex_decode, hex_encode, signed_payload};

/// Deterministic key from a 32-byte seed (tests pass a fixed seed). The seed copy is wiped.
pub fn keypair_from_seed(mut seed: [u8; 32]) -> SigningKey {
    let key = SigningKey::from_bytes(&seed);
    seed.zeroize();
    key
}

/// Fresh key from the OS random source.
pub fn generate_keypair() -> Result<SigningKey, String> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed)
        .map_err(|_| "Zufallsquelle des Betriebssystems nicht verfuegbar".to_string())?;
    Ok(keypair_from_seed(seed))
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

pub fn public_key_hex(signing_key: &SigningKey) -> String {
    hex_encode(&signing_key.verifying_key().to_bytes())
}

pub fn private_key_hex(signing_key: &SigningKey) -> String {
    hex_encode(&signing_key.to_bytes())
}

/// Restores a key from the hex stored in the credential store.
pub fn signing_key_from_hex(hex: &str) -> Result<SigningKey, String> {
    let mut bytes =
        hex_decode(hex.trim()).ok_or("gespeicherter Schluessel ist kein Hex".to_string())?;
    let result = <[u8; 32]>::try_from(bytes.as_slice())
        .map(|seed| SigningKey::from_bytes(&seed))
        .map_err(|_| "gespeicherter Schluessel hat die falsche Laenge".to_string());
    bytes.zeroize();
    result
}
