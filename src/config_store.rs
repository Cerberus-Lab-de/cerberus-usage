//! Keys inside `settings.json`: DPAPI-encrypted by default, readable when the user opts in.
//!
//! `ConfigStore` is a [`SecretStore`] over the file. `KeyStore` puts it in front of the old
//! credential-store entries so keys saved by v1.0.x keep working until they are saved again.
//! No error or log line here ever carries a key value.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use zeroize::Zeroizing;

use crate::cards::update_settings;
use crate::dpapi;
use crate::secrets::{SecretError, SecretStore, Slot};
use crate::settings::{self, KeyForm, Settings, StoredKey};

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Strict standard base64 with padding; `None` on any irregularity.
fn b64_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let groups = bytes.len() / 4;
    for (i, quad) in bytes.chunks(4).enumerate() {
        let pad = quad.iter().rev().take_while(|&&b| b == b'=').count();
        if pad > 2 || (pad > 0 && i + 1 != groups) {
            return None;
        }
        let mut n = 0u32;
        for &b in &quad[..4 - pad] {
            let v = B64.iter().position(|&c| c == b)? as u32;
            n = n << 6 | v;
        }
        n <<= 6 * pad as u32;
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

fn encode(plain: &str, plaintext: bool) -> Result<StoredKey, SecretError> {
    if plaintext {
        return Ok(StoredKey { form: KeyForm::Plain, value: plain.to_string() });
    }
    let blob = dpapi::protect(plain.as_bytes()).map_err(|_| SecretError::Other)?;
    Ok(StoredKey { form: KeyForm::Dpapi, value: b64_encode(&blob) })
}

fn decode(key: &StoredKey) -> Result<Zeroizing<String>, SecretError> {
    match key.form {
        KeyForm::Plain => Ok(Zeroizing::new(key.value.clone())),
        KeyForm::Dpapi => {
            let blob = b64_decode(&key.value).ok_or(SecretError::NotDecryptable)?;
            let bytes = dpapi::unprotect(&blob).map_err(|_| SecretError::NotDecryptable)?;
            let text = std::str::from_utf8(&bytes).map_err(|_| SecretError::NotDecryptable)?;
            Ok(Zeroizing::new(text.to_string()))
        }
    }
}

fn slot_of(s: &mut Settings, slot: Slot) -> &mut Option<StoredKey> {
    match slot {
        Slot::ApiKey => &mut s.api_key,
        Slot::AdminKey => &mut s.admin_key,
    }
}

/// Keys stored in the settings file.
#[derive(Debug)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl SecretStore for ConfigStore {
    fn get(&self, s: Slot) -> Result<Option<String>, SecretError> {
        let (mut settings, error) = settings::load(&self.path);
        if error.is_some() {
            return Err(SecretError::ConfigUnreadable);
        }
        match slot_of(&mut settings, s).as_ref() {
            None => Ok(None),
            Some(key) => Ok(Some(decode(key)?.to_string())),
        }
    }

    fn set(&self, s: Slot, v: &str) -> Result<(), SecretError> {
        let ran = Cell::new(false);
        update_settings(&self.path, |current| {
            ran.set(true);
            let mut next = current.clone();
            *slot_of(&mut next, s) = Some(encode(v, current.keys_plaintext).map_err(|_| String::new())?);
            Ok(next)
        })
        // A failure before the edit ran is a load failure; later ones are encryption or write errors.
        .map(|_| ())
        .map_err(|_| if ran.get() { SecretError::Other } else { SecretError::ConfigUnreadable })
    }

    fn delete(&self, s: Slot) -> Result<(), SecretError> {
        let (mut current, error) = settings::load(&self.path);
        if error.is_some() {
            return Err(SecretError::ConfigUnreadable);
        }
        if slot_of(&mut current, s).is_none() {
            return Ok(());
        }
        let ran = Cell::new(false);
        update_settings(&self.path, |current| {
            ran.set(true);
            let mut next = current.clone();
            *slot_of(&mut next, s) = None;
            Ok(next)
        })
        .map(|_| ())
        .map_err(|_| if ran.get() { SecretError::Other } else { SecretError::ConfigUnreadable })
    }
}

