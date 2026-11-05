//! Self-update security core: signed release manifests (Ed25519) and version rules.
//! Network and UI live elsewhere; this module only decides "trust this installer or not".

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ed25519_dalek::{Signature, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::analytics::{Http, NetError};

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
            .map_err(|_| UpdateError::Malformed("Manifest ist ungültig".to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateError {
    BadSignature,
    ChecksumMismatch,
    NotNewer,
    /// Pre-releases are never offered to customers.
    PreRelease,
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
            Self::PreRelease => write!(f, "Vorabversionen werden nicht automatisch installiert."),
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

/// Whitelist for installer file names: `[A-Za-z0-9._-]`, 1..=100 chars, ends in `.exe`, no
/// leading `.` or `-`, no `..`, and no reserved Windows device name as stem (before the first dot).
pub fn file_name_ok(name: &str) -> bool {
    const RESERVED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    if name.is_empty() || name.len() > 100 {
        return false;
    }
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')) {
        return false;
    }
    if name.starts_with(['.', '-']) || name.contains("..") {
        return false;
    }
    if !name.to_ascii_lowercase().ends_with(".exe") {
        return false;
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix)
            .is_some_and(|n| n.len() == 1 && matches!(n.as_bytes()[0], b'1'..=b'9'))
    };
    !(RESERVED.contains(&stem.as_str()) || numbered("COM") || numbered("LPT"))
}

fn sha_format_ok(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Checks, in this order: file name, sha256 format, version and pre-release, signature,
/// installer hash.
pub fn verify(
    m: &Manifest,
    installer: &[u8],
    current: &Version,
    public_key: &[u8; 32],
) -> Result<(), UpdateError> {
    if !file_name_ok(&m.file) {
        return Err(UpdateError::BadFileName);
    }
    if !sha_format_ok(&m.sha256) {
        return Err(UpdateError::Malformed("Prüfsumme im Manifest ist ungültig".to_string()));
    }
    if !m.version.pre.is_empty() {
        return Err(UpdateError::PreRelease);
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

/// [`verify`] with an optional key; `NoKey` while none is set.
pub fn verify_with_key(
    key: Option<[u8; 32]>,
    m: &Manifest,
    installer: &[u8],
    current: &Version,
) -> Result<(), UpdateError> {
    match key {
        Some(key) => verify(m, installer, current, &key),
        None => Err(UpdateError::NoKey),
    }
}

/// [`verify`] against the key compiled into the app.
pub fn verify_with_compiled_key(
    m: &Manifest,
    installer: &[u8],
    current: &Version,
) -> Result<(), UpdateError> {
    verify_with_key(crate::update_key::RELEASE_PUBLIC_KEY, m, installer, current)
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

// ---------------------------------------------------------------------------------------------
// Network: release lookup, verified download, installer start
// ---------------------------------------------------------------------------------------------

const LATEST_URL: &str =
    "https://api.github.com/repos/Cerberus-Lab-de/cerberus-usage/releases/latest";
const DOWNLOAD_PREFIX: &str =
    "https://github.com/Cerberus-Lab-de/cerberus-usage/releases/download/";
const NOTES_PREFIX: &str = "https://github.com/Cerberus-Lab-de/cerberus-usage/";
const USER_AGENT: &str = concat!("cerberus-usage/", env!("CARGO_PKG_VERSION"));
/// Largest accepted JSON answer (GitHub API, manifest).
pub const MAX_JSON_BYTES: usize = 1024 * 1024;
/// Largest accepted installer.
pub const MAX_INSTALLER_BYTES: usize = 200 * 1024 * 1024;
const UPDATE_TIMEOUT: Duration = Duration::from_secs(60);
const INSTALLER_PREFIX: &str = "CerberusUsage-Setup-";

/// HTTP client for updates: HTTPS only, up to 5 redirects, 60 s overall, bounded bodies.
/// It never sends credentials: callers pass only the headers they name.
pub struct UpdateHttp {
    agent: ureq::Agent,
}

impl UpdateHttp {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .https_only(true)
            .max_redirects(5)
            .timeout_global(Some(UPDATE_TIMEOUT))
            .build();
        Self { agent: ureq::Agent::new_with_config(config) }
    }
}

impl Default for UpdateHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl Http for UpdateHttp {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<(u16, String), NetError> {
        let (status, bytes) = self.get_bytes(url, headers, MAX_JSON_BYTES)?;
        let text = String::from_utf8(bytes).map_err(|_| NetError("Netzwerkfehler".to_string()))?;
        Ok((status, text))
    }

    fn get_bytes(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        max_bytes: usize,
    ) -> Result<(u16, Vec<u8>), NetError> {
        let mut request = self.agent.get(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let mut response = request.call().map_err(crate::analytics::net_error)?;
        let status = response.status().as_u16();
        let bytes = response
            .body_mut()
            .with_config()
            .limit(max_bytes as u64)
            .read_to_vec()
            .map_err(|e| match e {
                ureq::Error::BodyExceedsLimit(_) => NetError("Antwort zu groß".to_string()),
                other => crate::analytics::net_error(other),
            })?;
        Ok((status, bytes))
    }
}

fn net(e: NetError) -> UpdateError {
    UpdateError::Network(e.0)
}

/// A release that is newer than the running version and carries the expected assets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub notes_url: String,
    pub installer_url: String,
    pub manifest_url: String,
}

impl Release {
    fn installer_name(&self) -> String {
        installer_asset_name(&self.version)
    }
}

fn installer_asset_name(version: &Version) -> String {
    format!("{INSTALLER_PREFIX}{version}.exe")
}

#[derive(Deserialize)]
struct ReleaseDto {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<AssetDto>,
}

#[derive(Deserialize)]
struct AssetDto {
    name: String,
    browser_download_url: String,
}

fn bad_release() -> UpdateError {
    UpdateError::Malformed("Release-Daten ungültig".to_string())
}

/// Asks GitHub for the latest release. `Ok(None)` when there is none, it is a draft or
/// pre-release, or it is not newer than `current`.
pub fn latest_release(http: &dyn Http, current: &Version) -> Result<Option<Release>, UpdateError> {
    let headers = [
        ("User-Agent", USER_AGENT),
        ("Accept", "application/vnd.github+json"),
    ];
    let (status, body) = http.get_bytes(LATEST_URL, &headers, MAX_JSON_BYTES).map_err(net)?;
    match status {
        200 => {}
        404 => return Ok(None),
        // The transport does not expose headers, so every 403/429 counts as the rate limit.
        403 | 429 => {
            return Err(UpdateError::Network(
                "GitHub-Limit erreicht, später erneut versuchen".to_string(),
            ));
        }
        code => return Err(UpdateError::Network(format!("GitHub antwortet mit HTTP {code}"))),
    }
    let dto: ReleaseDto = serde_json::from_slice(&body).map_err(|_| bad_release())?;
    if dto.draft || dto.prerelease {
        return Ok(None);
    }
    let tag = dto.tag_name.strip_prefix('v').unwrap_or(&dto.tag_name);
    let version = Version::parse(tag).map_err(|_| bad_release())?;
    if !version.pre.is_empty() || !is_newer(&version, current) {
        return Ok(None);
    }
    if !dto.html_url.starts_with(NOTES_PREFIX) {
        return Err(bad_release());
    }
    let installer_name = installer_asset_name(&version);
    let manifest_name = format!("{installer_name}.manifest.json");
    let find = |wanted: &str| {
        dto.assets
            .iter()
            .find(|a| a.name == wanted)
            .map(|a| a.browser_download_url.clone())
            .filter(|u| u.starts_with(DOWNLOAD_PREFIX))
    };
    let (Some(installer_url), Some(manifest_url)) = (find(&installer_name), find(&manifest_name))
    else {
        return Err(bad_release());
    };
    Ok(Some(Release { version, notes_url: dto.html_url, installer_url, manifest_url }))
}

/// `%TEMP%\CerberusUsage-update` below the given temp directory.
pub fn update_dir_in(temp: &Path) -> PathBuf {
    temp.join("CerberusUsage-update")
}

/// Download directory below the system temp directory.
pub fn default_update_dir() -> PathBuf {
    update_dir_in(&std::env::temp_dir())
}

fn is_old_installer(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with(&INSTALLER_PREFIX.to_ascii_lowercase())
        && (lower.ends_with(".exe") || lower.ends_with(".exe.part"))
}

fn remove_old_installers(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let is_file = entry.file_type().is_ok_and(|t| t.is_file());
        if is_file && entry.file_name().to_str().is_some_and(is_old_installer) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Downloads manifest and installer, verifies them, and only then writes the installer into
/// `dir` (temp file + rename). Returns the absolute path of the verified installer.
pub fn download_verified(
    http: &dyn Http,
    r: &Release,
    current: &Version,
    key: Option<[u8; 32]>,
    dir: &Path,
) -> Result<PathBuf, UpdateError> {
    let key = key.ok_or(UpdateError::NoKey)?;
    if !dir.is_absolute() {
        return Err(UpdateError::Malformed("Download-Ordner ungültig".to_string()));
    }
    if !r.installer_url.starts_with(DOWNLOAD_PREFIX) || !r.manifest_url.starts_with(DOWNLOAD_PREFIX)
    {
        return Err(bad_release());
    }
    let headers = [("User-Agent", USER_AGENT)];

    let (status, body) = http.get_bytes(&r.manifest_url, &headers, MAX_JSON_BYTES).map_err(net)?;
    if status != 200 {
        return Err(UpdateError::Network(format!("Manifest nicht abrufbar (HTTP {status})")));
    }
    let text = std::str::from_utf8(&body)
        .map_err(|_| UpdateError::Malformed("Manifest ist ungültig".to_string()))?;
    let manifest = Manifest::parse(text)?;
    if manifest.version != r.version || manifest.file != r.installer_name() {
        return Err(UpdateError::Malformed("Manifest passt nicht zum Release".to_string()));
    }

    let (status, installer) =
        http.get_bytes(&r.installer_url, &headers, MAX_INSTALLER_BYTES).map_err(net)?;
    if status != 200 {
        return Err(UpdateError::Network(format!("Installer nicht abrufbar (HTTP {status})")));
    }
    verify(&manifest, &installer, current, &key)?;

    // Verified; only now touch the disk. `verify` has checked the file name.
    let io_err = |_: io::Error| UpdateError::Malformed("Installer konnte nicht gespeichert werden".to_string());
    std::fs::create_dir_all(dir).map_err(io_err)?;
    remove_old_installers(dir);
    let target = dir.join(&manifest.file);
    let tmp = dir.join(format!("{}.part", manifest.file));
    let written = std::fs::write(&tmp, &installer).and_then(|()| std::fs::rename(&tmp, &target));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(io_err(e));
    }
    Ok(target)
}

/// Silent installer switches; `/UPDATE=1` tells the setup that this is an update.
pub fn installer_args() -> [&'static str; 5] {
    ["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/CLOSEAPPLICATIONS", "/UPDATE=1"]
}

/// Starts the verified installer (no shell, fixed arguments) and does not wait for it.
/// Only for paths returned by [`download_verified`]; refuses relative paths and bad names.
pub fn run_installer(path: &Path) -> io::Result<()> {
    let name_ok = path.file_name().and_then(|n| n.to_str()).is_some_and(file_name_ok);
    if !path.is_absolute() || !name_ok {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "ungültiger Installer-Pfad"));
    }
    std::process::Command::new(path).args(installer_args()).spawn().map(|_| ())
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
        let m = signed("1.0.1", "setup.exe");
        assert_eq!(verify_with_key(None, &m, INSTALLER, &v("1.0.0")), Err(UpdateError::NoKey));
        assert_eq!(
            verify_with_key(Some(pk(&key(1))), &m, INSTALLER, &v("1.0.0")),
            Ok(())
        );
    }

    #[test]
    fn file_name_whitelist() {
        for bad in [
            "NUL.exe", "aux.exe", "COM1.exe", "con.setup.exe", "lpt9.exe", "a b.exe", "a\n.exe",
            "a*.exe", "..exe", ".exe", "a.exe.", "-a.exe", "ä.exe", "a.txt",
            &format!("{}.exe", "a".repeat(97)),
        ] {
            assert!(!file_name_ok(bad), "{bad:?}");
        }
        assert!(!file_name_ok(&"a".repeat(101)));
        assert!(file_name_ok("CerberusUsage-Setup-1.0.1.exe"));
        assert!(file_name_ok("COM0.exe") && file_name_ok("console.exe"));
        assert!(file_name_ok(&format!("{}.exe", "a".repeat(96))));
    }

    #[test]
    fn verify_rejects_bad_sha_format_before_signature() {
        let m = signed("1.0.1", "setup.exe");
        for bad in [m.sha256.to_uppercase(), m.sha256[..63].to_string(), format!("{}\n", m.sha256)] {
            let mut x = m.clone();
            x.sha256 = bad;
            assert!(matches!(
                verify(&x, INSTALLER, &v("1.0.0"), &pk(&key(1))),
                Err(UpdateError::Malformed(_))
            ));
        }
    }

    #[test]
    fn verify_rejects_prerelease_even_with_valid_signature() {
        let m = signed("1.1.0-rc.1", "setup.exe");
        assert_eq!(
            verify(&m, INSTALLER, &v("1.0.0"), &pk(&key(1))),
            Err(UpdateError::PreRelease)
        );
    }

    #[test]
    fn signature_over_other_version_or_file_name_is_bad() {
        let m = signed("1.0.1", "setup.exe");
        let mut other_version = m.clone();
        other_version.version = v("1.0.2");
        assert_eq!(
            verify(&other_version, INSTALLER, &v("1.0.0"), &pk(&key(1))),
            Err(UpdateError::BadSignature)
        );
        let mut other_file = m;
        other_file.file = "other.exe".to_string();
        assert_eq!(
            verify(&other_file, INSTALLER, &v("1.0.0"), &pk(&key(1))),
            Err(UpdateError::BadSignature)
        );
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

    // ----- network tests (fakes only)

    use std::collections::HashMap;

    struct Fake {
        responses: HashMap<String, (u16, Vec<u8>)>,
        calls: std::cell::RefCell<Vec<String>>,
    }

    impl Fake {
        fn new() -> Self {
            Self { responses: HashMap::new(), calls: Default::default() }
        }
        fn with(mut self, url: &str, status: u16, body: &[u8]) -> Self {
            self.responses.insert(url.to_string(), (status, body.to_vec()));
            self
        }
    }

    impl Http for Fake {
        fn get(&self, url: &str, _h: &[(&str, &str)]) -> Result<(u16, String), NetError> {
            let (s, b) = self.get_bytes(url, _h, usize::MAX)?;
            Ok((s, String::from_utf8(b).unwrap()))
        }
        fn get_bytes(
            &self,
            url: &str,
            _h: &[(&str, &str)],
            max: usize,
        ) -> Result<(u16, Vec<u8>), NetError> {
            self.calls.borrow_mut().push(url.to_string());
            match self.responses.get(url) {
                Some((s, b)) if b.len() <= max => Ok((*s, b.clone())),
                Some(_) => Err(NetError("Antwort zu groß".into())),
                None => Err(NetError("Server nicht erreichbar".into())),
            }
        }
    }

    const DL: &str = "https://github.com/Cerberus-Lab-de/cerberus-usage/releases/download/v1.0.1/";

    fn release_json(tag: &str, draft: bool, pre: bool, assets: &[(&str, String)]) -> Vec<u8> {
        let assets: Vec<_> = assets
            .iter()
            .map(|(n, u)| serde_json::json!({"name": n, "browser_download_url": u, "extra": 1}))
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "tag_name": tag, "html_url": "https://github.com/Cerberus-Lab-de/cerberus-usage/releases/tag/x",
            "draft": draft, "prerelease": pre, "assets": assets, "body": "ignored"
        }))
        .unwrap()
    }

    fn good_assets(ver: &str) -> Vec<(&'static str, String)> {
        let exe: &'static str = Box::leak(format!("CerberusUsage-Setup-{ver}.exe").into_boxed_str());
        let man: &'static str =
            Box::leak(format!("CerberusUsage-Setup-{ver}.exe.manifest.json").into_boxed_str());
        vec![(exe, format!("{DL}{exe}")), (man, format!("{DL}{man}"))]
    }

    fn lookup(body: Vec<u8>, status: u16, current: &str) -> Result<Option<Release>, UpdateError> {
        let http = Fake::new().with(LATEST_URL, status, &body);
        latest_release(&http, &v(current))
    }

    #[test]
    fn latest_release_parses_assets_and_skips_prerelease() {
        let ok = release_json("v1.0.1", false, false, &good_assets("1.0.1"));
        let r = lookup(ok, 200, "1.0.0").unwrap().unwrap();
        assert_eq!(r.version, v("1.0.1"));
        assert_eq!(r.installer_url, format!("{DL}CerberusUsage-Setup-1.0.1.exe"));
        assert!(r.manifest_url.ends_with(".exe.manifest.json"));
        // tag without v works too
        let no_v = release_json("1.0.1", false, false, &good_assets("1.0.1"));
        assert!(lookup(no_v, 200, "1.0.0").unwrap().is_some());
        // pre-release, draft, not newer
        let pre = release_json("v1.0.1", false, true, &good_assets("1.0.1"));
        assert_eq!(lookup(pre, 200, "1.0.0"), Ok(None));
        let draft = release_json("v1.0.1", true, false, &good_assets("1.0.1"));
        assert_eq!(lookup(draft, 200, "1.0.0"), Ok(None));
        let same = release_json("v1.0.0", false, false, &good_assets("1.0.0"));
        assert_eq!(lookup(same, 200, "1.0.0"), Ok(None));
        let rc_tag = release_json("v1.1.0-rc.1", false, false, &good_assets("1.1.0-rc.1"));
        assert_eq!(lookup(rc_tag, 200, "1.0.0"), Ok(None));
    }

    #[test]
    fn latest_release_rejects_wrong_assets_and_hosts() {
        let wrong_ver = release_json("v1.0.1", false, false, &good_assets("1.0.2"));
        assert!(matches!(lookup(wrong_ver, 200, "1.0.0"), Err(UpdateError::Malformed(_))));
        let mut evil = good_assets("1.0.1");
        evil[0].1 = "https://evil.example/CerberusUsage-Setup-1.0.1.exe".to_string();
        let evil = release_json("v1.0.1", false, false, &evil);
        assert!(matches!(lookup(evil, 200, "1.0.0"), Err(UpdateError::Malformed(_))));
        let missing = release_json("v1.0.1", false, false, &good_assets("1.0.1")[..1]);
        assert!(matches!(lookup(missing, 200, "1.0.0"), Err(UpdateError::Malformed(_))));
        assert!(matches!(lookup(b"nope".to_vec(), 200, "1.0.0"), Err(UpdateError::Malformed(_))));
        let bad_tag = release_json("latest", false, false, &[]);
        assert!(matches!(lookup(bad_tag, 200, "1.0.0"), Err(UpdateError::Malformed(_))));
    }

    #[test]
    fn latest_release_none_on_404() {
        assert_eq!(lookup(b"{}".to_vec(), 404, "1.0.0"), Ok(None));
    }

    #[test]
    fn latest_release_rate_limited_is_network_error() {
        for code in [403, 429] {
            match lookup(b"{}".to_vec(), code, "1.0.0") {
                Err(UpdateError::Network(m)) => assert!(m.contains("GitHub-Limit")),
                other => panic!("{other:?}"),
            }
        }
        assert!(matches!(lookup(b"{}".to_vec(), 500, "1.0.0"), Err(UpdateError::Network(_))));
    }

    #[test]
    fn latest_release_oversized_body_is_network_error() {
        let big = vec![b' '; MAX_JSON_BYTES + 1];
        assert!(matches!(lookup(big, 200, "1.0.0"), Err(UpdateError::Network(_))));
    }

    // download fixtures
    struct Dl {
        http: Fake,
        release: Release,
    }

    fn dl(version: &str, manifest: Option<Manifest>, installer: &[u8]) -> Dl {
        let name = format!("CerberusUsage-Setup-{version}.exe");
        let manifest = manifest.unwrap_or_else(|| signed(version, &name));
        let release = Release {
            version: v(version),
            notes_url: "https://github.com/Cerberus-Lab-de/cerberus-usage/releases/tag/x".into(),
            installer_url: format!("{DL}{name}"),
            manifest_url: format!("{DL}{name}.manifest.json"),
        };
        let http = Fake::new()
            .with(&release.manifest_url, 200, &serde_json::to_vec(&manifest).unwrap())
            .with(&release.installer_url, 200, installer);
        Dl { http, release }
    }

    fn files_in(dir: &Path) -> Vec<String> {
        match std::fs::read_dir(dir) {
            Ok(rd) => rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect(),
            Err(_) => vec![],
        }
    }

    #[test]
    fn download_verified_writes_verified_installer_and_cleans_old_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = update_dir_in(tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("CerberusUsage-Setup-0.9.0.exe"), b"old").unwrap();
        std::fs::write(dir.join("other.txt"), b"keep").unwrap();
        let d = dl("1.0.1", None, INSTALLER);
        let path =
            download_verified(&d.http, &d.release, &v("1.0.0"), Some(pk(&key(1))), &dir).unwrap();
        assert_eq!(path, dir.join("CerberusUsage-Setup-1.0.1.exe"));
        assert_eq!(std::fs::read(&path).unwrap(), INSTALLER);
        let mut files = files_in(&dir);
        files.sort();
        assert_eq!(files, vec!["CerberusUsage-Setup-1.0.1.exe", "other.txt"]);
    }

    #[test]
    fn download_verified_writes_only_after_verification() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = update_dir_in(tmp.path());
        // Bad signature (other key), tampered installer, bad file name in manifest.
        let d = dl("1.0.1", None, INSTALLER);
        assert_eq!(
            download_verified(&d.http, &d.release, &v("1.0.0"), Some(pk(&key(2))), &dir),
            Err(UpdateError::BadSignature)
        );
        let d = dl("1.0.1", None, b"tampered");
        assert_eq!(
            download_verified(&d.http, &d.release, &v("1.0.0"), Some(pk(&key(1))), &dir),
            Err(UpdateError::ChecksumMismatch)
        );
        assert!(!dir.exists(), "nothing may be created before verification");
    }

    #[test]
    fn download_verified_rejects_downgrade() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = update_dir_in(tmp.path());
        let d = dl("1.0.1", None, INSTALLER);
        assert_eq!(
            download_verified(&d.http, &d.release, &v("1.0.1"), Some(pk(&key(1))), &dir),
            Err(UpdateError::NotNewer)
        );
        assert_eq!(
            download_verified(&d.http, &d.release, &v("2.0.0"), Some(pk(&key(1))), &dir),
            Err(UpdateError::NotNewer)
        );
        assert!(!dir.exists());
    }

    #[test]
    fn download_verified_without_key_makes_no_request() {
        let tmp = tempfile::tempdir().unwrap();
        let d = dl("1.0.1", None, INSTALLER);
        assert_eq!(
            download_verified(&d.http, &d.release, &v("1.0.0"), None, tmp.path()),
            Err(UpdateError::NoKey)
        );
        assert!(d.http.calls.borrow().is_empty());
    }

    #[test]
    fn download_verified_rejects_manifest_mismatch_and_skips_installer() {
        let tmp = tempfile::tempdir().unwrap();
        // Manifest for another version / another file name, both validly signed.
        for m in [
            signed("1.0.2", "CerberusUsage-Setup-1.0.2.exe"),
            signed("1.0.1", "evil.exe"),
        ] {
            let d = dl("1.0.1", Some(m), INSTALLER);
            let r = download_verified(&d.http, &d.release, &v("1.0.0"), Some(pk(&key(1))), tmp.path());
            assert!(matches!(r, Err(UpdateError::Malformed(_))));
            assert_eq!(d.http.calls.borrow().len(), 1, "installer must not be fetched");
        }
    }

    #[test]
    fn download_verified_rejects_relative_dir_foreign_url_and_oversized_manifest() {
        let d = dl("1.0.1", None, INSTALLER);
        assert!(download_verified(&d.http, &d.release, &v("1.0.0"), Some(pk(&key(1))), Path::new("rel"))
            .is_err());
        let tmp = tempfile::tempdir().unwrap();
        let mut r = d.release.clone();
        r.installer_url = "https://evil.example/x.exe".into();
        assert!(download_verified(&d.http, &r, &v("1.0.0"), Some(pk(&key(1))), tmp.path()).is_err());
        let http = Fake::new().with(&d.release.manifest_url, 200, &vec![b' '; MAX_JSON_BYTES + 1]);
        assert!(matches!(
            download_verified(&http, &d.release, &v("1.0.0"), Some(pk(&key(1))), tmp.path()),
            Err(UpdateError::Network(_))
        ));
    }

    #[test]
    fn installer_args_and_dirs() {
        assert_eq!(
            installer_args(),
            ["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/CLOSEAPPLICATIONS", "/UPDATE=1"]
        );
        let d = update_dir_in(Path::new("C:\\Temp"));
        assert!(d.ends_with("CerberusUsage-update"));
        assert!(default_update_dir().is_absolute());
    }

    #[test]
    fn run_installer_refuses_relative_or_bad_paths_without_spawning() {
        assert!(run_installer(Path::new("setup.exe")).is_err());
        let tmp = tempfile::tempdir().unwrap();
        assert!(run_installer(&tmp.path().join("NUL.exe")).is_err());
        assert!(run_installer(&tmp.path().join("setup.bat")).is_err());
    }
}
