//! API keys in the Windows credential store (via `keyring` 4).
//!
//! Keys never appear in errors: `SecretError` carries no payload, and the underlying
//! `keyring` errors (some of which embed the raw secret bytes) are mapped, never forwarded.

use std::fmt;

use crate::i18n::tr;

/// Service name under which all entries are stored.
const SERVICE: &str = "cerberus-usage";

/// Which key an entry holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// Normal API key, passed to Obsidian in API-key mode.
    ApiKey,
    /// Admin key, used for the usage/cost analytics.
    AdminKey,
}

impl Slot {
    fn entry_name(self) -> &'static str {
        match self {
            Slot::ApiKey => "anthropic-api-key",
            Slot::AdminKey => "anthropic-admin-key",
        }
    }
}

/// Store failure. Deliberately free of any secret value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretError {
    /// The credential store could not be initialised or reached.
    StoreUnavailable,
    /// The stored value could not be read in the expected format.
    BadData,
    /// A key in settings.json cannot be decrypted (other user or machine, or damaged).
    NotDecryptable,
    /// settings.json is unreadable or broken, so keys can be neither read nor saved.
    ConfigUnreadable,
    /// Any other store failure.
    Other,
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&tr(match self {
            SecretError::StoreUnavailable => "secrets.error.store_unavailable",
            SecretError::BadData => "secrets.error.bad_data",
            SecretError::NotDecryptable => "secrets.error.not_decryptable",
            SecretError::ConfigUnreadable => "secrets.error.config_unreadable",
            SecretError::Other => "secrets.error.other",
        }))
    }
}

impl std::error::Error for SecretError {}

pub trait SecretStore {
    /// `Ok(None)` when no entry exists.
    fn get(&self, s: Slot) -> Result<Option<String>, SecretError>;
    fn set(&self, s: Slot, v: &str) -> Result<(), SecretError>;
    /// Deleting a missing entry is not an error.
    fn delete(&self, s: Slot) -> Result<(), SecretError>;
}

/// Windows Credential Manager. `keyring::Entry::new` registers the platform store
/// on first use, so callers need no extra setup.
pub struct KeyringStore {
    service: String,
}

impl KeyringStore {
    pub fn new() -> Self {
        Self { service: SERVICE.to_string() }
    }

    /// Same store under a different service name (used by the ignored round-trip test).
    pub fn with_service(service: &str) -> Self {
        Self { service: service.to_string() }
    }

    fn entry(&self, s: Slot) -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(&self.service, s.entry_name()).map_err(map_error)
    }
}

impl Default for KeyringStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Map without forwarding payloads: `BadEncoding`/`BadDataFormat` hold secret bytes.
fn map_error(e: keyring::Error) -> SecretError {
    match e {
        keyring::Error::NoDefaultStore | keyring::Error::NoStorageAccess(_) => {
            SecretError::StoreUnavailable
        }
        keyring::Error::BadEncoding(_) | keyring::Error::BadDataFormat(..) => SecretError::BadData,
        _ => SecretError::Other,
    }
}

impl SecretStore for KeyringStore {
    fn get(&self, s: Slot) -> Result<Option<String>, SecretError> {
        match self.entry(s)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(map_error(e)),
        }
    }

    fn set(&self, s: Slot, v: &str) -> Result<(), SecretError> {
        self.entry(s)?.set_password(v).map_err(map_error)
    }

    fn delete(&self, s: Slot) -> Result<(), SecretError> {
        match self.entry(s)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(map_error(e)),
        }
    }
}

