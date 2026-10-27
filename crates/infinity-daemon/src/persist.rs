//! Crash-safe file primitives for the daemon's on-disk state.
//!
//! Two write patterns cover every file the daemon persists:
//!
//! - **Append-only logs** ([`ThreadLog`]): a thread's conversation history is
//!   a JSONL file whose first line is a [`ThreadLogEntry::Header`] and whose
//!   remaining lines are appended, never rewritten. A crash mid-append can
//!   only tear the *last* line, so loading recovers the longest valid prefix
//!   and truncates the torn tail before anything new is appended.
//! - **Atomic replacement** ([`write_atomic`]): small "last writer wins"
//!   documents (thread metadata, tool state, views, the session index) are
//!   written to a temporary file in the same directory, fsynced, and renamed
//!   over the destination, so readers only ever see the old or the new
//!   contents, never a partial write.
//!
//! Every file carries a format version so future layout changes can migrate
//! old files explicitly instead of guessing from their shape. Files written
//! before versioning was introduced deserialize with version
//! [`LEGACY_VERSION`] (`0`).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use infinity_agent_core::ThreadId;
use infinity_agent_core::message::InfinityMessage;
use infinity_agent_core::stores::CompactionSummary;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The version deserialized for files written before format versioning
/// existed (the `version` field is absent).
pub const LEGACY_VERSION: u32 = 0;

/// Current format version of a thread's conversation log (`{thread_id}.jsonl`).
pub const THREAD_LOG_VERSION: u32 = 1;
/// Current format version of a thread's metadata file (`{thread_id}.meta.json`).
pub const THREAD_META_VERSION: u32 = 1;
/// Current format version of a thread's views file (`{thread_id}.views.json`).
pub const THREAD_VIEWS_VERSION: u32 = 1;
/// Current format version of a thread's tool-state file (`{thread_id}.state.json`).
pub const THREAD_STATE_VERSION: u32 = 1;
/// Current format version of the session index (`sessions.json`).
pub const SESSIONS_VERSION: u32 = 1;

/// Errors from reading or writing persisted state.
#[derive(Debug)]
pub enum PersistError {
    Io {
        path: String,
        source: io::Error,
    },
    /// The file is well-formed but was written by a newer format version
    /// than this daemon understands. Refusing to read (and therefore to
    /// write) it is deliberate: appending in the old format would corrupt
    /// the newer file.
    UnsupportedVersion {
        path: String,
        found: u32,
        supported: u32,
    },
    /// A complete, newline-terminated log line (or a whole JSON document)
    /// failed to parse. This is not a torn write — torn tails are repaired
    /// transparently — so it is surfaced rather than silently truncated.
    Corrupt {
        path: String,
        detail: String,
    },
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PersistError::Io { path, source } => write!(f, "{path}: {source}"),
            PersistError::UnsupportedVersion {
                path,
                found,
                supported,
            } => write!(
                f,
                "{path}: format version {found} is newer than the supported version {supported} \
                 (was this file written by a newer daemon?)"
            ),
            PersistError::Corrupt { path, detail } => write!(f, "{path}: corrupt: {detail}"),
        }
    }
}

impl std::error::Error for PersistError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PersistError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn io_err(path: &Path, source: io::Error) -> PersistError {
    PersistError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// Fail if `found` is a version this daemon cannot read.
pub fn check_version(path: &Path, found: u32, supported: u32) -> Result<(), PersistError> {
    if found > supported {
        return Err(PersistError::UnsupportedVersion {
            path: path.display().to_string(),
            found,
            supported,
        });
    }
    Ok(())
}

/// Flush a directory so a rename (or new file) inside it is durable.
fn sync_dir(dir: &Path) -> io::Result<()> {
    // Directory fsync is a no-op or unsupported on some platforms/filesystems;
    // those report an error we can safely ignore after logging.
    match File::open(dir).and_then(|d| d.sync_all()) {
        Ok(()) => Ok(()),
        Err(e) if cfg!(windows) => {
            tracing::debug!(dir = %dir.display(), error = %e, "directory fsync unsupported");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Atomically replace the file at `path` with `bytes`: write to a uniquely
/// named temporary file in the same directory, fsync it, rename it over
/// `path`, and fsync the directory. A crash at any point leaves either the
/// previous contents or the new ones; the temporary file is removed on every
/// failure path (it is dropped unless persisted).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), PersistError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    (|| -> io::Result<()> {
        let mut tmp = tempfile::Builder::new()
            .prefix(".")
            .suffix(".tmp")
            .tempfile_in(dir)?;
        tmp.write_all(bytes)?;
        tmp.as_file().sync_all()?;
        tmp.persist(path).map_err(|e| e.error)?;
        sync_dir(dir)
    })()
    .map_err(|e| io_err(path, e))
}

/// Serialize `value` as pretty JSON and [`write_atomic`] it to `path`.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), PersistError> {
    let json = serde_json::to_vec_pretty(value).map_err(|e| PersistError::Corrupt {
        path: path.display().to_string(),
        detail: format!("serialization failed: {e}"),
    })?;
    write_atomic(path, &json)
}