/// Converts both keys and the flag in one locked save. Nothing is written unless every stored key
/// can be decrypted, so a foreign blob can never be replaced by something unreadable.
pub fn set_plaintext(path: &Path, on: bool) -> Result<Settings, String> {
    update_settings(path, |current| {
        let mut next = current.clone();
        for key in [&mut next.api_key, &mut next.admin_key] {
            if let Some(stored) = key.as_ref() {
                let plain = decode(stored).map_err(|_| {
                    "Ein gespeicherter Schlüssel lässt sich nicht entschlüsseln. Umstellung abgebrochen, nichts wurde geändert."
                        .to_string()
                })?;
                *key = Some(
                    encode(&plain, on)
                        .map_err(|_| "Ein Schlüssel konnte nicht verschlüsselt werden. Umstellung abgebrochen.".to_string())?,
                );
            }
        }
        next.keys_plaintext = on;
        Ok(next)
    })
}

/// Settings file first, then the old credential-store entries (read-only fallback).
pub struct KeyStore {
    config: ConfigStore,
    legacy: Arc<dyn SecretStore + Send + Sync>,
}

impl KeyStore {
    pub fn new(config: ConfigStore, legacy: Arc<dyn SecretStore + Send + Sync>) -> Self {
        Self { config, legacy }
    }
}

impl SecretStore for KeyStore {
    fn get(&self, s: Slot) -> Result<Option<String>, SecretError> {
        // Only a clean "nothing there" falls through; a broken file or foreign blob must not
        // quietly surface an older key.
        match self.config.get(s)? {
            Some(v) => Ok(Some(v)),
            None => self.legacy.get(s),
        }
    }

    fn set(&self, s: Slot, v: &str) -> Result<(), SecretError> {
        self.config.set(s, v)?;
        let _ = self.legacy.delete(s);
        Ok(())
    }

    fn delete(&self, s: Slot) -> Result<(), SecretError> {
        let config = self.config.delete(s);
        let legacy = self.legacy.delete(s);
        config.and(legacy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    const KEY: &str = "sk-ant-test-NICHTECHT-1234";
    const ADMIN: &str = "sk-ant-admin01-NICHTECHT-5678";

    fn temp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        (dir, path)
    }

    #[derive(Default)]
    struct FakeLegacy {
        map: Mutex<HashMap<&'static str, String>>,
        fail_delete: bool,
    }

    fn name(s: Slot) -> &'static str {
        match s {
            Slot::ApiKey => "api",
            Slot::AdminKey => "admin",
        }
    }

    impl FakeLegacy {
        fn with(s: Slot, v: &str) -> Arc<Self> {
            let f = Self::default();
            f.map.lock().unwrap().insert(name(s), v.to_string());
            Arc::new(f)
        }
        fn failing() -> Arc<Self> {
            Arc::new(Self { fail_delete: true, ..Self::default() })
        }
        fn has(&self, s: Slot) -> bool {
            self.map.lock().unwrap().contains_key(name(s))
        }
    }

    impl SecretStore for FakeLegacy {
        fn get(&self, s: Slot) -> Result<Option<String>, SecretError> {
            Ok(self.map.lock().unwrap().get(name(s)).cloned())
        }
        fn set(&self, s: Slot, v: &str) -> Result<(), SecretError> {
            self.map.lock().unwrap().insert(name(s), v.to_string());
            Ok(())
        }
        fn delete(&self, s: Slot) -> Result<(), SecretError> {
            if self.fail_delete {
                return Err(SecretError::StoreUnavailable);
            }
            self.map.lock().unwrap().remove(name(s));
            Ok(())
        }
    }

    #[test]
    fn base64_matches_known_vectors_and_rejects_junk() {
        for (plain, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(b64_encode(plain.as_bytes()), enc);
            assert_eq!(b64_decode(enc).unwrap(), plain.as_bytes());
        }
        for bad in ["Zg=", "Zm9", "Zm9v!", "Zg==Zm9v", "====", "Z===", " Zm9v"] {
            assert!(b64_decode(bad).is_none(), "accepted {bad:?}");
        }
    }

