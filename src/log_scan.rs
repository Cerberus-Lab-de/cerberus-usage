//! Incremental scan of Claude Code JSONL session logs.
//!
//! Only metadata is read and stored (token counts, model, timestamps, quota
//! events). `message.content` is never deserialized or kept.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Token usage of one assistant message (deduplicated by `message_id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub message_id: String,
    pub ts: DateTime<Utc>,
    pub model: String,
    pub project: String,
    pub session: String,
    pub is_subagent: bool,
    pub input: u64,
    pub cache_read: u64,
    pub cache_5m: u64,
    pub cache_1h: u64,
    pub output: u64,
    pub speed: Option<String>,
    pub geo: Option<String>,
}

/// A rate-limit event found in a `quotaLimits` field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaEvent {
    pub ts: DateTime<Utc>,
    pub status: String,
    pub resets_at: Option<DateTime<Utc>>,
    pub limit_type: Option<String>,
}

/// Read position of one log file (bytes up to and including the last `\n` seen).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMark {
    pub offset: u64,
}

/// Version of the saved state. 1: token fields are the maximum over repeated lines.
/// Older files keep their records (the history of deleted logs) but lose their read marks,
/// so every existing log is read again and the maximum corrects the records in place.
pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanState {
    /// Missing in files from before versioning (read as 0).
    #[serde(default)]
    pub version: u32,
    pub files: HashMap<PathBuf, FileMark>,
    pub records: HashMap<String, UsageRecord>,
    pub quota: Vec<QuotaEvent>,
    /// Folder these numbers were read from. `None` in files written before this field existed:
    /// the next scan adopts its root, so the history of deleted logs is not lost.
    #[serde(default)]
    pub root: Option<PathBuf>,
    /// Something changed since the state was last saved. Not persisted.
    #[serde(skip)]
    pub dirty: bool,
}

impl Default for ScanState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            files: HashMap::new(),
            records: HashMap::new(),
            quota: Vec::new(),
            root: None,
            dirty: false,
        }
    }
}

impl ScanState {
    /// Brings a loaded state to the current version. An older one keeps its records but forgets
    /// which bytes it has read, so all logs are read again.
    fn upgrade(&mut self) {
        if self.version < STATE_VERSION {
            self.files.clear();
            self.version = STATE_VERSION;
            self.dirty = true;
        }
    }
}

/// Same folder, ignoring trailing separators, `/` vs `\` and letter case (Windows).
fn same_root(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    norm(a) == norm(b)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanStats {
    /// `.jsonl` files found under the root.
    pub files_total: usize,
    /// Files that had new bytes to read.
    pub files_read: usize,
    /// Files that could not be opened or read (skipped).
    pub files_failed: usize,
    /// Complete lines consumed in this run.
    pub lines_read: u64,
    /// Lines that carried a usable usage block (before dedupe).
    pub usage_lines: u64,
    /// Records not seen before.
    pub new_records: usize,
    /// Bytes consumed (complete lines only).
    pub bytes_read: u64,
}

// --- Line schema -----------------------------------------------------------
// Only the fields we need. `message.content` is not declared, so serde skips
// it without keeping anything.

#[derive(Deserialize)]
struct Line {
    timestamp: Option<DateTime<Utc>>,
    cwd: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "quotaLimits")]
    quota_limits: Option<QuotaLimits>,
    message: Option<Message>,
}

