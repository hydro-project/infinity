use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio_util::codec::LengthDelimitedCodec;

/// The identifier kinds used throughout the runtime (re-exported from
/// `rap-protocol`).
pub use rap_protocol::{ChoiceId, ProviderCallId, ThreadId, ToolCallId};

strkind::strkind! {
    /// The name of a configured remote daemon (from `remotes.json`), e.g.
    /// `"devbox"`. Remote names must not contain `/` (they are joined with
    /// thread IDs in [`ThreadRef`]'s wire encoding).
    pub RemoteName;
}

/// A client-facing reference to a thread: which daemon it lives on
/// (`remote: None` means the local daemon) plus its thread ID there.
///
/// On the wire this serializes as the historical composite string —
/// `"{remote}/{id}"` for remote threads, bare `"{id}"` for local ones — so
/// clients see a plain string and the daemon parses it back losslessly.
/// A session is identified by its root thread's `ThreadRef`.
///
/// Ordering is `(remote, id)`: local threads sort before remote ones, then
/// by thread ID. (This differs from lexicographic ordering of the composite
/// string, which would interleave remotes with local IDs.)
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ThreadRef {
    /// The remote daemon hosting the thread; `None` for the local daemon.
    pub remote: Option<RemoteName>,
    /// The thread's ID on its home daemon.
    pub id: ThreadId,
}

impl ThreadRef {
    /// A reference to a thread on the local daemon.
    pub fn local(id: ThreadId) -> Self {
        Self { remote: None, id }
    }

    /// A reference to a thread on the named remote daemon.
    ///
    /// # Panics
    ///
    /// Panics if `remote` contains `/`, which would make the composite wire
    /// encoding (`"{remote}/{id}"`) ambiguous.
    pub fn remote(remote: RemoteName, id: ThreadId) -> Self {
        assert!(
            !remote.as_str().contains('/'),
            "bug: remote name {remote:?} contains '/'"
        );
        Self {
            remote: Some(remote),
            id,
        }
    }

    /// Re-home this reference onto `remote` (used when a daemon proxies a
    /// remote daemon's messages to its own clients).
    ///
    /// # Panics
    ///
    /// Panics if `remote` contains `/`, which would make the composite wire
    /// encoding (`"{remote}/{id}"`) ambiguous.
    pub fn prefixed(self, remote: &RemoteName) -> Self {
        Self::remote(remote.clone(), self.id)
    }

    /// Strip the given remote, yielding the thread's ID on its home daemon.
    /// References to other remotes (or local ones) are returned unchanged.
    pub fn strip(self, remote: &RemoteName) -> Self {
        match self.remote {
            Some(ref r) if r == remote => Self {
                remote: None,
                id: self.id,
            },
            _ => self,
        }
    }
}

impl std::fmt::Display for ThreadRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.remote {
            Some(remote) => write!(f, "{}/{}", remote, self.id),
            None => write!(f, "{}", self.id),
        }
    }
}

/// Error parsing a [`ThreadRef`] from its composite string form.
///
/// Produced when the input is not a bare thread ID or a well-formed
/// `"{remote}/{id}"` composite: an empty remote or ID half, or a second `/`
/// (remote names and thread IDs must not contain `/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRefParseError {
    input: String,
    reason: &'static str,
}

impl std::fmt::Display for ThreadRefParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid thread ref {:?}: {}", self.input, self.reason)
    }
}

impl std::error::Error for ThreadRefParseError {}

impl std::str::FromStr for ThreadRef {
    type Err = ThreadRefParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = |reason| {
            Err(ThreadRefParseError {
                input: s.to_owned(),
                reason,
            })
        };
        match s.split_once('/') {
            Some((remote, id)) => {
                if remote.is_empty() {
                    return err("empty remote name");
                }
                if id.is_empty() {
                    return err("empty thread ID");
                }
                if id.contains('/') {
                    return err("more than one '/'");
                }
                Ok(Self {
                    remote: Some(remote.into()),
                    id: id.into(),
                })
            }
            None => Ok(Self {
                remote: None,
                id: s.into(),
            }),
        }
    }
}

impl From<&str> for ThreadRef {
    fn from(s: &str) -> Self {
        s.parse().expect("invalid ThreadRef")
    }
}

impl From<String> for ThreadRef {
    fn from(s: String) -> Self {
        s.as_str().into()
    }
}

