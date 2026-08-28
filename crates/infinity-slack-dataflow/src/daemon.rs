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
        session_id: String,
    },
    /// Send user input on the connection for this thread.
    SendInput {
        thread_ts: String,
        session_id: String,
        text: String,
    },
    /// Answer a choice prompt on the connection for this thread.
    AnswerChoice {
        thread_ts: String,
        choice_id: String,
        selected: usize,
    },
}

/// A message received from a daemon connection, tagged with the thread it belongs to.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DaemonEvent {
    pub thread_ts: String,
    pub message: DaemonMessage,
}