/// `message` is normally an object; tolerate any other shape.
#[derive(Deserialize)]
#[serde(untagged)]
enum Message {
    Object(MessageObj),
    Other(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
struct MessageObj {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_creation: Option<CacheCreation>,
    speed: Option<String>,
    inference_geo: Option<String>,
}

#[derive(Deserialize)]
struct CacheCreation {
    ephemeral_5m_input_tokens: Option<u64>,
    ephemeral_1h_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct QuotaLimits {
    status: Option<String>,
    #[serde(rename = "resetsAt")]
    resets_at: Option<f64>,
    #[serde(rename = "rateLimitType")]
    rate_limit_type: Option<String>,
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.is_empty())
}

/// Last non-empty path component of `cwd`, accepting `/` and `\` separators.
fn project_name(cwd: Option<&str>) -> String {
    cwd.and_then(|c| c.rsplit(['/', '\\']).find(|p| !p.is_empty()))
        .unwrap_or("unknown")
        .to_string()
}

/// Parses one complete line; returns the usage record and/or quota event it holds.
fn parse_line(
    bytes: &[u8],
    file_session: &str,
    is_subagent: bool,
) -> (Option<UsageRecord>, Option<QuotaEvent>) {
    let Ok(line) = serde_json::from_slice::<Line>(bytes) else {
        return (None, None);
    };
    let Some(ts) = line.timestamp else {
        return (None, None);
    };

    let quota = line.quota_limits.and_then(|q| {
        Some(QuotaEvent {
            ts,
            status: q.status?,
            resets_at: q
                .resets_at
                .and_then(|s| DateTime::from_timestamp(s as i64, 0)),
            limit_type: non_empty(q.rate_limit_type),
        })
    });

    let record = match line.message {
        Some(Message::Object(m)) => (|| {
            let usage = m.usage?;
            let model = m.model?;
            if model == "<synthetic>" {
                return None;
            }
            let (cache_5m, cache_1h) = match usage.cache_creation {
                Some(c) => (
                    c.ephemeral_5m_input_tokens.unwrap_or(0),
                    c.ephemeral_1h_input_tokens.unwrap_or(0),
                ),
                None => (usage.cache_creation_input_tokens.unwrap_or(0), 0),
            };
            Some(UsageRecord {
                message_id: m.id.filter(|id| !id.is_empty())?,
                ts,
                model,
                project: project_name(line.cwd.as_deref()),
                session: non_empty(line.session_id).unwrap_or_else(|| file_session.to_string()),
                is_subagent,
                input: usage.input_tokens.unwrap_or(0),
                cache_read: usage.cache_read_input_tokens.unwrap_or(0),
                cache_5m,
                cache_1h,
                output: usage.output_tokens.unwrap_or(0),
                speed: non_empty(usage.speed),
                geo: non_empty(usage.inference_geo),
            })
        })(),
        _ => None,
    };
    (record, quota)
}

// --- File discovery --------------------------------------------------------

/// Collects `*.jsonl` files below `dir` (symlinks are not followed), sorted for
/// a deterministic scan order. Unreadable subdirectories are skipped.
fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>, top: bool) -> io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if top => return Err(e),
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            collect_jsonl(&path, out, false)?;
        } else if kind.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
    Ok(())
}

// --- Scan ------------------------------------------------------------------

/// Counts of one file pass, merged into `ScanStats` by the caller.
#[derive(Default)]
struct FilePass {
    lines: u64,
    usage_lines: u64,
    new_records: usize,
    bytes: u64,
}

/// Scans every `.jsonl` file below `root`, reading only bytes not yet seen.
///
/// `progress(done, total)` is called after each file. A missing root is an
/// empty scan. Files that fail to open or read are skipped and counted in
/// `files_failed`; their mark is left untouched so the next run retries.
pub fn scan(
    root: &Path,
    state: &mut ScanState,
    progress: &dyn Fn(usize, usize),
) -> io::Result<ScanStats> {
    // Numbers of two folders must not mix: a different root rebuilds the state. A state without a
    // root (old file) is kept and adopts this one.
    // While the new folder does not exist (typo, unplugged drive) the state and the old root stay.
    if root.is_dir() {
        if state.root.as_deref().is_some_and(|old| !same_root(old, root)) {
            *state = ScanState::default();
            state.dirty = true;
        }
        if state.root.as_deref() != Some(root) {
            state.root = Some(root.to_path_buf());
            state.dirty = true;
        }
    }
    let mut files = Vec::new();
    match collect_jsonl(root, &mut files, true) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    files.sort();

    let mut stats = ScanStats {
        files_total: files.len(),
        ..ScanStats::default()
    };
    progress(0, files.len());
    for (i, path) in files.iter().enumerate() {
        match scan_file(root, path, state) {
            Ok(Some(pass)) => {
                stats.files_read += 1;
                stats.lines_read += pass.lines;
                stats.usage_lines += pass.usage_lines;
                stats.new_records += pass.new_records;
                stats.bytes_read += pass.bytes;
            }
            Ok(None) => {}
            Err(_) => stats.files_failed += 1,
        }
        progress(i + 1, files.len());
    }
    Ok(stats)
}