impl From<ThreadId> for ThreadRef {
    fn from(id: ThreadId) -> Self {
        Self::local(id)
    }
}

impl Serialize for ThreadRef {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ThreadRef {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Maximum frame size for daemon ↔ client communication (256 MiB).
///
/// The default `LengthDelimitedCodec` limit is 8 MiB, which is too small for
/// messages that carry large tool outputs, file contents, or replayed
/// conversation history. We use a generous 256 MiB limit to avoid "frame size
/// too big" errors in practice.
const MAX_FRAME_LENGTH: usize = 256 * 1024 * 1024;

/// Create a [`LengthDelimitedCodec`] configured with the project-wide maximum
/// frame size. Use this instead of `LengthDelimitedCodec::new()` to ensure
/// both client and daemon agree on the limit.
pub fn length_delimited_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME_LENGTH)
        .new_codec()
}

/// Returns the path to the daemon unix socket: `~/.infinity/daemon.sock`.
pub fn socket_path() -> PathBuf {
    dirs::home_dir()
        .expect("could not determine home directory")
        .join(".infinity")
        .join("daemon.sock")
}

/// Returns the path to the daemon PID file: `~/.infinity/daemon.pid`.
pub fn pid_path() -> PathBuf {
    dirs::home_dir()
        .expect("could not determine home directory")
        .join(".infinity")
        .join("daemon.pid")
}

/// Returns the base directory for daemon state: `~/.infinity/`.
pub fn state_dir() -> PathBuf {
    dirs::home_dir()
        .expect("could not determine home directory")
        .join(".infinity")
}

/// Returns the path to the remotes config: `~/.infinity/remotes.json`.
pub fn remotes_config_path() -> PathBuf {
    dirs::home_dir()
        .expect("could not determine home directory")
        .join(".infinity")
        .join("remotes.json")
}

// ── Client → Daemon ─────────────────────────────────────────────────────────

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Create a new session with the given working directory.
    CreateSession {
        cwd: PathBuf,
        /// Optional target location. `None` means local.
        /// Otherwise, the name of a remote.
        #[serde(default)]
        location: Option<RemoteName>,
        /// Optional model to use for the new session. `None` uses the
        /// daemon's default model.
        #[serde(default)]
        model: Option<ModelRef>,
        /// When `false`, this connection will not prevent the session from
        /// going idle and being shut down by the daemon. Defaults to `true`
        /// so that normal interactive clients keep sessions alive.
        #[serde(default = "default_true")]
        keeps_session_alive: bool,
    },
    /// Connect to an existing session (optionally a specific thread).
    Connect {
        root_thread_id: ThreadRef,
        thread_id: Option<ThreadRef>,
        /// When `false`, this connection will not prevent the session from
        /// going idle and being shut down by the daemon. Defaults to `true`
        /// so that normal interactive clients keep sessions alive.
        #[serde(default = "default_true")]
        keeps_session_alive: bool,
    },
    UserInput {
        thread_id: ThreadRef,
        text: String,
    },
    /// Disconnect from the session while letting the agent continue to run in the background.
    Disconnect,
    /// Immediately attempt to detach. If the agent is idle, the daemon shuts
    /// down the session (closing the display channel). If not idle, the daemon
    /// responds with `DisconnectNotIdle` so the client can show a picker.
    SoftDetach {
        root_thread_id: ThreadRef,
    },
    /// Disconnects from the session and shuts down the agent so that it can only be woken bu
    /// new user inputs.
    ShutdownSession {
        root_thread_id: ThreadRef,
    },
    /// Archive a session (shut it down and hide from the main list).
    ArchiveSession {
        root_thread_id: ThreadRef,
    },
    /// Switch the model used for future requests on a thread. `thread_id`
    /// may be any thread (root or subthread); the switch affects only that
    /// specific thread — it does not propagate to child threads. If a
    /// completion is currently in flight on the thread, it finishes on the
    /// old model and the switch applies to subsequent requests.
    SwitchModel {
        thread_id: ThreadRef,
        model: ModelRef,
    },
    /// Notify the daemon that a user choice was answered so it can be
    /// removed from the pending replay list.
    UserChoiceAnswered {
        choice_id: ChoiceId,
        selected: usize,
    },
    /// Trigger compaction for the given session.
    TriggerCompaction {
        root_thread_id: ThreadRef,
    },
    /// Request migration of a session to a different host.
    RequestMigrate {
        root_thread_id: ThreadRef,
        /// `None` means local, `Some(name)` means a remote.
        #[serde(default)]
        to: Option<RemoteName>,
        dest_cwd: PathBuf,
    },
    /// Daemon-to-daemon: request a session to emigrate. Includes destination RAP URLs
    /// so source RAP servers can migrate their state.
    Emigrate {
        root_thread_id: ThreadId,
        /// config_id → destination URL
        dest_rap_urls: HashMap<String, String>,
    },
    /// Daemon-to-daemon: immigration is complete, source can clean up.
    EmigrateDone {
        root_thread_id: ThreadId,
    },
    /// Daemon-to-daemon: import a serialized session at the given cwd.
    ImportSession {
        root_thread_id: ThreadId,
        cwd: PathBuf,
        session_data: String,
    },
    /// Daemon-to-daemon: boot RAP servers at the given cwd and return their ports.
    BootRapServers {
        cwd: PathBuf,
    },
    /// Request directory listing for path completion.
    ListDirectory {
        path: String,
        /// Target remote name. `None` means list on the local filesystem.
        #[serde(default)]
        on: Option<RemoteName>,
    },
}

