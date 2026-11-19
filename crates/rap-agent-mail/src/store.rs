//! Durable mailbox state, persisted as one JSON file.
//!
//! Every mutation is followed by [`Store::save`], which writes a temp file
//! and renames it over `state.json`, so a crash leaves either the old or the
//! new state, never a torn file.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rap_protocol::{ProviderCallId, ThreadId, ToolCallId};
use serde::{Deserialize, Serialize};

use crate::wire::{Envelope, Outcome};

/// An open inbox: a subscription registered by `open_inbox`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inbox {
    pub owner: ThreadId,
    /// The owner's root thread (its session), so closing the session closes
    /// the inbox even when a child thread owns it.
    pub root: ThreadId,
    /// The `open_inbox` tool call; events are sent against it.
    pub tool_call_id: ToolCallId,
    pub callback_url: String,
    /// Deliver inline into the owner thread (`true`) or one child thread per
    /// message (`false`).
    pub inline: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub opened_at_ms: u64,
}

/// An inbox closed along with its session, awaiting the session's return.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosedInbox {
    pub inbox: Inbox,
    pub closed_at_ms: u64,
}

/// An envelope waiting for the peer's acknowledgement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outgoing {
    pub peer: String,
    pub envelope: Envelope,
    pub queued_at_ms: u64,
}

/// An ask this mailbox sent, whose tool call waits for the reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingAsk {
    /// The peer the ask went to; `None` for a local inbox.
    pub peer: Option<String>,
    /// The recipient as shown to the asker (`host/inbox`).
    pub to: String,
    pub group_id: ThreadId,
    /// The asking thread's root thread (its session).
    pub root: ThreadId,
    pub tool_call_id: ToolCallId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<ProviderCallId>,
    pub callback_url: String,
    pub asked_at_ms: u64,
}

/// An ask this mailbox delivered to one of its inboxes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceivedAsk {
    /// The peer the ask came from; `None` for a local sender.
    pub peer: Option<String>,
    /// The sender as shown to the recipient.
    pub from: String,
    /// The inbox that received it (replies are sent as this inbox).
    pub inbox: String,
    /// The receiving inbox's owner thread and its session when the ask
    /// arrived, so closing that thread or session declines it even after
    /// the inbox name moved on (reopened or taken over).
    pub inbox_owner: ThreadId,
    pub inbox_root: ThreadId,
    pub answered: bool,
    pub withdrawn: bool,
    /// The inbox's session closed before it answered, and the asker was
    /// told so.
    #[serde(default)]
    pub declined: bool,
    pub received_at_ms: u64,
}

/// A host that has dialed this mailbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundPeer {
    pub host_id: String,
    pub last_seen_ms: u64,
}

/// The recorded outcome of an inbound envelope, for deduplication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Seen {
    pub outcome: Outcome,
    pub at_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// This mailbox's stable identity, announced in handshakes so peers can
    /// recognize it whatever name it is reached by.
    #[serde(default)]
    pub host_id: String,
    /// Host IDs of peers this mailbox has dialed, mapped to the name it
    /// dials them by (so their inbound links get the same name).
    #[serde(default)]
    pub peer_ids: BTreeMap<String, String>,
    /// Names dialed peers announce for themselves, mapped to the name this
    /// mailbox dials them by.
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    /// Hosts that have dialed this mailbox, by the name their links are
    /// filed under, so they can be messaged (queued) while they are away.
    #[serde(default)]
    pub inbound_peers: BTreeMap<String, InboundPeer>,
    /// Inboxes closed because their session was stopped, archived, or moved
    /// away. Their subscriptions are ended (with a final event) the next
    /// time that session uses the mailbox, which it can only do once
    /// resumed; ending them earlier would be dropped by the runtime.
    #[serde(default)]
    pub closed_inboxes: Vec<ClosedInbox>,
    #[serde(default)]
    pub inboxes: BTreeMap<String, Inbox>,
    #[serde(default)]
    pub outbox: Vec<Outgoing>,
    /// Keyed by the ask envelope's ID.
    #[serde(default)]
    pub pending_asks: BTreeMap<String, PendingAsk>,
    /// Keyed by the ask envelope's ID.
    #[serde(default)]
    pub received_asks: BTreeMap<String, ReceivedAsk>,
    /// Keyed by envelope ID.
    #[serde(default)]
    pub seen: BTreeMap<String, Seen>,
}

pub struct Store {
    path: PathBuf,
    pub state: State,
}

impl Store {
    /// Load `state.json` from `dir`. A missing file starts empty; an
    /// unreadable one is moved aside (so it can be inspected) and the
    /// mailbox starts empty rather than refusing to run.
    pub fn load(dir: &std::path::Path) -> Self {
        let path = dir.join("state.json");
        let state = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(state) => state,
                Err(e) => {
                    let aside = dir.join(format!("state.json.corrupt-{}", now_ms()));
                    tracing::error!(
                        "{} is unreadable ({e}); moving it to {} and starting empty",
                        path.display(),
                        aside.display()
                    );
                    if let Err(e) = std::fs::rename(&path, &aside) {
                        tracing::error!("failed to move corrupt state aside: {e}");
                    }
                    State::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => {
                tracing::error!("failed to read {}: {e}; starting empty", path.display());
                State::default()
            }
        };
        let mut store = Self { path, state };
        if store.state.host_id.is_empty() {
            store.state.host_id = uuid::Uuid::new_v4().to_string();
            store.save();
        }
        store
    }

    /// Persist the state. Failures are logged: the in-memory state stays
    /// authoritative for this process, and the next save retries.
    pub fn save(&self) {
        let tmp = self.path.with_extension("json.tmp");
        let result = serde_json::to_vec(&self.state)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&tmp, bytes))
            .and_then(|()| std::fs::rename(&tmp, &self.path));
        if let Err(e) = result {
            tracing::error!("failed to save {}: {e}", self.path.display());
        }
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}