/// Read and parse a JSON document, returning `Ok(None)` if the file does not
/// exist. Parse failures are reported as [`PersistError::Corrupt`].
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, PersistError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err(path, e)),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| PersistError::Corrupt {
            path: path.display().to_string(),
            detail: e.to_string(),
        })
}

// ── Thread conversation log ──

/// One line of a thread's conversation log.
///
/// The log is the single source of truth for a thread's conversation: its
/// messages (each with the dedup id it was delivered under, so processed-id
/// tracking is derived from the log rather than kept in a second file) and
/// its compaction summaries, in the order they were committed.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ThreadLogEntry {
    /// Always the first line.
    Header { version: u32, thread_id: ThreadId },
    /// A committed history entry and the dedup id it was delivered under.
    Message {
        id: String,
        message: InfinityMessage,
    },
    /// A compaction summary covering the thread's first `up_to_order`
    /// messages.
    CompactionSummary { summary: String, up_to_order: i64 },
}

/// The contents of a thread log after loading.
#[derive(Debug, Default)]
pub struct LoadedThreadLog {
    pub messages: Vec<(InfinityMessage, String)>,
    pub compaction_summaries: Vec<CompactionSummary>,
}

/// A thread's append-only conversation log at a fixed path.
pub struct ThreadLog<'a> {
    pub path: &'a Path,
    pub thread_id: &'a ThreadId<str>,
}