// ── Daemon → Client ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DaemonMessage {
    Connected {
        root_thread_id: ThreadRef,
        thread_id: ThreadRef,
        model_name: String,
        context_window: usize,
        title: Option<String>,
        total_tokens_used: usize,
        #[serde(default)]
        provider_id: String,
    },
    StartOutput {
        thread_id: Option<ThreadRef>,
    },
    TextChunk {
        thread_id: Option<ThreadRef>,
        chunk: String,
    },
    ToolCall {
        name: String,
        args: String,
        thread_id: Option<ThreadRef>,
        display_as: Option<String>,
    },
    ToolResult {
        /// Prioritized display segments. Clients use the first type they support.
        segments: Vec<rap_protocol::DisplaySegment>,
        thread_id: Option<ThreadRef>,
    },
    Info {
        thread_id: Option<ThreadRef>,
        text: String,
    },
    ResponseDone {
        thread_id: Option<ThreadRef>,
        token_usage: Option<TokenUsage>,
    },
    UserInputEcho {
        thread_id: Option<ThreadRef>,
        text: String,
    },
    SubscriptionEvent {
        name: String,
        text: String,
        thread_id: Option<ThreadRef>,
    },
    OAuthRequired {
        thread_id: Option<ThreadRef>,
        auth_url: String,
    },
    UserChoiceRequired {
        thread_id: Option<ThreadRef>,
        id: ChoiceId,
        prompt: String,
        choices: Vec<String>,
        default: usize,
    },
    UserChoiceComplete {
        choice_id: ChoiceId,
    },
    ThinkingStart {
        thread_id: Option<ThreadRef>,
    },
    ThinkingEnd {
        thread_id: Option<ThreadRef>,
    },
    ThinkingChunk {
        thread_id: Option<ThreadRef>,
        chunk: String,
    },
    CompactionApplied {
        thread_id: Option<ThreadRef>,
    },
    /// Confirmation that a thread's model was switched (via
    /// [`ClientMessage::SwitchModel`]). Sent to the requesting client and
    /// broadcast to the thread's subscribers so every attached UI can update
    /// its model indicator.
    ModelSwitched {
        thread_id: ThreadRef,
        model_name: String,
        context_window: usize,
        provider_id: String,
    },
    Error {
        thread_id: Option<ThreadRef>,
        text: String,
    },
    /// A view update pushed by a RAP tool server.
    ViewUpdate {
        thread_id: Option<ThreadRef>,
        view_type: String,
        content: serde_json::Value,
    },
    /// Batch replay of history messages, sent on connect/load.
    Replay {
        history: Vec<DaemonMessage>,
        pending_choices: Vec<DaemonMessage>,
        #[serde(default)]
        views: HashMap<String, serde_json::Value>,
        /// Whether a completion is currently in flight for this thread. When
        /// false, clients should treat the end of the replay as an implicit
        /// ResponseDone (a trailing unresolved ToolCall in the history still
        /// implies a "waiting for tool result" state); when true, the
        /// spinner state implied by the end of the history is live and more
        /// events will follow.
        #[serde(default)]
        in_progress: bool,
    },
    /// Sent immediately on socket connection with session list and default model info.
    Welcome {
        sessions: HashMap<ThreadRef, SessionInfo>,
        available_models: Vec<ModelInfo>,
        default_model_name: String,
        default_context_window: usize,
        provider_name: String,
        #[serde(default)]
        remotes: Vec<RemoteInfo>,
    },
    /// Broadcast: one or more sessions were created or updated.
    SessionsUpdated {
        sessions: HashMap<ThreadRef, SessionInfo>,
    },
    /// Broadcast: remote connection statuses changed.
    RemotesUpdated {
        remotes: Vec<RemoteInfo>,
    },
    /// The agent is not idle — the client should show the full quit picker UI.
    DisconnectNotIdle,
    /// The agent was idle and has been detached — the client can proceed with
    /// its pending action (quit, switch, new session) without showing a picker.
    DetachedIdle,
    /// Response to Emigrate: serialized session data (thread tree as JSON).
    EmigrateResult {
        root_thread_id: ThreadId,
        session_data: String,
    },
    MigrateStarted {
        root_thread_id: ThreadRef,
    },
    MigrateComplete {
        root_thread_id: ThreadRef,
        new_root_thread_id: ThreadRef,
    },
    MigrateError {
        root_thread_id: ThreadRef,
        error: String,
    },
    /// Response to ImportSession.
    ImportComplete {
        root_thread_id: ThreadId,
    },
    /// Response to BootRapServers: maps config ID → local port for servers needing migration.
    RapServersBooted {
        /// config_id → port on the remote host (only servers with needsMigration)
        server_ports: HashMap<String, u16>,
    },
    /// Response to ListDirectory: directory entries for path completion.
    DirectoryListing {
        /// The path that was requested (for matching responses to requests).
        request_path: String,
        /// Directory entries (names only, directories have trailing `/`).
        entries: Vec<String>,
        /// The remote that was queried, if any.
        #[serde(default)]
        on: Option<RemoteName>,
    },
}