/// Reads new complete lines of one file. `Ok(None)` means nothing new.
fn scan_file(root: &Path, path: &Path, state: &mut ScanState) -> io::Result<Option<FilePass>> {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};

    let len = std::fs::metadata(path)?.len();
    let mut offset = state.files.get(path).map_or(0, |m| m.offset);
    if len < offset {
        // File was truncated or replaced: start over (records stay, dedupe by id).
        offset = 0;
    }
    if len == offset {
        set_mark(state, path, offset);
        return Ok(None);
    }

    let rel = path.strip_prefix(root).unwrap_or(path);
    let is_subagent = rel.components().any(|c| c.as_os_str() == "subagents");
    let file_session = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::with_capacity(256 * 1024, file);

    let usage_finder = memchr::memmem::Finder::new(b"\"usage\"");
    let quota_finder = memchr::memmem::Finder::new(b"\"quotaLimits\"");

    let mut pass = FilePass::default();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 || buf.last() != Some(&b'\n') {
            // EOF, or a partial last line that is still being written.
            break;
        }
        offset += n as u64;
        pass.bytes += n as u64;
        pass.lines += 1;

        if usage_finder.find(&buf).is_none() && quota_finder.find(&buf).is_none() {
            continue;
        }
        let (record, quota) = parse_line(&buf, &file_session, is_subagent);
        if let Some(record) = record {
            pass.usage_lines += 1;
            if insert_record(state, record) {
                pass.new_records += 1;
            }
        }
        if let Some(event) = quota
            && !state.quota.contains(&event)
        {
            state.quota.push(event);
            state.dirty = true;
        }
    }

    set_mark(state, path, offset);
    Ok(Some(pass))
}

/// Stores the read position of a file; marks the state dirty only when it actually changed.
fn set_mark(state: &mut ScanState, path: &Path, offset: u64) {
    let mark = FileMark { offset };
    if state.files.insert(path.to_path_buf(), mark) != Some(mark) {
        state.dirty = true;
    }
}

/// Inserts a record unless its id is known. Repeated lines of one message do not always carry the
/// same usage (the later lines hold the final `output_tokens`), so every token field becomes the
/// maximum over all lines, independent of file order. Timestamp, model and project stay from the
/// first line; a main-session copy replaces a subagent copy's project and session. Returns true
/// when the id was new.
fn insert_record(state: &mut ScanState, record: UsageRecord) -> bool {
    match state.records.get_mut(&record.message_id) {
        None => {
            state.records.insert(record.message_id.clone(), record);
            state.dirty = true;
            true
        }
        Some(existing) => {
            let before = existing.clone();
            existing.input = existing.input.max(record.input);
            existing.cache_read = existing.cache_read.max(record.cache_read);
            existing.cache_5m = existing.cache_5m.max(record.cache_5m);
            existing.cache_1h = existing.cache_1h.max(record.cache_1h);
            existing.output = existing.output.max(record.output);
            if existing.is_subagent && !record.is_subagent {
                existing.is_subagent = false;
                existing.project = record.project;
                existing.session = record.session;
            }
            if *existing != before {
                state.dirty = true;
            }
            false
        }
    }
}

// --- Persistence -----------------------------------------------------------

/// A loaded state plus a one-time note for the user when something had to be set aside.
pub struct LoadedState {
    pub state: ScanState,
    pub note: Option<String>,
}

/// Loads the saved state. A missing file is a normal first run. A file that exists but cannot be
/// read or parsed is renamed to `<name>.bad` (only the newest such copy is kept) and the scan
/// rebuilds from the logs; the note tells the user, since records of already deleted logs are
/// only in the `.bad` copy. An older version is upgraded (see [`STATE_VERSION`]).
pub fn load_state_checked(path: &Path) -> LoadedState {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return LoadedState { state: ScanState::default(), note: None };
        }
        Err(_) => return set_aside(path),
    };
    match serde_json::from_slice::<ScanState>(&bytes) {
        Ok(mut state) => {
            state.upgrade();
            LoadedState { state, note: None }
        }
        Err(_) => set_aside(path),
    }
}

