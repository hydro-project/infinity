//! The mailbox-to-mailbox wire protocol.
//!
//! Peers talk over a Unix socket (the remote end reached through an SSH
//! `-L` socket forward) using length-delimited JSON [`Frame`]s. The side
//! that dials sends [`Frame::Hello`]; the listener answers
//! [`Frame::Welcome`] (or [`Frame::Reject`]). After that the link is
//! symmetric: either side sends [`Frame::Envelope`]s and acknowledges each
//! envelope it receives with a [`Frame::Ack`] once it has been handled.
//! Envelopes stay in the sender's durable outbox until acknowledged and are
//! re-sent on every reconnect, so delivery is at-least-once; receivers
//! deduplicate by envelope ID.

use serde::{Deserialize, Serialize};
use tokio_util::codec::LengthDelimitedCodec;

/// Wire protocol version, checked during the handshake.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest frame accepted from a peer.
const MAX_FRAME_LEN: usize = 4 * 1024 * 1024;

pub fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME_LEN)
        .new_codec()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    /// First frame from the dialing side: its own host name and stable host
    /// ID.
    Hello {
        name: String,
        host_id: String,
        protocol: u32,
    },
    /// The listener's answer to an acceptable [`Frame::Hello`].
    Welcome {
        name: String,
        host_id: String,
        protocol: u32,
    },
    /// The listener refuses the link; it closes the connection afterwards.
    Reject {
        reason: String,
    },
    Envelope(Envelope),
    /// The receiver has handled envelope `id`.
    Ack {
        id: String,
        outcome: Outcome,
    },
    /// Request the peer's open inboxes.
    ListInboxes {
        request_id: String,
    },
    Inboxes {
        request_id: String,
        inboxes: Vec<InboxInfo>,
    },
    /// Keepalive. Each side sends one periodically; a link that receives
    /// nothing for a while is considered dead (this catches a hung peer
    /// that SSH keepalives alone would not).
    Ping,
}

/// One message between mailboxes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// Globally unique ID (a UUID), also the user-facing message ID.
    pub id: String,
    /// The sender's inbox name, when the sending thread owns one (directly or
    /// through an ancestor thread). `None` means the sender cannot be
    /// messaged back, only replied to (for asks).
    pub from: Option<String>,
    pub body: Body,
    pub sent_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Body {
    /// A one-way message to inbox `to`.
    Tell { to: String, text: String },
    /// A question to inbox `to`; the asking tool call stays pending until a
    /// [`Body::Reply`] with `ask_id` = this envelope's ID arrives.
    Ask { to: String, text: String },
    /// The answer to the ask with envelope ID `ask_id`.
    Reply { ask_id: String, text: String },
    /// The asker gave up on the ask `ask_id` (its tool call was cancelled).
    Withdraw { ask_id: String },
    /// The ask `ask_id` will never be answered (its recipient's session was
    /// closed first); the asker's call fails with `reason`.
    Decline { ask_id: String, reason: String },
}

/// The receiver's verdict on an envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    /// Handed to the recipient's agent runtime.
    Delivered,
    /// Permanently undeliverable; the sender must not retry.
    Rejected { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub opened_at_ms: u64,
}
