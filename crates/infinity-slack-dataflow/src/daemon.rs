//! Message types exchanged between the daemon I/O layer and the dataflow.

use std::path::PathBuf;

use infinity_protocol::DaemonMessage;

/// A command sent from the dataflow to the daemon I/O layer.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum DaemonCommand {
    /// Create a new session for this thread.
    CreateSession {
        thread_ts: String,
        cwd: PathBuf,
        model: Option<infinity_protocol::ModelRef>,
    },
    /// Connect to an existing session.
    ConnectSession {
        thread_ts: String,
        session_id: infinity_protocol::ThreadRef,
        /// The agent thread to attach to. `None` attaches to the session
        /// root; `Some` attaches to a subagent thread. Used by startup
        /// resume: a subagent whose stream was active at shutdown gets its
        /// own connection, because the daemon's replay (and the live events
        /// of an already-running subagent driver) are delivered per thread.
        #[serde(default)]
        target_thread: Option<infinity_protocol::ThreadRef>,
    },
    /// Send user input on the connection for this thread.
    SendInput {
        thread_ts: String,
        session_id: infinity_protocol::ThreadRef,
        /// The agent thread to address the input to. `None` targets the
        /// session root; `Some` targets a subagent thread (used when the
        /// user replies inside a dedicated subagent Slack thread).
        #[serde(default)]
        target_thread: Option<infinity_protocol::ThreadRef>,
        text: String,
    },
    /// Answer a choice prompt on the connection for this thread.
    AnswerChoice {
        thread_ts: String,
        choice_id: infinity_protocol::ChoiceId,
        selected: usize,
    },
    /// Interrupt in-flight generation (the user clicked Slack's native stop
    /// button). Sent on the connection for this thread; the daemon cancels
    /// the targeted thread's work and its descendant subtree without queuing
    /// any input (see [`infinity_protocol::ClientMessage::Interrupt`]).
    Interrupt {
        thread_ts: String,
        session_id: infinity_protocol::ThreadRef,
        /// The agent thread to interrupt. `None` targets the session root
        /// (stopping all of the session's work); `Some` targets a subagent
        /// thread (used when the user stopped a dedicated subagent Slack
        /// thread).
        #[serde(default)]
        target_thread: Option<infinity_protocol::ThreadRef>,
    },
    /// Fork the session's root thread into a NEW session just before its
    /// `user_turn`-th user turn (see
    /// [`infinity_protocol::ClientMessage::ForkSession`]). `thread_ts` is the
    /// fork's own (new) Slack thread: the daemon replies `Connected` for the
    /// fork on that thread's connection, after which the stashed pending
    /// input (the edited turn) is sent. `session_id` is the source session.
    ForkSession {
        thread_ts: String,
        session_id: infinity_protocol::ThreadRef,
        user_turn: usize,
    },
    /// Trigger context compaction for the session (see
    /// [`infinity_protocol::ClientMessage::TriggerCompaction`]). Emitted by
    /// the "Compact session" message shortcut; the daemon broadcasts
    /// `CompactionApplied` when done, which the dataflow surfaces as a note
    /// in the session's Slack thread.
    TriggerCompaction {
        thread_ts: String,
        session_id: infinity_protocol::ThreadRef,
    },
}

/// A message received from a daemon connection, tagged with the thread it belongs to.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DaemonEvent {
    pub thread_ts: String,
    pub message: DaemonMessage,
}