/// Format check only (no network). Returned messages are fixed strings, never the value.
pub fn validate_key(s: Slot, v: &str) -> Result<(), String> {
    // Control characters too: a NUL would make the process start fail after Obsidian was already closed.
    if v.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(tr("secrets.key.whitespace"));
    }
    match s {
        Slot::ApiKey => {
            if v.starts_with("sk-ant-admin") {
                return Err(tr("secrets.key.is_admin"));
            }
            if !v.starts_with("sk-ant-") || v.len() <= "sk-ant-".len() {
                return Err(tr("secrets.key.api_prefix"));
            }
        }
        Slot::AdminKey => {
            const PREFIX: &str = "sk-ant-admin01-";
            if !v.starts_with(PREFIX) || v.len() <= PREFIX.len() {
                return Err(tr("secrets.key.admin_prefix"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE_API: &str = "sk-ant-test-NICHTECHT-1234";
    const FAKE_ADMIN: &str = "sk-ant-admin01-NICHTECHT-1234";

    #[test]
    fn validate_accepts_well_formed_keys() {
        assert!(validate_key(Slot::ApiKey, FAKE_API).is_ok());
        assert!(validate_key(Slot::AdminKey, FAKE_ADMIN).is_ok());
    }

    #[test]
    fn validate_rejects_admin_key_in_api_slot() {
        assert!(validate_key(Slot::ApiKey, FAKE_ADMIN).is_err());
    }

    #[test]
    fn validate_rejects_api_key_in_admin_slot() {
        assert!(validate_key(Slot::AdminKey, FAKE_API).is_err());
    }

    #[test]
    fn validate_rejects_whitespace() {
        assert!(validate_key(Slot::ApiKey, "sk-ant-test NICHTECHT").is_err());
        assert!(validate_key(Slot::ApiKey, " sk-ant-test-NICHTECHT").is_err());
        assert!(validate_key(Slot::AdminKey, "sk-ant-admin01-NICHT\tECHT").is_err());
        assert!(validate_key(Slot::ApiKey, "sk-ant-test-NICHTECHT\n").is_err());
    }

    #[test]
    fn validate_rejects_control_characters() {
        for bad in [
            "sk-ant-test-NICHT\0ECHT",
            "sk-ant-test-NICHT\x1bECHT",
            "sk-ant-test-NICHT\x7fECHT",
        ] {
            assert!(validate_key(Slot::ApiKey, bad).is_err(), "accepted {bad:?}");
        }
        assert!(validate_key(Slot::AdminKey, "sk-ant-admin01-NICHT\0ECHT").is_err());
    }

    #[test]
    fn validate_rejects_empty_and_prefix_only() {
        assert!(validate_key(Slot::ApiKey, "").is_err());
        assert!(validate_key(Slot::ApiKey, "sk-ant-").is_err());
        assert!(validate_key(Slot::AdminKey, "sk-ant-admin01-").is_err());
    }

    #[test]
    fn error_strings_never_contain_key() {
        let candidates = [
            (Slot::ApiKey, FAKE_ADMIN),
            (Slot::ApiKey, "sk-ant-test NICHTECHT-1234"),
            (Slot::AdminKey, FAKE_API),
            (Slot::ApiKey, "sk-ant-"),
            (Slot::ApiKey, "NICHTECHT-1234"),
        ];
        for (slot, value) in candidates {
            let msg = validate_key(slot, value).unwrap_err();
            assert!(!msg.contains("NICHTECHT"), "validate error leaked value: {msg}");
        }
        // Store errors carry no payload, even when built from a keyring error holding the bytes.
        let leaky = [
            keyring::Error::BadEncoding(FAKE_API.as_bytes().to_vec()),
            keyring::Error::NoDefaultStore,
            keyring::Error::Invalid(FAKE_API.to_string(), FAKE_API.to_string()),
            keyring::Error::TooLong(FAKE_API.to_string(), 1),
        ];
        for e in leaky {
            let mapped = map_error(e);
            let shown = format!("{mapped} {mapped:?}");
            assert!(!shown.contains("NICHTECHT") && !shown.contains("1234"), "leaked: {shown}");
        }
    }

    /// Touches the real Windows credential store. Run manually:
    /// `cargo test keyring_roundtrip -- --ignored`
    #[test]
    #[ignore]
    fn keyring_roundtrip() {
        let store = KeyringStore::with_service("cerberus-usage-test");
        // Clean slate in case an earlier run aborted.
        store.delete(Slot::ApiKey).unwrap();
        assert_eq!(store.get(Slot::ApiKey).unwrap(), None);

        store.set(Slot::ApiKey, FAKE_API).unwrap();
        assert_eq!(store.get(Slot::ApiKey).unwrap().as_deref(), Some(FAKE_API));
        // Slots are independent entries.
        assert_eq!(store.get(Slot::AdminKey).unwrap(), None);

        store.delete(Slot::ApiKey).unwrap();
        assert_eq!(store.get(Slot::ApiKey).unwrap(), None);
        // Deleting again is not an error.
        store.delete(Slot::ApiKey).unwrap();
    }
}
