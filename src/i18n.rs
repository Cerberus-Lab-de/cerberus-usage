//! Interface texts: language files, lookup and fallback.
//!
//! Texts live in flat JSON files (`lang/*.json`). German and English are built
//! in; further files are read from the `lang` folder next to the executable.
//! Lookup order: chosen language (folder file, then built-in), then English,
//! then the key name itself. The state is process-wide and thread-safe so
//! worker threads can translate too. Nothing here touches the disk per lookup.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError, RwLock};

#[cfg(test)]
use std::cell::RefCell;

const DE_JSON: &str = include_str!("../lang/de.json");
const EN_JSON: &str = include_str!("../lang/en.json");

/// Largest language file that is accepted, in bytes.
const MAX_FILE_BYTES: u64 = 1_048_576;
const ENGLISH: &str = "en";

/// One entry of the language choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LangInfo {
    pub code: String,
    pub name: String,
}

/// A file in the language folder that was skipped, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LangProblem {
    pub file: String,
    pub reason: String,
}

/// Why a language file was skipped: the key of the reason text and its placeholders. It is turned
/// into text only after a scan has stored the files, because the built-in languages are parsed
/// while the state itself is being built.
struct Reason {
    key: &'static str,
    args: Vec<(&'static str, String)>,
}

impl Reason {
    fn new(key: &'static str) -> Self {
        Reason { key, args: Vec::new() }
    }

    fn with(key: &'static str, name: &'static str, value: impl Into<String>) -> Self {
        Reason { key, args: vec![(name, value.into())] }
    }

    fn text(&self) -> String {
        let args: Vec<(&str, &str)> = self.args.iter().map(|(k, v)| (*k, v.as_str())).collect();
        tr_args(self.key, &args)
    }
}

struct Lang {
    code: String,
    name: String,
    texts: HashMap<String, String>,
}

struct State {
    /// Built-in languages, keyed by lowercase code.
    builtin: HashMap<String, Lang>,
    /// Languages loaded from the folder, keyed by lowercase code.
    folder: HashMap<String, Lang>,
    /// Code of the language in use, spelled as in its file.
    chosen: String,
    /// What the user asked for; resolved again after every scan, because the
    /// file may appear, break or be repaired later.
    requested: Option<String>,
}

impl State {
    fn new() -> Self {
        let mut builtin = HashMap::new();
        for raw in [DE_JSON, EN_JSON] {
            if let Ok(lang) = parse_lang(raw.as_bytes()) {
                builtin.insert(lang.code.to_ascii_lowercase(), lang);
            }
        }
        let mut state = State { builtin, folder: HashMap::new(), chosen: ENGLISH.to_string(), requested: None };
        // Existing tests assert German texts, so tests start on built-in German.
        state.chosen = if cfg!(test) { "de".to_string() } else { default_language(&state) };
        state
    }
}

static GLOBAL: LazyLock<RwLock<State>> = LazyLock::new(|| RwLock::new(State::new()));

#[cfg(test)]
thread_local! {
    /// Per-thread state so parallel tests can load their own folders.
    static LOCAL: RefCell<Option<State>> = const { RefCell::new(None) };
    /// Per-thread language override, wins over the chosen language.
    static OVERRIDE: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn read<R>(f: impl FnOnce(&State) -> R) -> R {
    #[cfg(test)]
    if LOCAL.with(|l| l.borrow().is_some()) {
        return LOCAL.with(|l| f(l.borrow().as_ref().expect("checked above")));
    }
    let guard = GLOBAL.read().unwrap_or_else(PoisonError::into_inner);
    f(&guard)
}

fn write<R>(f: impl FnOnce(&mut State) -> R) -> R {
    #[cfg(test)]
    if LOCAL.with(|l| l.borrow().is_some()) {
        return LOCAL.with(|l| f(l.borrow_mut().as_mut().expect("checked above")));
    }
    let mut guard = GLOBAL.write().unwrap_or_else(PoisonError::into_inner);
    f(&mut guard)
}

/// Gives the current thread its own language state (built-ins only, German).
#[cfg(test)]
pub fn isolate() {
    LOCAL.with(|l| *l.borrow_mut() = Some(State::new()));
}

/// Runs `f` with the language forced to `code` on this thread only.
#[cfg(test)]
pub fn with_language<R>(code: &str, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OVERRIDE.with(|o| *o.borrow_mut() = self.0.take());
        }
    }
    let previous = OVERRIDE.with(|o| o.borrow_mut().replace(code.to_string()));
    let _restore = Restore(previous);
    f()
}