fn set_aside(path: &Path) -> LoadedState {
    let mut bad = path.as_os_str().to_owned();
    bad.push(".bad");
    let bad = PathBuf::from(bad);
    // rename replaces an older .bad on Windows, so only the newest copy stays.
    let kept = std::fs::rename(path, &bad).is_ok();
    let note = if kept {
        format!(
            "Die gespeicherte Verbrauchsliste war nicht lesbar und wird aus den Protokollen neu aufgebaut. Die alte Datei liegt als {}.",
            bad.display()
        )
    } else {
        "Die gespeicherte Verbrauchsliste war nicht lesbar und wird aus den Protokollen neu aufgebaut.".to_string()
    };
    LoadedState { state: ScanState::default(), note: Some(note) }
}

/// Like [`load_state_checked`] without the note.
pub fn load_state(path: &Path) -> ScanState {
    load_state_checked(path).state
}

/// Writes the state atomically: temporary file next to the target, then rename.
pub fn save_state(path: &Path, state: &ScanState) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let result = (|| {
        let file = std::fs::File::create(&tmp)?;
        let mut writer = io::BufWriter::new(file);
        serde_json::to_writer(&mut writer, state).map_err(io::Error::other)?;
        let file = writer.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::io::Write;

    const TS: &str = "2026-11-04T10:00:00.000Z";

    fn usage_line(id: &str, model: &str, output: u64) -> String {
        json!({
            "timestamp": TS,
            "cwd": "C:\\Work\\demo-project",
            "sessionId": "sess-1",
            "isSidechain": false,
            "message": {
                "id": id,
                "model": model,
                "content": [{"type": "text", "text": "GEHEIM"}],
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": output,
                    "cache_read_input_tokens": 20,
                    "cache_creation_input_tokens": 30,
                    "cache_creation": {
                        "ephemeral_5m_input_tokens": 30,
                        "ephemeral_1h_input_tokens": 0
                    },
                    "speed": "standard",
                    "inference_geo": "us"
                }
            }
        })
        .to_string()
    }

    fn write_lines(path: &Path, lines: &[String]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = lines.join("\n");
        text.push('\n');
        fs::write(path, text).unwrap();
    }

    fn append(path: &Path, text: &str) {
        let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    fn run(root: &Path, state: &mut ScanState) -> ScanStats {
        scan(root, state, &|_, _| {}).unwrap()
    }

    /// A line like `usage_line`, but with explicit token fields.
    fn tokens_line(id: &str, input: u64, cache_read: u64, cache_5m: u64, cache_1h: u64, output: u64) -> String {
        json!({
            "timestamp": TS,
            "cwd": "C:\\Work\\demo-project",
            "sessionId": "sess-1",
            "message": {
                "id": id,
                "model": "claude-sonnet-5",
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_read_input_tokens": cache_read,
                    "cache_creation": {
                        "ephemeral_5m_input_tokens": cache_5m,
                        "ephemeral_1h_input_tokens": cache_1h
                    }
                }
            }
        })
        .to_string()
    }

    #[test]
    fn counts_identical_repeats_once() {
        let dir = tempfile::tempdir().unwrap();
        let line = usage_line("msg_1", "claude-sonnet-5", 77);
        write_lines(
            &dir.path().join("proj").join("a.jsonl"),
            &[line.clone(), line.clone(), line],
        );
        let mut state = ScanState::default();
        let stats = run(dir.path(), &mut state);
        assert_eq!(state.records.len(), 1);
        assert_eq!(state.records["msg_1"].output, 77);
        assert_eq!(stats.usage_lines, 3);
        assert_eq!(stats.new_records, 1);
        let r = &state.records["msg_1"];
        assert_eq!(r.project, "demo-project");
        assert_eq!(r.session, "sess-1");
        assert_eq!(r.model, "claude-sonnet-5");
        assert_eq!(r.input, 10);
        assert_eq!(r.cache_read, 20);
        assert_eq!(r.speed.as_deref(), Some("standard"));
        assert_eq!(r.geo.as_deref(), Some("us"));
        assert!(!r.is_subagent);
    }

    #[test]
    fn growing_output_takes_the_maximum_in_one_file() {
        let dir = tempfile::tempdir().unwrap();
        write_lines(
            &dir.path().join("proj").join("a.jsonl"),
            &[
                tokens_line("m", 10, 20, 30, 0, 3),
                tokens_line("m", 10, 20, 30, 0, 250),
                tokens_line("m", 10, 20, 30, 0, 90),
            ],
        );
        let mut state = ScanState::default();
        let stats = run(dir.path(), &mut state);
        assert_eq!(state.records["m"].output, 250);
        assert_eq!(stats.new_records, 1);
    }

    #[test]
    fn maximum_is_independent_of_file_order_and_per_field() {
        // Two files, scanned in name order; the same two lines in both orders give the same record.
        let lines = [
            tokens_line("m", 10, 20, 30, 0, 5),
            tokens_line("m", 12, 15, 30, 40, 400),
        ];
        let mut results = Vec::new();
        for order in [[0, 1], [1, 0]] {
            let dir = tempfile::tempdir().unwrap();
            write_lines(&dir.path().join("proj/a.jsonl"), std::slice::from_ref(&lines[order[0]]));
            write_lines(&dir.path().join("proj/b.jsonl"), std::slice::from_ref(&lines[order[1]]));
            let mut state = ScanState::default();
            run(dir.path(), &mut state);
            let r = &state.records["m"];
            results.push((r.input, r.cache_read, r.cache_5m, r.cache_1h, r.output));
        }
        assert_eq!(results[0], results[1]);
        assert_eq!(results[0], (12, 20, 30, 40, 400), "each field is the maximum over all lines");
    }

    #[test]
    fn a_later_larger_line_in_an_appended_scan_raises_the_stored_record() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("proj").join("a.jsonl");
        write_lines(&file, &[tokens_line("m", 10, 20, 30, 0, 5)]);
        let mut state = ScanState::default();
        run(dir.path(), &mut state);
        append(&file, &format!("{}\n", tokens_line("m", 10, 20, 30, 0, 900)));
        let stats = run(dir.path(), &mut state);
        assert_eq!(state.records["m"].output, 900);
        assert_eq!(stats.new_records, 0);
    }

    #[test]
    fn dedupes_across_files() {
        let dir = tempfile::tempdir().unwrap();
        let line = usage_line("msg_1", "claude-sonnet-5", 5);
        write_lines(
            &dir.path().join("proj").join("s1.jsonl"),
            std::slice::from_ref(&line),
        );
        write_lines(
            &dir.path()
                .join("proj")
                .join("s1")
                .join("subagents")
                .join("agent-x.jsonl"),
            &[line],
        );
        let mut state = ScanState::default();
        run(dir.path(), &mut state);
        assert_eq!(state.records.len(), 1);
        // The main-session copy wins over the subagent copy.
        assert!(!state.records["msg_1"].is_subagent);
    }

    #[test]
    fn marks_subagent_files() {
        let dir = tempfile::tempdir().unwrap();
        write_lines(
            &dir.path()
                .join("proj")
                .join("s1")
                .join("subagents")
                .join("agent-x.jsonl"),
            &[usage_line("msg_sub", "claude-sonnet-5", 5)],
        );
        let mut state = ScanState::default();
        run(dir.path(), &mut state);
        assert!(state.records["msg_sub"].is_subagent);
    }

    #[test]
    fn second_scan_reads_only_new_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("proj").join("a.jsonl");
        write_lines(&file, &[usage_line("msg_1", "claude-sonnet-5", 1)]);
        let mut state = ScanState::default();
        run(dir.path(), &mut state);

        let extra = format!("{}\n", usage_line("msg_2", "claude-sonnet-5", 2));
        append(&file, &extra);
        let stats = run(dir.path(), &mut state);
        assert_eq!(stats.bytes_read, extra.len() as u64);
        assert_eq!(stats.new_records, 1);
        assert_eq!(state.records.len(), 2);

        let stats = run(dir.path(), &mut state);
        assert_eq!(stats.bytes_read, 0);
        assert_eq!(stats.files_read, 0);
    }

    #[test]
    fn partial_last_line_is_read_next_time() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("proj").join("a.jsonl");
        let first = usage_line("msg_1", "claude-sonnet-5", 1);
        let second = usage_line("msg_2", "claude-sonnet-5", 2);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, format!("{first}\n{second}")).unwrap();

        let mut state = ScanState::default();
        run(dir.path(), &mut state);
        assert_eq!(state.records.len(), 1);
        assert_eq!(state.files[&file].offset, first.len() as u64 + 1);

        append(&file, "\n");
        let stats = run(dir.path(), &mut state);
        assert_eq!(state.records.len(), 2);
        assert_eq!(stats.new_records, 1);
        assert_eq!(stats.bytes_read, second.len() as u64 + 1);
        assert_eq!(
            state.files[&file].offset,
            (first.len() + second.len() + 2) as u64
        );
    }

    #[test]
    fn rescans_when_file_shrinks() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("proj").join("a.jsonl");
        write_lines(
            &file,
            &[
                usage_line("msg_1", "claude-sonnet-5", 1),
                usage_line("msg_2", "claude-sonnet-5", 2),
            ],
        );
        let mut state = ScanState::default();
        run(dir.path(), &mut state);
        let old_offset = state.files[&file].offset;

        let replacement = usage_line("msg_3", "claude-sonnet-5", 3);
        write_lines(&file, std::slice::from_ref(&replacement));
        assert!((replacement.len() as u64) < old_offset);
        let stats = run(dir.path(), &mut state);
        assert!(state.records.contains_key("msg_3"));
        assert_eq!(stats.bytes_read, replacement.len() as u64 + 1);
        assert_eq!(state.files[&file].offset, replacement.len() as u64 + 1);
    }

    #[test]
    fn skips_malformed_lines_and_synthetic_model() {
        let dir = tempfile::tempdir().unwrap();
        write_lines(
            &dir.path().join("proj").join("a.jsonl"),
            &[
                "this line mentions usage but is not json".to_string(),
                "{\"usage\": broken".to_string(),
                usage_line("msg_syn", "<synthetic>", 9),
                json!({"timestamp": TS, "message": {"id": "msg_nousage", "model": "m"}})
                    .to_string(),
                usage_line("msg_ok", "claude-sonnet-5", 4),
            ],
        );
        let mut state = ScanState::default();
        let stats = run(dir.path(), &mut state);
        assert_eq!(state.records.len(), 1);
        assert!(state.records.contains_key("msg_ok"));
        assert_eq!(stats.lines_read, 5);
    }

    #[test]
    fn splits_cache_write_ttls_and_falls_back_to_5m_without_details() {
        let dir = tempfile::tempdir().unwrap();
        let split = json!({
            "timestamp": TS, "cwd": "/w/p", "sessionId": "s",
            "message": {"id": "msg_split", "model": "m", "usage": {
                "input_tokens": 1, "output_tokens": 1,
                "cache_creation_input_tokens": 300,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 100,
                    "ephemeral_1h_input_tokens": 200
                }
            }}
        })
        .to_string();
        let flat = json!({
            "timestamp": TS, "cwd": "/w/p", "sessionId": "s",
            "message": {"id": "msg_flat", "model": "m", "usage": {
                "input_tokens": 1, "output_tokens": 1,
                "cache_creation_input_tokens": 50
            }}
        })
        .to_string();
        write_lines(&dir.path().join("proj").join("a.jsonl"), &[split, flat]);
        let mut state = ScanState::default();
        run(dir.path(), &mut state);
        let s = &state.records["msg_split"];
        assert_eq!((s.cache_5m, s.cache_1h), (100, 200));
        let f = &state.records["msg_flat"];
        assert_eq!((f.cache_5m, f.cache_1h), (50, 0));
        assert_eq!(f.speed, None);
        assert_eq!(f.geo, None);
        assert_eq!(f.project, "p");
    }

    #[test]
    fn collects_rejected_quota_event() {
        let dir = tempfile::tempdir().unwrap();
        let line = json!({
            "timestamp": TS,
            "quotaLimits": {
                "status": "rejected",
                "resetsAt": 1793659800u64,
                "rateLimitType": "five_hour",
                "overageStatus": "rejected"
            }
        })
        .to_string();
        write_lines(
            &dir.path().join("proj").join("a.jsonl"),
            &[line.clone(), line],
        );
        let mut state = ScanState::default();
        run(dir.path(), &mut state);
        assert_eq!(state.quota.len(), 1, "identical events are stored once");
        let e = &state.quota[0];
        assert_eq!(e.status, "rejected");
        assert_eq!(e.resets_at, DateTime::from_timestamp(1_793_659_800, 0));
        assert_eq!(e.limit_type.as_deref(), Some("five_hour"));
        assert_eq!(
            e.ts,
            "2026-11-04T10:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn state_never_contains_message_content() {
        let dir = tempfile::tempdir().unwrap();
        write_lines(
            &dir.path().join("proj").join("a.jsonl"),
            &[usage_line("msg_1", "claude-sonnet-5", 1)],
        );
        let mut state = ScanState::default();
        run(dir.path(), &mut state);
        assert!(!state.records.is_empty());
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("GEHEIM"));

        let out = dir.path().join("state").join("scan.json");
        save_state(&out, &state).unwrap();
        assert!(!fs::read_to_string(&out).unwrap().contains("GEHEIM"));
    }

    #[test]
    fn keeps_records_after_source_file_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("proj").join("a.jsonl");
        write_lines(&file, &[usage_line("msg_1", "claude-sonnet-5", 1)]);
        let mut state = ScanState::default();
        run(dir.path(), &mut state);
        fs::remove_file(&file).unwrap();
        run(dir.path(), &mut state);
        assert_eq!(state.records.len(), 1);
    }

    #[test]
    fn state_roundtrips_and_bad_state_file_gives_default() {
        let dir = tempfile::tempdir().unwrap();
        write_lines(
            &dir.path().join("proj").join("a.jsonl"),
            &[usage_line("msg_1", "claude-sonnet-5", 1)],
        );
        let mut state = ScanState::default();
        run(dir.path(), &mut state);
        let out = dir.path().join("out").join("scan.json");
        save_state(&out, &state).unwrap();
        assert!(!out.with_extension("json.tmp").exists());
        let loaded = load_state(&out);
        assert_eq!(loaded.records, state.records);
        assert_eq!(loaded.files, state.files);

        fs::write(&out, "{ not valid").unwrap();
        assert!(load_state(&out).records.is_empty());
        assert!(
            load_state(&dir.path().join("missing.json"))
                .records
                .is_empty()
        );
    }

    #[test]
    fn older_state_keeps_records_of_deleted_logs_and_corrects_low_ones() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        // Log still on disk: the stored record is too low (output 5, real 250).
        write_lines(&logs.join("p/a.jsonl"), &[tokens_line("alive", 10, 20, 30, 0, 5), tokens_line("alive", 10, 20, 30, 0, 250)]);
        let mut old = ScanState::default();
        run(&logs, &mut old);
        old.records.get_mut("alive").unwrap().output = 5;
        // A record whose log is long gone.
        let mut gone = old.records["alive"].clone();
        gone.message_id = "deleted-log".into();
        gone.output = 42;
        old.records.insert("deleted-log".into(), gone);
        // Write it as a pre-version file: no `version` key.
        let mut value = serde_json::to_value(&old).unwrap();
        value.as_object_mut().unwrap().remove("version");
        let path = dir.path().join("state.json");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

        let loaded = load_state_checked(&path);
        assert!(loaded.note.is_none());
        let mut state = loaded.state;
        assert_eq!(state.version, STATE_VERSION);
        assert!(state.files.is_empty(), "read marks are cleared so every log is read again");
        assert_eq!(state.records.len(), 2, "records of deleted logs survive");

        run(&logs, &mut state);
        assert_eq!(state.records["alive"].output, 250, "too-low record is corrected in place");
        assert_eq!(state.records["deleted-log"].output, 42);
        assert!(!path.with_extension("json.bad").exists());

        // Saved and loaded again at the current version: marks stay.
        save_state(&path, &state).unwrap();
        let again = load_state(&path);
        assert_eq!(again.version, STATE_VERSION);
        assert!(!again.files.is_empty());
    }

    #[test]
    fn unreadable_state_is_set_aside_as_bad_keeping_only_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scan_state.json");
        let bad = dir.path().join("scan_state.json.bad");
        fs::write(&path, "{ first").unwrap();
        let loaded = load_state_checked(&path);
        assert!(loaded.state.records.is_empty());
        assert!(loaded.note.as_deref().unwrap().contains("scan_state.json.bad"));
        assert!(!path.exists());
        assert_eq!(fs::read_to_string(&bad).unwrap(), "{ first");

        fs::write(&path, "{ second").unwrap();
        assert!(load_state_checked(&path).note.is_some());
        assert_eq!(fs::read_to_string(&bad).unwrap(), "{ second", "only the newest copy is kept");

        // A missing file is a normal first run: no note, nothing set aside.
        let none = load_state_checked(&dir.path().join("missing.json"));
        assert!(none.note.is_none() && none.state.records.is_empty());
    }

    #[test]
    fn dirty_only_when_the_scan_changed_something() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("proj").join("a.jsonl");
        write_lines(&file, &[usage_line("msg_1", "claude-sonnet-5", 1)]);
        let mut state = ScanState::default();
        assert!(!state.dirty);
        run(dir.path(), &mut state);
        assert!(state.dirty);
        state.dirty = false;

        run(dir.path(), &mut state);
        assert!(!state.dirty, "an unchanged scan must not ask for a save");

        append(&file, &format!("{}\n", usage_line("msg_2", "claude-sonnet-5", 2)));
        run(dir.path(), &mut state);
        assert!(state.dirty);
    }

    #[test]
    fn changed_logs_root_resets_scan_state() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        write_lines(&a.path().join("p/a.jsonl"), &[usage_line("a1", "m", 5)]);
        write_lines(&b.path().join("p/b.jsonl"), &[usage_line("b1", "m", 7)]);

        let mut state = ScanState::default();
        run(a.path(), &mut state);
        assert_eq!(state.records.len(), 1);
        assert_eq!(state.root.as_deref(), Some(a.path()));

        // Same root (trailing separator and letter case do not matter): history is kept.
        let mut upper = a.path().to_string_lossy().to_uppercase();
        upper.push('\\');
        run(Path::new(&upper), &mut state);
        assert_eq!(state.records.len(), 1);

        // Different root: numbers of two folders never mix.
        run(b.path(), &mut state);
        assert_eq!(state.records.len(), 1);
        assert!(state.records.contains_key("b1"));
        assert_eq!(state.root.as_deref(), Some(b.path()));
    }

    #[test]
    fn missing_new_root_keeps_the_state_until_it_exists() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        write_lines(&a.path().join("p/a.jsonl"), &[usage_line("a1", "m", 5)]);
        let mut state = ScanState::default();
        run(a.path(), &mut state);

        let gone = b.path().join("not-there");
        run(&gone, &mut state);
        assert_eq!(state.records.len(), 1, "typo or unplugged drive must not wipe the history");
        assert_eq!(state.root.as_deref(), Some(a.path()));

        std::fs::create_dir_all(&gone).unwrap();
        run(&gone, &mut state);
        assert!(state.records.is_empty());
        assert_eq!(state.root.as_deref(), Some(gone.as_path()));
    }

    #[test]
    fn state_without_root_adopts_the_current_root_and_keeps_history() {
        let a = tempfile::tempdir().unwrap();
        let log = a.path().join("p/a.jsonl");
        write_lines(&log, &[usage_line("old", "m", 5)]);
        let mut state = ScanState::default();
        run(a.path(), &mut state);
        // An old state file has no root; the log it came from is gone by now.
        state.root = None;
        std::fs::remove_file(&log).unwrap();
        run(a.path(), &mut state);
        assert!(state.records.contains_key("old"), "history of deleted logs must survive");
        assert_eq!(state.root.as_deref(), Some(a.path()));
    }

    #[test]
    fn missing_root_is_empty_scan() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = ScanState::default();
        let stats = run(&dir.path().join("nope"), &mut state);
        assert_eq!(stats.files_total, 0);
    }
}
