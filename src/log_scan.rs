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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanState {
    pub files: HashMap<PathBuf, FileMark>,
    pub records: HashMap<String, UsageRecord>,
    pub quota: Vec<QuotaEvent>,
    /// Folder these numbers were read from. `None` in files written before this field existed:
    /// the next scan adopts its root, so the history of deleted logs is not lost.
    #[serde(default)]
    pub root: Option<PathBuf>,
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
        }
        state.root = Some(root.to_path_buf());
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
        state.files.insert(path.to_path_buf(), FileMark { offset });
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
        }
    }

    state.files.insert(path.to_path_buf(), FileMark { offset });
    Ok(Some(pass))
}

/// Inserts a record unless its id is known. Repeated lines carry identical
/// usage; a main-session copy replaces a subagent copy so the result does not
/// depend on file order. Returns true when the id was new.
fn insert_record(state: &mut ScanState, record: UsageRecord) -> bool {
    match state.records.get_mut(&record.message_id) {
        None => {
            state.records.insert(record.message_id.clone(), record);
            true
        }
        Some(existing) => {
            if existing.is_subagent && !record.is_subagent {
                existing.is_subagent = false;
                existing.project = record.project;
                existing.session = record.session;
            }
            false
        }
    }
}

// --- Persistence -----------------------------------------------------------

/// Loads the saved state; a missing or unreadable file yields an empty state
/// (the next scan then rebuilds it from the logs).
pub fn load_state(path: &Path) -> ScanState {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
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

    #[test]
    fn counts_repeated_message_id_once() {
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