fn effective_code(state: &State) -> String {
    #[cfg(test)]
    if let Some(code) = OVERRIDE.with(|o| o.borrow().clone()) {
        return code;
    }
    state.chosen.clone()
}

// ---------------------------------------------------------------------------
// Parsing and loading

fn valid_code(code: &str) -> bool {
    let (primary, rest) = match code.split_once('-') {
        Some((p, r)) => (p, Some(r)),
        None => (code, None),
    };
    let primary_ok = (2..=3).contains(&primary.len()) && primary.bytes().all(|b| b.is_ascii_alphabetic());
    let rest_ok = rest.is_none_or(|r| (2..=8).contains(&r.len()) && r.bytes().all(|b| b.is_ascii_alphanumeric()));
    primary_ok && rest_ok
}

fn parse_lang(bytes: &[u8]) -> Result<Lang, Reason> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let text = std::str::from_utf8(bytes).map_err(|_| Reason::new("lang.reason.not_utf8"))?;
    // The parser's own wording is a technical detail and stays as it is.
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(text)
        .map_err(|e| Reason::with("lang.reason.invalid_json", "detail", e.to_string()))?;
    let meta = map
        .get("_meta")
        .and_then(|m| m.as_object())
        .ok_or_else(|| Reason::new("lang.reason.no_meta"))?;
    if meta.get("format").and_then(|f| f.as_u64()) != Some(1) {
        return Err(Reason::new("lang.reason.bad_format"));
    }
    let code = meta
        .get("code")
        .and_then(|c| c.as_str())
        .filter(|c| valid_code(c))
        .ok_or_else(|| Reason::new("lang.reason.bad_code"))?;
    let name = meta
        .get("name")
        .and_then(|n| n.as_str())
        .filter(|n| !n.trim().is_empty())
        .ok_or_else(|| Reason::new("lang.reason.no_name"))?;
    let mut texts = HashMap::new();
    for (key, value) in &map {
        if key == "_meta" {
            continue;
        }
        let Some(s) = value.as_str() else {
            return Err(Reason::with("lang.reason.not_string", "key", key.as_str()));
        };
        texts.insert(key.clone(), s.to_string());
    }
    Ok(Lang { code: code.to_string(), name: name.to_string(), texts })
}

fn load_file(path: &Path) -> Result<Lang, Reason> {
    let unreadable = |e: std::io::Error| Reason::with("lang.reason.unreadable", "kind", e.kind().to_string());
    let file = std::fs::File::open(path).map_err(unreadable)?;
    let mut bytes = Vec::new();
    // Read one byte past the limit so an oversized file is never read in full.
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).map_err(unreadable)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(Reason::new("lang.reason.too_large"));
    }
    parse_lang(&bytes)
}