impl ThreadLog<'_> {
    fn corrupt(&self, detail: String) -> PersistError {
        PersistError::Corrupt {
            path: self.path.display().to_string(),
            detail,
        }
    }

    fn header_line(&self) -> Vec<u8> {
        let mut line = serde_json::to_vec(&ThreadLogEntry::Header {
            version: THREAD_LOG_VERSION,
            thread_id: self.thread_id.to_owned(),
        })
        .expect("bug: header serialization failed");
        line.push(b'\n');
        line
    }

    fn encode(&self, entries: &[ThreadLogEntry]) -> Result<Vec<u8>, PersistError> {
        let mut buf = Vec::new();
        for entry in entries {
            serde_json::to_writer(&mut buf, entry)
                .map_err(|e| self.corrupt(format!("serialization failed: {e}")))?;
            buf.push(b'\n');
        }
        Ok(buf)
    }

    /// Load the log, returning `Ok(None)` if it does not exist.
    ///
    /// Recovers the longest valid prefix: a trailing line without a
    /// terminating newline is a torn write from a crash mid-append, so it is
    /// dropped (with a warning) and the file is truncated to the last
    /// complete line, keeping the on-disk log consistent with what was
    /// loaded before anything new is appended. A complete line that fails
    /// to parse is *not* a torn write and is reported as corruption.
    pub fn load(&self) -> Result<Option<LoadedThreadLog>, PersistError> {
        let bytes = match std::fs::read(self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(self.path, e)),
        };

        // Split into complete lines; whatever follows the last newline is a
        // torn tail.
        let valid_len = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        if valid_len < bytes.len() {
            tracing::warn!(
                path = %self.path.display(),
                dropped_bytes = bytes.len() - valid_len,
                "thread log has a torn trailing line (crash mid-append?); \
                 recovering the last complete prefix"
            );
            OpenOptions::new()
                .write(true)
                .open(self.path)
                .and_then(|f| f.set_len(valid_len as u64))
                .map_err(|e| io_err(self.path, e))?;
        }
        let complete = &bytes[..valid_len];

        let mut loaded = LoadedThreadLog::default();
        let mut saw_header = false;
        for (idx, line) in complete.split(|&b| b == b'\n').enumerate() {
            if line.is_empty() {
                // The final split element after the last '\n', or a stray
                // blank line.
                continue;
            }
            let entry: ThreadLogEntry = serde_json::from_slice(line)
                .map_err(|e| self.corrupt(format!("line {}: {e}", idx + 1)))?;
            match entry {
                ThreadLogEntry::Header { version, thread_id } => {
                    if saw_header {
                        return Err(self.corrupt(format!("line {}: duplicate header", idx + 1)));
                    }
                    check_version(self.path, version, THREAD_LOG_VERSION)?;
                    if thread_id != *self.thread_id {
                        return Err(self.corrupt(format!(
                            "header names thread {thread_id} but file belongs to {}",
                            self.thread_id
                        )));
                    }
                    saw_header = true;
                }
                entry if !saw_header => {
                    return Err(self.corrupt(format!(
                        "line {}: expected header, found {entry:?}",
                        idx + 1
                    )));
                }
                ThreadLogEntry::Message { id, message } => loaded.messages.push((message, id)),
                ThreadLogEntry::CompactionSummary {
                    summary,
                    up_to_order,
                } => loaded.compaction_summaries.push(CompactionSummary {
                    summary,
                    up_to_order,
                }),
            }
        }
        // An empty (or fully torn) file is indistinguishable from a missing
        // one: the header is written with the first append.
        Ok(Some(loaded))
    }

    /// Append entries durably. Creates the file (with its header) on first
    /// use. All lines are written with a single `write_all` followed by
    /// `fdatasync`, so a crash tears at most the final line. If the write
    /// itself fails, the file is truncated back to its previous length so a
    /// partial line can never end up in the middle of the log.
    pub fn append(&self, entries: &[ThreadLogEntry]) -> Result<(), PersistError> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path)
            .map_err(|e| io_err(self.path, e))?;
        let previous_len = file.metadata().map_err(|e| io_err(self.path, e))?.len();
        let mut buf = if previous_len == 0 {
            self.header_line()
        } else {
            Vec::new()
        };
        buf.extend(self.encode(entries)?);
        if let Err(e) = file.write_all(&buf).and_then(|()| file.sync_data()) {
            if let Err(truncate_err) = file.set_len(previous_len) {
                tracing::error!(
                    path = %self.path.display(),
                    error = %truncate_err,
                    "failed to roll back a partial append; the torn tail will be \
                     recovered on next load"
                );
            }
            return Err(io_err(self.path, e));
        }
        Ok(())
    }

    /// Replace the whole log (header included) atomically. Used when a thread
    /// is imported or migrated from an older on-disk format, never for
    /// ordinary appends.
    pub fn rewrite(&self, contents: &LoadedThreadLog) -> Result<(), PersistError> {
        let mut buf = self.header_line();
        let entries: Vec<ThreadLogEntry> = contents
            .messages
            .iter()
            .map(|(message, id)| ThreadLogEntry::Message {
                id: id.clone(),
                message: message.clone(),
            })
            .chain(contents.compaction_summaries.iter().map(|c| {
                ThreadLogEntry::CompactionSummary {
                    summary: c.summary.clone(),
                    up_to_order: c.up_to_order,
                }
            }))
            .collect();
        buf.extend(self.encode(&entries)?);
        write_atomic(self.path, &buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use infinity_provider_protocol::message::UserContent;

    fn user(text: &str) -> InfinityMessage {
        InfinityMessage::User {
            content: UserContent::text(text),
        }
    }

    /// Messages as `(dedup id, JSON)` pairs, for equality assertions
    /// (`InfinityMessage` is not `PartialEq`).
    fn as_json(messages: &[(InfinityMessage, String)]) -> Vec<(String, serde_json::Value)> {
        messages
            .iter()
            .map(|(m, id)| (id.clone(), serde_json::to_value(m).expect("serialize")))
            .collect()
    }

    fn msg_entry(id: &str, text: &str) -> ThreadLogEntry {
        ThreadLogEntry::Message {
            id: id.to_owned(),
            message: user(text),
        }
    }

    #[test]
    fn append_then_load_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        let log = ThreadLog {
            path: &path,
            thread_id: ThreadId::from_ref("t"),
        };
        assert!(log.load().expect("load missing").is_none());

        log.append(&[msg_entry("m1", "one"), msg_entry("m2", "two")])
            .expect("append");
        log.append(&[ThreadLogEntry::CompactionSummary {
            summary: "s".to_owned(),
            up_to_order: 2,
        }])
        .expect("append compaction");

        let loaded = log.load().expect("load").expect("exists");
        assert_eq!(
            as_json(&loaded.messages),
            as_json(&[
                (user("one"), "m1".to_owned()),
                (user("two"), "m2".to_owned())
            ])
        );
        assert_eq!(loaded.compaction_summaries.len(), 1);
        assert_eq!(loaded.compaction_summaries[0].up_to_order, 2);

        // Exactly one header line, first.
        let text = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("\"kind\":\"header\""));
        assert!(lines[0].contains(&format!("\"version\":{THREAD_LOG_VERSION}")));
    }

    #[test]
    fn torn_tail_is_dropped_and_truncated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        let log = ThreadLog {
            path: &path,
            thread_id: ThreadId::from_ref("t"),
        };
        log.append(&[msg_entry("m1", "one"), msg_entry("m2", "two")])
            .expect("append");

        // Simulate a crash mid-append: a partial third line.
        let mut f = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open for append");
        f.write_all(b"{\"kind\":\"message\",\"id\":\"m3\",\"mess")
            .expect("write partial");
        drop(f);
        let torn_len = std::fs::metadata(&path).expect("stat").len();

        let loaded = log.load().expect("load").expect("exists");
        assert_eq!(loaded.messages.len(), 2, "torn line is dropped");
        assert!(
            std::fs::metadata(&path).expect("stat").len() < torn_len,
            "file truncated to the last complete line"
        );

        // Appending after recovery yields a clean log.
        log.append(&[msg_entry("m3", "three")]).expect("append");
        let loaded = log.load().expect("load").expect("exists");
        assert_eq!(loaded.messages.len(), 3);
        assert_eq!(loaded.messages[2].1, "m3");
    }

    #[test]
    fn torn_header_recovers_to_empty_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, b"{\"kind\":\"header\",\"ver").expect("write");
        let log = ThreadLog {
            path: &path,
            thread_id: ThreadId::from_ref("t"),
        };
        let loaded = log.load().expect("load").expect("exists");
        assert!(loaded.messages.is_empty());
        // The next append rewrites the header.
        log.append(&[msg_entry("m1", "one")]).expect("append");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text.lines().count(), 2);
        assert!(text.lines().next().expect("header").contains("header"));
    }

    #[test]
    fn complete_but_corrupt_line_is_an_error_not_truncated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        let log = ThreadLog {
            path: &path,
            thread_id: ThreadId::from_ref("t"),
        };
        log.append(&[msg_entry("m1", "one")]).expect("append");
        let mut f = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open for append");
        f.write_all(b"not json\n").expect("write garbage");
        drop(f);
        let before = std::fs::metadata(&path).expect("stat").len();
        let err = log.load().expect_err("corrupt line must fail");
        assert!(matches!(err, PersistError::Corrupt { .. }), "{err}");
        assert_eq!(
            std::fs::metadata(&path).expect("stat").len(),
            before,
            "corruption is never silently truncated"
        );
    }

    #[test]
    fn newer_version_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        std::fs::write(
            &path,
            format!(
                "{{\"kind\":\"header\",\"version\":{},\"thread_id\":\"t\"}}\n",
                THREAD_LOG_VERSION + 1
            ),
        )
        .expect("write");
        let log = ThreadLog {
            path: &path,
            thread_id: ThreadId::from_ref("t"),
        };
        let err = log.load().expect_err("newer version must fail");
        assert!(
            matches!(err, PersistError::UnsupportedVersion { .. }),
            "{err}"
        );
    }

    #[test]
    fn rewrite_replaces_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        let log = ThreadLog {
            path: &path,
            thread_id: ThreadId::from_ref("t"),
        };
        log.append(&[msg_entry("old", "old")]).expect("append");
        log.rewrite(&LoadedThreadLog {
            messages: vec![(user("new"), "new".to_owned())],
            compaction_summaries: vec![],
        })
        .expect("rewrite");
        let loaded = log.load().expect("load").expect("exists");
        assert_eq!(
            as_json(&loaded.messages),
            as_json(&[(user("new"), "new".to_owned())])
        );
    }
}