    #[test]
    fn default_writes_encrypted() {
        let (_d, path) = temp();
        let store = ConfigStore::new(path.clone());
        store.set(Slot::ApiKey, KEY).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("NICHTECHT") && text.contains("\"dpapi\""), "{text}");
        assert_eq!(store.get(Slot::ApiKey).unwrap().as_deref(), Some(KEY));
        assert_eq!(store.get(Slot::AdminKey).unwrap(), None);
    }

    #[test]
    fn plaintext_writes_readable() {
        let (_d, path) = temp();
        settings::save(&path, &Settings { keys_plaintext: true, ..Settings::default() }).unwrap();
        let store = ConfigStore::new(path.clone());
        store.set(Slot::AdminKey, ADMIN).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(ADMIN) && text.contains("\"plain\""), "{text}");
        assert_eq!(settings::load(&path).0.admin_key.unwrap().form, KeyForm::Plain);
        assert_eq!(store.get(Slot::AdminKey).unwrap().as_deref(), Some(ADMIN));
    }

    #[test]
    fn switch_there_and_back() {
        let (_d, path) = temp();
        let store = ConfigStore::new(path.clone());
        store.set(Slot::ApiKey, KEY).unwrap();
        store.set(Slot::AdminKey, ADMIN).unwrap();

        let on = set_plaintext(&path, true).unwrap();
        assert!(on.keys_plaintext);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(KEY) && text.contains(ADMIN) && !text.contains("dpapi"), "{text}");
        assert_eq!(store.get(Slot::ApiKey).unwrap().as_deref(), Some(KEY));

        let off = set_plaintext(&path, false).unwrap();
        assert!(!off.keys_plaintext);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("NICHTECHT") && !text.contains("\"plain\""), "{text}");
        assert_eq!(store.get(Slot::ApiKey).unwrap().as_deref(), Some(KEY));
        assert_eq!(store.get(Slot::AdminKey).unwrap().as_deref(), Some(ADMIN));
    }

    fn foreign_blob_settings() -> Settings {
        Settings {
            api_key: Some(StoredKey { form: KeyForm::Dpapi, value: b64_encode(b"a blob from another user") }),
            admin_key: Some(StoredKey { form: KeyForm::Plain, value: ADMIN.to_string() }),
            ..Settings::default()
        }
    }

    #[test]
    fn switch_refuses_when_a_key_cannot_be_decrypted() {
        let (_d, path) = temp();
        settings::save(&path, &foreign_blob_settings()).unwrap();
        let before = std::fs::read(&path).unwrap();
        let err = set_plaintext(&path, true).unwrap_err();
        assert!(!err.contains("NICHTECHT"));
        assert_eq!(std::fs::read(&path).unwrap(), before, "file must stay byte-identical");
    }

    #[test]
    fn foreign_blob_reads_as_not_decryptable() {
        let (_d, path) = temp();
        settings::save(&path, &foreign_blob_settings()).unwrap();
        let store = ConfigStore::new(path.clone());
        assert_eq!(store.get(Slot::ApiKey), Err(SecretError::NotDecryptable));
        // Invalid base64 gives the same outcome.
        let mut s = foreign_blob_settings();
        s.api_key.as_mut().unwrap().value = "not base64!!".to_string();
        settings::save(&path, &s).unwrap();
        assert_eq!(store.get(Slot::ApiKey), Err(SecretError::NotDecryptable));
        // The other, readable key is unaffected.
        assert_eq!(store.get(Slot::AdminKey).unwrap().as_deref(), Some(ADMIN));
    }

    #[test]
    fn broken_file_reads_as_config_unreadable_and_set_leaves_it_alone() {
        let (_d, path) = temp();
        let broken = "{ this is not json";
        std::fs::write(&path, broken).unwrap();
        let store = ConfigStore::new(path.clone());
        assert_eq!(store.get(Slot::ApiKey), Err(SecretError::ConfigUnreadable));
        assert_eq!(store.set(Slot::ApiKey, KEY), Err(SecretError::ConfigUnreadable));
        assert_eq!(store.delete(Slot::ApiKey), Err(SecretError::ConfigUnreadable));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
    }

    #[test]
    fn missing_file_is_no_key_and_delete_creates_nothing() {
        let (_d, path) = temp();
        let store = ConfigStore::new(path.clone());
        assert_eq!(store.get(Slot::ApiKey).unwrap(), None);
        store.delete(Slot::ApiKey).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn delete_removes_only_its_slot() {
        let (_d, path) = temp();
        let store = ConfigStore::new(path);
        store.set(Slot::ApiKey, KEY).unwrap();
        store.set(Slot::AdminKey, ADMIN).unwrap();
        store.delete(Slot::ApiKey).unwrap();
        assert_eq!(store.get(Slot::ApiKey).unwrap(), None);
        assert_eq!(store.get(Slot::AdminKey).unwrap().as_deref(), Some(ADMIN));
    }

    #[test]
    fn concurrent_key_and_settings_saves_lose_nothing() {
        let (_d, path) = temp();
        let a = path.clone();
        let b = path.clone();
        let keys = std::thread::spawn(move || {
            let store = ConfigStore::new(a);
            for i in 0..50 {
                store.set(Slot::ApiKey, &format!("sk-ant-test-{i}")).unwrap();
            }
        });
        let other = std::thread::spawn(move || {
            for i in 0..50u64 {
                update_settings(&b, |s| Ok(Settings { monthly_budget_nano: Some(i), ..s.clone() })).unwrap();
            }
        });
        keys.join().unwrap();
        other.join().unwrap();
        assert_eq!(ConfigStore::new(path.clone()).get(Slot::ApiKey).unwrap().as_deref(), Some("sk-ant-test-49"));
        assert_eq!(settings::load(&path).0.monthly_budget_nano, Some(49));
    }

    #[test]
    fn key_store_prefers_config_and_falls_back_to_legacy() {
        let (_d, path) = temp();
        let legacy = FakeLegacy::with(Slot::ApiKey, "sk-ant-test-OLD");
        let store = KeyStore::new(ConfigStore::new(path.clone()), legacy);
        assert_eq!(store.get(Slot::ApiKey).unwrap().as_deref(), Some("sk-ant-test-OLD"));
        assert_eq!(store.get(Slot::AdminKey).unwrap(), None);
        ConfigStore::new(path).set(Slot::ApiKey, KEY).unwrap();
        assert_eq!(store.get(Slot::ApiKey).unwrap().as_deref(), Some(KEY));
    }

    #[test]
    fn key_store_set_clears_legacy() {
        let (_d, path) = temp();
        let legacy = FakeLegacy::with(Slot::ApiKey, "sk-ant-test-OLD");
        let store = KeyStore::new(ConfigStore::new(path), legacy.clone());
        store.set(Slot::ApiKey, KEY).unwrap();
        assert!(!legacy.has(Slot::ApiKey));
        assert_eq!(store.get(Slot::ApiKey).unwrap().as_deref(), Some(KEY));
    }

    #[test]
    fn key_store_set_survives_a_failing_legacy_delete() {
        let (_d, path) = temp();
        let store = KeyStore::new(ConfigStore::new(path), FakeLegacy::failing());
        store.set(Slot::ApiKey, KEY).unwrap();
        assert_eq!(store.get(Slot::ApiKey).unwrap().as_deref(), Some(KEY));
    }

    #[test]
    fn key_store_delete_clears_both() {
        let (_d, path) = temp();
        let legacy = FakeLegacy::with(Slot::ApiKey, "sk-ant-test-OLD");
        let store = KeyStore::new(ConfigStore::new(path.clone()), legacy.clone());
        ConfigStore::new(path).set(Slot::ApiKey, KEY).unwrap();
        store.delete(Slot::ApiKey).unwrap();
        assert!(!legacy.has(Slot::ApiKey));
        assert_eq!(store.get(Slot::ApiKey).unwrap(), None);
    }

    #[test]
    fn key_store_delete_reports_first_error_but_clears_config() {
        let (_d, path) = temp();
        let store = KeyStore::new(ConfigStore::new(path.clone()), FakeLegacy::failing());
        ConfigStore::new(path.clone()).set(Slot::ApiKey, KEY).unwrap();
        assert_eq!(store.delete(Slot::ApiKey), Err(SecretError::StoreUnavailable));
        assert_eq!(ConfigStore::new(path).get(Slot::ApiKey).unwrap(), None);
    }

    #[test]
    fn broken_config_does_not_fall_back_to_legacy() {
        let (_d, path) = temp();
        std::fs::write(&path, "{ broken").unwrap();
        let legacy = FakeLegacy::with(Slot::ApiKey, "sk-ant-test-OLD");
        let store = KeyStore::new(ConfigStore::new(path), legacy);
        assert_eq!(store.get(Slot::ApiKey), Err(SecretError::ConfigUnreadable));
    }

    #[test]
    fn errors_never_contain_a_key() {
        for e in [SecretError::NotDecryptable, SecretError::ConfigUnreadable] {
            let shown = format!("{e} {e:?}");
            assert!(!shown.contains("sk-ant") && !shown.contains("NICHTECHT"));
        }
    }
}