/// Reads all `*.json` files of `dir`, replaces the folder-loaded languages and
/// returns the files that were skipped. Built-in languages stay in place. A
/// missing folder is not a problem.
pub fn scan_folder(dir: &Path) -> Vec<LangProblem> {
    let mut problems: Vec<(String, Reason)> = Vec::new();
    let mut loaded: HashMap<String, (String, Lang)> = HashMap::new();

    match std::fs::read_dir(dir) {
        Ok(entries) => {
            let mut files: Vec<(String, PathBuf)> = entries
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .filter(|e| {
                    e.path()
                        .extension()
                        .is_some_and(|x| x.eq_ignore_ascii_case("json"))
                })
                .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
                .collect();
            files.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()).then(a.0.cmp(&b.0)));
            for (name, path) in files {
                match load_file(&path) {
                    Ok(lang) => {
                        let key = lang.code.to_ascii_lowercase();
                        if let Some((first, _)) = loaded.get(&key) {
                            let reason = Reason {
                                key: "lang.reason.duplicate",
                                args: vec![("code", lang.code.clone()), ("first", first.clone())],
                            };
                            problems.push((name, reason));
                        } else {
                            loaded.insert(key, (name, lang));
                        }
                    }
                    Err(reason) => problems.push((name, reason)),
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => problems.push((
            dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            Reason::with("lang.reason.folder_unreadable", "kind", e.kind().to_string()),
        )),
    }

    write(|state| {
        state.folder = loaded.into_iter().map(|(k, (_, lang))| (k, lang)).collect();
        state.chosen = resolve_choice(state);
    });
    // Worded after the files are stored, so a reason already follows a language that was just loaded.
    problems.into_iter().map(|(file, reason)| LangProblem { file, reason: reason.text() }).collect()
}

/// Problems of the start-up scan, kept for the window (which shows them in the settings).
static STARTUP_PROBLEMS: Mutex<Vec<LangProblem>> = Mutex::new(Vec::new());

/// Picks the language for the start: the saved choice (or the system language), then the language
/// folder. A choice without a matching file falls back to the default language; nothing is written,
/// so the choice comes back when the file does. Returns the files that were skipped.
pub fn apply_startup(language: Option<&str>, dir: Option<&Path>) -> Vec<LangProblem> {
    // The choice first, so the reasons of skipped files are worded in it where it is built in.
    set_language(language);
    dir.map(scan_folder).unwrap_or_default()
}

/// Applies the language from the saved settings before the first dialog can appear. Read only: a
/// missing or broken settings file simply leaves the system language.
pub fn apply_saved_language() {
    let language = crate::settings::default_path().ok().and_then(|path| crate::settings::load(&path).0.language);
    let problems = apply_startup(language.as_deref(), app_lang_dir().as_deref());
    *STARTUP_PROBLEMS.lock().unwrap_or_else(PoisonError::into_inner) = problems;
}

/// The skipped files of [`apply_saved_language`], handed over once.
pub fn take_startup_problems() -> Vec<LangProblem> {
    std::mem::take(&mut *STARTUP_PROBLEMS.lock().unwrap_or_else(PoisonError::into_inner))
}

/// The `lang` folder next to the running executable (it may not exist).
pub fn app_lang_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("lang"))
}

// ---------------------------------------------------------------------------
// Language choice

fn list(state: &State) -> Vec<LangInfo> {
    let mut by_code: HashMap<&String, &Lang> = HashMap::new();
    for (key, lang) in &state.builtin {
        by_code.insert(key, lang);
    }
    // A folder file replaces the built-in entry of the same code.
    for (key, lang) in &state.folder {
        by_code.insert(key, lang);
    }
    let mut out: Vec<LangInfo> = by_code
        .values()
        .map(|l| LangInfo { code: l.code.clone(), name: l.name.clone() })
        .collect();
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.code.cmp(&b.code)));
    out
}

/// All languages that can be chosen, sorted by name.
pub fn available() -> Vec<LangInfo> {
    read(list)
}

/// Locale names for the Windows UI language, most specific first ("de", "pt-BR", "pt").
fn windows_candidates() -> Vec<String> {
    use windows_sys::Win32::Globalization::{GetUserDefaultUILanguage, LCIDToLocaleName};
    // SAFETY: plain query without arguments.
    let id = unsafe { GetUserDefaultUILanguage() };
    if id & 0x3ff == 0x07 {
        return vec!["de".to_string()];
    }
    let mut buf = [0u16; 85];
    // SAFETY: the buffer is valid for `buf.len()` UTF-16 units.
    let n = unsafe { LCIDToLocaleName(u32::from(id), buf.as_mut_ptr(), buf.len() as i32, 0) };
    if n <= 1 {
        return Vec::new();
    }
    let name = String::from_utf16_lossy(&buf[..(n - 1) as usize]);
    let mut out = vec![name.clone()];
    if let Some((primary, _)) = name.split_once('-') {
        out.push(primary.to_string());
    }
    out
}

/// Windows UI language if a language of that code exists, else English.
fn default_language(state: &State) -> String {
    let langs = list(state);
    for candidate in windows_candidates() {
        if let Some(found) = langs.iter().find(|l| l.code.eq_ignore_ascii_case(&candidate)) {
            return found.code.clone();
        }
    }
    ENGLISH.to_string()
}