// ── Supporting types ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionStatus {
    Running,
    Idle,
    Stopped,
    WaitingForChoice,
    Migrating,
    Archived,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub title: Option<String>,
    pub last_updated: String,
    pub total_tokens_used: usize,
    pub status: SessionStatus,
    #[serde(default)]
    pub threads: Vec<SubthreadInfo>,
    /// If set, this session lives on a remote daemon with this name.
    #[serde(default)]
    pub remote: Option<RemoteName>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubthreadInfo {
    pub thread_id: ThreadRef,
    pub parent_thread_id: ThreadRef,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Total tokens including cached input. When prompt caching is active,
    /// `input_tokens` only reflects uncached input, so consumers should prefer
    /// `total_tokens` (falling back to `input + output` if absent for
    /// backwards compatibility).
    #[serde(default)]
    pub total_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub display_name: String,
    /// The provider this model belongs to.
    #[serde(default)]
    pub provider_id: String,
    pub model_id: String,
    pub context_window: usize,
}

/// Globally unique reference to a model: a provider id plus the model's
/// provider-scoped id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteInfo {
    pub name: RemoteName,
    pub status: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_ref_parse_local() {
        let r: ThreadRef = "abc-123".parse().expect("local ref should parse");
        assert_eq!(r, ThreadRef::local("abc-123".into()));
        assert_eq!(r.to_string(), "abc-123");
    }

    #[test]
    fn thread_ref_parse_remote() {
        let r: ThreadRef = "devbox/abc-123".parse().expect("remote ref should parse");
        assert_eq!(r, ThreadRef::remote("devbox".into(), "abc-123".into()));
        assert_eq!(r.to_string(), "devbox/abc-123");
    }

    #[test]
    fn thread_ref_parse_rejects_malformed() {
        assert!("a/b/c".parse::<ThreadRef>().is_err(), "second '/'");
        assert!("/abc".parse::<ThreadRef>().is_err(), "empty remote");
        assert!("devbox/".parse::<ThreadRef>().is_err(), "empty thread ID");
    }

    #[test]
    fn thread_ref_deserialize_rejects_malformed() {
        assert!(serde_json::from_str::<ThreadRef>("\"a/b/c\"").is_err());
        let r: ThreadRef =
            serde_json::from_str("\"devbox/abc-123\"").expect("valid composite should deserialize");
        assert_eq!(r, ThreadRef::remote("devbox".into(), "abc-123".into()));
    }

    #[test]
    #[should_panic(expected = "contains '/'")]
    fn thread_ref_remote_rejects_slash_in_name() {
        ThreadRef::remote("dev/box".into(), "abc-123".into());
    }
}