/// The requested language if it exists, else the default language.
fn resolve_choice(state: &State) -> String {
    let wanted = state.requested.as_deref().and_then(|c| {
        list(state).into_iter().find(|l| l.code.eq_ignore_ascii_case(c)).map(|l| l.code)
    });
    wanted.unwrap_or_else(|| default_language(state))
}

/// Chooses the language. `None` or an unknown code means: Windows UI language
/// if available, else English.
pub fn set_language(code: Option<&str>) {
    write(|state| {
        state.requested = code.map(str::to_string);
        state.chosen = resolve_choice(state);
    });
}

/// Code of the language in use.
pub fn current_code() -> String {
    read(effective_code)
}

// ---------------------------------------------------------------------------
// Lookup

fn lookup_in(state: &State, code: &str, keys: &[&str]) -> Option<String> {
    let lc = code.to_ascii_lowercase();
    for layer in [state.folder.get(&lc), state.builtin.get(&lc)].into_iter().flatten() {
        for key in keys {
            if let Some(text) = layer.texts.get(*key) {
                return Some(text.clone());
            }
        }
    }
    None
}

/// Chosen language first, then English.
fn resolve(state: &State, keys: &[&str]) -> Option<String> {
    lookup_in(state, &effective_code(state), keys).or_else(|| lookup_in(state, ENGLISH, keys))
}

/// Replaces `{name}` placeholders; unknown ones stay as written. Substituted
/// values are not expanded again.
fn fill(text: &str, args: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                match args.iter().find(|(k, _)| *k == name) {
                    Some((_, value)) => out.push_str(value),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Text for `key`, or the key itself when no language has it.
pub fn tr(key: &str) -> String {
    read(|s| resolve(s, &[key])).unwrap_or_else(|| key.to_string())
}

/// Like [`tr`], with `{name}` placeholders filled from `args`.
pub fn tr_args(key: &str, args: &[(&str, &str)]) -> String {
    fill(&tr(key), args)
}

/// Plural-aware text: `<key>.one` for `n == 1`, `<key>.other` otherwise (and as
/// fallback). `{n}` is filled from `n`.
pub fn tr_n(key: &str, n: u64, args: &[(&str, &str)]) -> String {
    let one = format!("{key}.one");
    let other = format!("{key}.other");
    let found = read(|s| {
        if n == 1 {
            resolve(s, &[one.as_str(), other.as_str()])
        } else {
            resolve(s, &[other.as_str()])
        }
    });
    let text = found.unwrap_or_else(|| key.to_string());
    let n_text = n.to_string();
    let mut all: Vec<(&str, &str)> = args.to_vec();
    all.push(("n", n_text.as_str()));
    fill(&text, &all)
}

/// Like [`tr_args`] for code that must work when the language state is unusable (crash box).
/// It never waits for the state lock, which the panicking thread itself may be holding, and a
/// poisoned lock is read anyway. When no text is available it uses `fallback`.
pub fn tr_args_or(key: &str, fallback: &str, args: &[(&str, &str)]) -> String {
    #[cfg(test)]
    let local = LOCAL.with(|l| l.borrow().as_ref().map(|s| resolve(s, &[key])));
    #[cfg(not(test))]
    let local: Option<Option<String>> = None;
    let found = match local {
        Some(found) => found,
        None => match GLOBAL.try_read() {
            Ok(guard) => resolve(&guard, &[key]),
            Err(std::sync::TryLockError::Poisoned(p)) => resolve(&p.into_inner(), &[key]),
            Err(std::sync::TryLockError::WouldBlock) => None,
        },
    };
    fill(found.as_deref().unwrap_or(fallback), args)
}

/// Poisons the process-wide state, as a panic inside a write would (tests only).
#[cfg(test)]
pub fn poison_global_for_test() {
    let _ = std::thread::spawn(|| {
        let _guard = GLOBAL.write().unwrap_or_else(PoisonError::into_inner);
        panic!("poisoning the language state on purpose");
    })
    .join();
}

/// Runs `f` while this thread holds the process-wide state exclusively (tests only).
#[cfg(test)]
pub fn with_global_held<R>(f: impl FnOnce() -> R) -> R {
    let _guard = GLOBAL.write().unwrap_or_else(PoisonError::into_inner);
    f()
}

/// Thousands separator of the current language (`,` if a file omits it).
pub fn thousands_separator() -> String {
    read(|s| resolve(s, &["format.thousands"])).unwrap_or_else(|| ",".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn write_file(dir: &Path, name: &str, body: &[u8]) {
        fs::write(dir.join(name), body).unwrap();
    }

    fn lang_json(code: &str, name: &str, entries: &str) -> String {
        let sep = if entries.is_empty() { "" } else { "," };
        format!("{{\"_meta\":{{\"code\":\"{code}\",\"name\":\"{name}\",\"format\":1}}{sep}{entries}}}")
    }

    #[test]
    fn lookup_order_is_chosen_then_english_then_key() {
        isolate();
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "en.json",
            lang_json("en", "English", "\"only.en\":\"english\",\"both\":\"en both\"").as_bytes(),
        );
        write_file(dir.path(), "de.json", lang_json("de", "Deutsch", "\"both\":\"de both\"").as_bytes());
        assert!(scan_folder(dir.path()).is_empty());
        with_language("de", || {
            assert_eq!(tr("both"), "de both");
            assert_eq!(tr("only.en"), "english");
            assert_eq!(tr("no.such.key"), "no.such.key");
        });
    }

    #[test]
    fn placeholders_are_filled_and_unknown_ones_stay() {
        isolate();
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "de.json", lang_json("de", "Deutsch", "\"k\":\"{a} und {b}\"").as_bytes());
        scan_folder(dir.path());
        with_language("de", || {
            assert_eq!(tr_args("k", &[("a", "1")]), "1 und {b}");
            // A substituted value is never expanded again.
            assert_eq!(tr_args("k", &[("a", "{b}"), ("b", "2")]), "{b} und 2");
        });
    }

    #[test]
    fn one_and_other() {
        isolate();
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "de.json",
            lang_json(
                "de",
                "Deutsch",
                "\"c.one\":\"{n} Anfrage\",\"c.other\":\"{n} Anfragen\",\"d.other\":\"{n} Dinge\"",
            )
            .as_bytes(),
        );
        scan_folder(dir.path());
        with_language("de", || {
            assert_eq!(tr_n("c", 1, &[]), "1 Anfrage");
            assert_eq!(tr_n("c", 5, &[]), "5 Anfragen");
            assert_eq!(tr_n("c", 0, &[]), "0 Anfragen");
            assert_eq!(tr_n("d", 1, &[]), "1 Dinge");
        });
    }

    #[test]
    fn bom_file_loads() {
        isolate();
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(lang_json("fr", "Francais", "").as_bytes());
        write_file(dir.path(), "fr.json", &bytes);
        assert!(scan_folder(dir.path()).is_empty());
        assert!(available().iter().any(|l| l.code == "fr"));
    }

    #[test]
    fn broken_files_are_skipped_with_a_reason() {
        isolate();
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "a_invalid.json", b"{ not json");
        write_file(dir.path(), "b_nometa.json", b"{\"x\":\"y\"}");
        write_file(
            dir.path(),
            "c_format.json",
            lang_json("cc", "C", "").replace("\"format\":1", "\"format\":2").as_bytes(),
        );
        write_file(dir.path(), "d_code.json", lang_json("d_d", "D", "").as_bytes());
        write_file(dir.path(), "e_values.json", lang_json("ee", "E", "\"k\":5").as_bytes());
        let mut big = lang_json("ff", "F", "").into_bytes();
        big.resize(1_048_577, b' ');
        write_file(dir.path(), "f_big.json", &big);
        // The reasons are worded in the language in use; the words checked below are the English ones.
        let problems = with_language("en", || scan_folder(dir.path()));
        let names: Vec<&str> = problems.iter().map(|p| p.file.as_str()).collect();
        assert_eq!(
            names,
            ["a_invalid.json", "b_nometa.json", "c_format.json", "d_code.json", "e_values.json", "f_big.json"]
        );
        let expected = [
            "invalid JSON",
            "missing _meta",
            "_meta.format",
            "valid language code",
            "not a string",
            "larger than 1 MB",
        ];
        for (problem, part) in problems.iter().zip(expected) {
            assert!(problem.reason.contains(part), "{}: {}", problem.file, problem.reason);
        }
        let codes: Vec<String> = available().into_iter().map(|l| l.code).collect();
        assert_eq!(codes.len(), 2, "only the built-ins remain: {codes:?}");
    }

    #[test]
    fn size_limit_is_inclusive() {
        isolate();
        let dir = tempfile::tempdir().unwrap();
        let mut ok = lang_json("gg", "G", "").into_bytes();
        ok.resize(1_048_576, b' ');
        write_file(dir.path(), "g.json", &ok);
        assert!(scan_folder(dir.path()).is_empty());
        assert!(available().iter().any(|l| l.code == "gg"));
    }

    #[test]
    fn folder_file_wins_over_builtin() {
        isolate();
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "de.json", lang_json("de", "Deutsch", "\"format.thousands\":\"'\"").as_bytes());
        assert!(scan_folder(dir.path()).is_empty());
        with_language("de", || {
            assert_eq!(tr("format.thousands"), "'");
            // Missing in the folder file: built-in German, not English.
            assert_eq!(tr("format.date"), "%d.%m.%Y");
        });
        // Rescanning an empty folder drops the folder set and keeps built-ins.
        let empty = tempfile::tempdir().unwrap();
        scan_folder(empty.path());
        with_language("de", || assert_eq!(tr("format.thousands"), "."));
    }

    #[test]
    fn same_code_twice_keeps_first_by_file_name_and_reports_the_other() {
        isolate();
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "b.json", lang_json("xx", "Second", "\"k\":\"second\"").as_bytes());
        write_file(dir.path(), "a.json", lang_json("xx", "First", "\"k\":\"first\"").as_bytes());
        let problems = scan_folder(dir.path());
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].file, "b.json");
        with_language("xx", || assert_eq!(tr("k"), "first"));
        assert_eq!(available().iter().filter(|l| l.code == "xx").count(), 1);
    }

    #[test]
    fn unknown_language_falls_back() {
        isolate();
        set_language(Some("qq"));
        let code = current_code();
        assert!(available().iter().any(|l| l.code == code));
        assert!(!tr("format.thousands").is_empty());
        set_language(None);
        assert!(available().iter().any(|l| l.code == current_code()));
        set_language(Some("EN"));
        assert_eq!(current_code(), "en");
    }

    #[test]
    fn requested_language_is_resolved_again_on_every_scan() {
        isolate();
        set_language(Some("fr"));
        assert_ne!(current_code(), "fr", "not scanned yet");
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "fr.json", lang_json("fr", "Francais", "").as_bytes());
        assert!(scan_folder(dir.path()).is_empty());
        assert_eq!(current_code(), "fr");
        write_file(dir.path(), "fr.json", b"{ broken");
        assert_eq!(scan_folder(dir.path()).len(), 1);
        assert_ne!(current_code(), "fr");
        write_file(dir.path(), "fr.json", lang_json("fr", "Francais", "").as_bytes());
        assert!(scan_folder(dir.path()).is_empty());
        assert_eq!(current_code(), "fr");
    }

    #[test]
    fn poisoned_lock_is_tolerated() {
        let _ = std::thread::spawn(|| {
            let _guard = GLOBAL.write().unwrap_or_else(PoisonError::into_inner);
            panic!("poison the language state");
        })
        .join();
        assert!(GLOBAL.is_poisoned());
        // This thread has no isolated state, so it reads the poisoned global.
        with_language("en", || assert_eq!(tr("format.thousands"), ","));
        assert_eq!(tr("no.such.key"), "no.such.key");
        set_language(Some("de"));
        assert_eq!(current_code(), "de");
    }

    fn placeholders(text: &str) -> std::collections::BTreeSet<String> {
        let mut set = std::collections::BTreeSet::new();
        let mut rest = text;
        while let Some(start) = rest.find('{') {
            let after = &rest[start + 1..];
            match after.find('}') {
                Some(end) => {
                    set.insert(after[..end].to_string());
                    rest = &after[end + 1..];
                }
                None => break,
            }
        }
        set
    }

    #[test]
    fn builtins_have_same_keys_and_placeholders() {
        let de: serde_json::Map<String, serde_json::Value> = serde_json::from_str(DE_JSON).unwrap();
        let en: serde_json::Map<String, serde_json::Value> = serde_json::from_str(EN_JSON).unwrap();
        let keys = |m: &serde_json::Map<String, serde_json::Value>| {
            m.keys().filter(|k| *k != "_meta").cloned().collect::<std::collections::BTreeSet<_>>()
        };
        assert_eq!(keys(&de), keys(&en));
        for key in keys(&de) {
            assert_eq!(
                placeholders(de[&key].as_str().unwrap()),
                placeholders(en[&key].as_str().unwrap()),
                "placeholders differ for {key}"
            );
        }
    }

    /// Modules whose texts come from the language files, outside the window code.
    const NON_UI_MODULES: [&str; 19] = [
        "format", "cards", "view", "report", "worker", "analytics", "update", "update_worker", "paths", "paths_ui",
        "secrets", "settings", "cleanup", "crash", "single_instance", "launcher", "balance", "log_scan",
        "config_store",
    ];

    /// The window code and the tray: every text the user sees there comes from the language files.
    const UI_MODULES: [&str; 9] = [
        "ui/mod", "ui/cockpit", "ui/details", "ui/banners", "ui/chart", "ui/drawer", "ui/updates", "ui/theme", "tray",
    ];

    /// The part of a source file before its test module.
    fn non_test_source(module: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(format!("{module}.rs"));
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let end = text.find("#[cfg(test)]\nmod tests").or_else(|| text.find("#[cfg(test)]\r\nmod tests"));
        text[..end.unwrap_or(text.len())].to_string()
    }

    /// Contents of the string literals in Rust code; comments, char literals and lifetimes are skipped.
    fn string_literals(code: &str) -> Vec<String> {
        let chars: Vec<char> = code.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '/' if chars.get(i + 1) == Some(&'/') => {
                    while i < chars.len() && chars[i] != '\n' {
                        i += 1;
                    }
                }
                '/' if chars.get(i + 1) == Some(&'*') => {
                    i += 2;
                    while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                        i += 1;
                    }
                    i += 2;
                }
                'r' if matches!(chars.get(i + 1), Some('"' | '#'))
                    && (i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_')) =>
                {
                    let mut j = i + 1;
                    let mut hashes = 0;
                    while chars.get(j) == Some(&'#') {
                        hashes += 1;
                        j += 1;
                    }
                    if chars.get(j) != Some(&'"') {
                        i += 1;
                        continue;
                    }
                    j += 1;
                    let start = j;
                    let closing: String = std::iter::once('"').chain(std::iter::repeat_n('#', hashes)).collect();
                    let mut end = start;
                    while end < chars.len() && !chars[end..].starts_with(&closing.chars().collect::<Vec<_>>()) {
                        end += 1;
                    }
                    out.push(chars[start..end.min(chars.len())].iter().collect());
                    i = end + closing.len();
                }
                '"' => {
                    let mut j = i + 1;
                    let mut lit = String::new();
                    while j < chars.len() && chars[j] != '"' {
                        if chars[j] == '\\' {
                            lit.push(chars[j]);
                            j += 1;
                        }
                        if j < chars.len() {
                            lit.push(chars[j]);
                        }
                        j += 1;
                    }
                    out.push(lit);
                    i = j + 1;
                }
                '\'' => {
                    // A char literal ('x', '\n', '"') is skipped whole; a lifetime has no closing quote.
                    let close = if chars.get(i + 1) == Some(&'\\') { i + 3 } else { i + 2 };
                    if chars.get(close) == Some(&'\'') {
                        i = close + 1;
                    } else {
                        i += 1;
                    }
                }
                _ => i += 1,
            }
        }
        out
    }

    #[test]
    fn no_german_left_in_non_ui_modules() {
        let mut found = Vec::new();
        for module in NON_UI_MODULES {
            for lit in string_literals(&non_test_source(module)) {
                if lit.chars().any(|c| "äöüÄÖÜß".contains(c)) {
                    found.push(format!("{module}.rs: {lit}"));
                }
            }
        }
        assert!(found.is_empty(), "German text outside the language files:\n{}", found.join("\n"));
    }

    #[test]
    fn no_german_left_in_ui_modules() {
        let mut found = Vec::new();
        for module in UI_MODULES {
            for lit in string_literals(&non_test_source(module)) {
                if lit.chars().any(|c| "äöüÄÖÜß".contains(c)) {
                    found.push(format!("{module}.rs: {lit}"));
                }
            }
        }
        assert!(found.is_empty(), "German text outside the language files:\n{}", found.join("\n"));
    }

    #[test]
    fn the_scanner_reads_literals_and_skips_comments_and_chars() {
        let code = "// \"ä\" in a comment\nlet a = \"ö \\\" ü\"; let c = '\"'; let r = r#\"ß \"x\"\"#; fn f<'a>() {} /* \"ä\" */";
        assert_eq!(string_literals(code), vec!["ö \\\" ü".to_string(), "ß \"x\"".to_string()]);
    }

    #[test]
    fn every_key_the_code_asks_for_exists_in_both_builtins() {
        let de: serde_json::Map<String, serde_json::Value> = serde_json::from_str(DE_JSON).unwrap();
        let en: serde_json::Map<String, serde_json::Value> = serde_json::from_str(EN_JSON).unwrap();
        let known = |m: &serde_json::Map<String, serde_json::Value>, key: &str| {
            m.contains_key(key) || m.contains_key(&format!("{key}.one")) || m.contains_key(&format!("{key}.other"))
        };
        let looks_like_key = |s: &str| {
            s.contains('.')
                && !s.ends_with(['.'])
                && s.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))
                && !["json", "log", "exe", "jsonl", "part", "tmp", "bad", "ttf"].contains(&s.rsplit('.').next().unwrap_or(""))
        };
        let mut missing = Vec::new();
        for module in NON_UI_MODULES.into_iter().chain(UI_MODULES) {
            for lit in string_literals(&non_test_source(module)) {
                if looks_like_key(&lit) && !(known(&de, &lit) && known(&en, &lit)) {
                    missing.push(format!("{module}.rs: {lit}"));
                }
            }
        }
        assert!(missing.is_empty(), "keys missing in a built-in language:\n{}", missing.join("\n"));
    }

    #[test]
    fn no_shipped_text_has_an_em_dash() {
        for (name, json) in [("de", DE_JSON), ("en", EN_JSON)] {
            let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(json).unwrap();
            for (key, value) in &map {
                assert!(!value.as_str().unwrap_or_default().contains('\u{2014}'), "{name}: {key}");
            }
        }
    }

    #[test]
    fn all_weekday_keys_exist_in_both_builtins() {
        for (name, json) in [("de", DE_JSON), ("en", EN_JSON)] {
            let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(json).unwrap();
            for day in ["mon", "tue", "wed", "thu", "fri", "sat", "sun"] {
                let key = format!("format.weekday.{day}");
                let text = map.get(&key).and_then(|v| v.as_str());
                assert!(text.is_some_and(|t| !t.is_empty()), "{name}: {key} is missing or empty");
            }
        }
    }

    /// Every `.rs` file below `dir`.
    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap().filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|x| x == "rs") {
                out.push(path);
            }
        }
    }

    /// A key nobody asks for is dead weight in every language file. Test code does not count.
    #[test]
    fn every_key_is_used() {
        // Keys built at run time: the day abbreviations (`format!("format.weekday.{day}")`).
        const DYNAMIC_PREFIXES: [&str; 1] = ["format.weekday."];
        let mut files = Vec::new();
        rust_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        let mut used = std::collections::HashSet::new();
        for file in files {
            let text = fs::read_to_string(&file).unwrap();
            let end = text.find("#[cfg(test)]\nmod tests").or_else(|| text.find("#[cfg(test)]\r\nmod tests"));
            used.extend(string_literals(&text[..end.unwrap_or(text.len())]));
        }
        let en: serde_json::Map<String, serde_json::Value> = serde_json::from_str(EN_JSON).unwrap();
        let unused: Vec<&String> = en
            .keys()
            .filter(|k| *k != "_meta")
            .filter(|key| {
                let base = key.strip_suffix(".one").or_else(|| key.strip_suffix(".other")).unwrap_or(key);
                let plural = base.len() != key.len();
                !(used.contains(key.as_str()) || (plural && used.contains(base)) || DYNAMIC_PREFIXES.iter().any(|p| key.starts_with(p)))
            })
            .collect();
        assert!(unused.is_empty(), "keys in lang/en.json that no non-test code uses:\n{unused:#?}");
    }
}
