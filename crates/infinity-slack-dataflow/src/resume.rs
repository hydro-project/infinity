//! Restart continuity: persistence of actively-streaming threads and replay
//! trimming.
//!
//! On clean shutdown the bot snapshots the delivery state of every stream
//! with an open response (plus the routing state needed to keep threads
//! working) into a resume file. On startup it restores that state, reconnects
//! to each stream's daemon thread, and uses the persisted
//! [`StreamCursor`](crate::runtime::StreamCursor)s to trim the daemon's
//! `Replay` down to the output that was never delivered to Slack.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use infinity_protocol::{DaemonMessage, ThreadRef};

use crate::runtime::{Runtime, StreamCursor, SubagentRoute};
use crate::BoxError;

/// One stream (root conversation or one subagent) with an open response at
/// shutdown, to be resumed on startup.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ResumeStream {
    /// The Slack thread of the root conversation.
    pub thread_ts: String,
    /// The subagent id (`ThreadRef` string form) for a subagent's stream;
    /// `None` for the root conversation's stream.
    #[serde(default)]
    pub subagent: Option<String>,
    /// The session's root thread (for reconnecting).
    pub session_id: ThreadRef,
    /// How much of the stream had been delivered to Slack.
    #[serde(default)]
    pub cursor: StreamCursor,
}

/// Everything a restarted bot needs to seamlessly continue: the streams to
/// resume plus the cross-restart routing state (channel mappings, subagent
/// thread routes, user-stop latches, posted choice buttons).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ResumeState {
    #[serde(default)]
    pub streams: Vec<ResumeStream>,
    /// thread_ts → channel id (all known threads, not just resumed ones).
    #[serde(default)]
    pub channels: HashMap<String, String>,
    /// Subagent anchor ts → route (all known routes): keeps replies inside
    /// dedicated subagent threads routed to their subagent across restarts,
    /// and prevents resumed subagent output from forking duplicate threads.
    #[serde(default)]
    pub subagent_routes: HashMap<String, SubagentRoute>,
    /// Target thread ts values latched as user-stopped.
    #[serde(default)]
    pub halted: Vec<String>,
    /// choice_id → (channel, message ts) of posted choice buttons, so a
    /// replayed pending choice is not posted twice and can still be
    /// dismissed after the restart.
    #[serde(default)]
    pub choice_messages: HashMap<String, (String, String)>,
}

impl ResumeState {
    /// True when there is nothing to resume and no routing state to carry.
    pub fn is_empty(&self) -> bool {
        self.streams.is_empty()
            && self.channels.is_empty()
            && self.subagent_routes.is_empty()
            && self.halted.is_empty()
            && self.choice_messages.is_empty()
    }
}

/// Default path of the resume state file: `~/.infinity/slack_resume.json`.
pub fn default_path() -> PathBuf {
    infinity_protocol::state_dir().join("slack_resume.json")
}

/// Load the resume state, or an empty state if the file doesn't exist.
/// A corrupt file is an error: the caller logs it and starts clean.
pub fn load(path: &Path) -> Result<ResumeState, BoxError> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(serde_json::from_str(&contents)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ResumeState::default()),
        Err(e) => Err(e.into()),
    }
}

/// Persist the resume state (atomically via a sibling temp file, so a crash
/// mid-write can't truncate the previous snapshot).
pub fn save(path: &Path, state: &ResumeState) -> Result<(), BoxError> {
    let json = serde_json::to_string_pretty(state)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Snapshot the runtime's cross-restart state. Streams whose channel or
/// session mapping is unknown are skipped (they could not be resumed).
pub fn snapshot(rt: &Runtime) -> ResumeState {
    let progress = rt
        .stream_progress
        .lock()
        .expect("bug: lock poisoned")
        .clone();
    let channels = rt.channels.lock().expect("bug: lock poisoned").clone();
    let subagent_routes = rt
        .subagent_routes
        .lock()
        .expect("bug: lock poisoned")
        .clone();
    let halted = rt
        .halted_streams
        .lock()
        .expect("bug: lock poisoned")
        .iter()
        .cloned()
        .collect();
    let choice_messages = rt
        .choice_messages
        .lock()
        .expect("bug: lock poisoned")
        .clone();

    let mut streams = Vec::new();
    {
        let sessions = rt.sessions.lock().expect("bug: lock poisoned");
        for ((thread_ts, subagent), cursor) in progress {
            if !channels.contains_key(&thread_ts) {
                tracing::warn!("not persisting stream in {thread_ts}: unknown channel");
                continue;
            }
            let Some(session_id) = sessions.get(&thread_ts).cloned() else {
                tracing::warn!("not persisting stream in {thread_ts}: unknown session");
                continue;
            };
            streams.push(ResumeStream {
                thread_ts,
                subagent,
                session_id,
                cursor,
            });
        }
    }
    // Deterministic file contents (for change detection and readable diffs).
    streams.sort_by(|a, b| (&a.thread_ts, &a.subagent).cmp(&(&b.thread_ts, &b.subagent)));

    ResumeState {
        streams,
        channels,
        subagent_routes,
        halted,
        choice_messages,
    }
}

/// Restore a loaded resume state into the runtime. The `streams` are entered
/// into both `stream_progress` (so delivery counting continues from the
/// persisted cursors) and `resuming` (so the reconnects' replays are trimmed
/// to the undelivered remainder).
pub fn restore(rt: &Runtime, state: ResumeState) {
    rt.channels
        .lock()
        .expect("bug: lock poisoned")
        .extend(state.channels);
    rt.subagent_routes
        .lock()
        .expect("bug: lock poisoned")
        .extend(state.subagent_routes);
    rt.halted_streams
        .lock()
        .expect("bug: lock poisoned")
        .extend(state.halted);
    rt.choice_messages
        .lock()
        .expect("bug: lock poisoned")
        .extend(state.choice_messages);
    let mut progress = rt.stream_progress.lock().expect("bug: lock poisoned");
    let mut resuming = rt.resuming.lock().expect("bug: lock poisoned");
    for stream in state.streams {
        let key = (stream.thread_ts, stream.subagent);
        progress.insert(key.clone(), stream.cursor.clone());
        resuming.insert(key, stream.cursor);
    }
}

/// The undelivered remainder of a replayed thread history.
#[derive(Debug)]
pub struct ReplayDelta {
    /// History events to deliver (only text/tool-call/tool-result entries;
    /// everything else in the history produces no Slack output).
    pub events: Vec<DaemonMessage>,
    /// How many synthetic `ResponseDone` messages to append after `events`.
    ///
    /// Replay history carries no `ResponseDone` entries, and the dataflow's
    /// response-end logic swallows one `ResponseDone` after a round with
    /// tool calls (more output follows a tool round). The counts here
    /// restore exactly the end-state the live event sequence would have
    /// produced:
    /// - tool still executing (unresolved trailing call): `0` — real events
    ///   will follow and the delta's tool call correctly arms the
    ///   round-continues flag;
    /// - completion in flight: `1` if the delta contained a (resolved) tool
    ///   call, else `0` — consumes the armed flag the way the pre-restart
    ///   intermediate `ResponseDone` already had, so the in-flight
    ///   completion's own `ResponseDone` behaves correctly;
    /// - thread idle: one final `ResponseDone` to close the stream, plus one
    ///   more to consume the armed flag if the delta contained a tool call.
    pub synthetic_dones: usize,
    /// Tool results that will arrive (replayed past the cursor or live) for
    /// calls *skipped* by the cursor: those calls' cards were posted and
    /// then completed-at-finalize before the restart, so their results must
    /// pair with sentinels instead of consuming the queue entries of new
    /// post-restart calls (see `Runtime::resume_orphan_results`). The
    /// delta's own replayed orphan results are included in this count — they
    /// flow through the same `ToolResult` arm as live ones.
    pub orphan_results: usize,
}

/// Round `i` up to the next UTF-8 character boundary of `s` (dropping at
/// most the tail bytes of one partially-counted character).
fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
}

/// Trim a replayed thread history to the suffix not yet delivered to Slack,
/// per the delivery `cursor` persisted at shutdown.
///
/// The cursor is **response-scoped**: it is created at the first delivery of
/// a response and dropped on the response-final stop, so its counts cover
/// only the thread's *open turn*. The replay history, by contrast, spans the
/// thread's whole life. The delta therefore considers only the suffix after
/// the last input-like row (`UserInputEcho` or `SubscriptionEvent` — the
/// events that start a turn), which is the span the cursor counted.
///
/// Replay `TextChunk`s are whole assistant messages while live chunks are
/// partial deltas, so the cursor's byte offset can fall inside a replayed
/// chunk; the chunk is sliced at that offset (live chunks concatenate to the
/// stored text, so the offset lands on a chunk — hence character — boundary
/// unless the history was rewritten underneath us, in which case we round up
/// to the next character). If the open turn shrank below the cursor (rewind
/// or compaction during downtime), the delta clamps to empty.
pub fn replay_delta(
    history: &[DaemonMessage],
    cursor: &StreamCursor,
    in_progress: bool,
) -> ReplayDelta {
    // Scope to the open turn: everything after the last input-like row.
    // (A response-scoped cursor never counts across an input boundary — the
    // response-final stop that precedes new input drops it.)
    let turn_start = history
        .iter()
        .rposition(|msg| {
            matches!(
                msg,
                DaemonMessage::UserInputEcho { .. } | DaemonMessage::SubscriptionEvent { .. }
            )
        })
        .map(|i| i + 1)
        .unwrap_or(0);
    let turn = &history[turn_start..];

    let mut events = Vec::new();
    let mut text_seen = 0usize;
    let mut calls_seen = 0usize;
    let mut results_seen = 0usize;
    let mut delta_has_tool_call = false;

    for msg in turn {
        match msg {
            DaemonMessage::TextChunk { thread_id, chunk } => {
                let start = text_seen;
                text_seen += chunk.len();
                if text_seen <= cursor.text_bytes {
                    continue; // fully delivered
                }
                let offset = ceil_char_boundary(chunk, cursor.text_bytes.saturating_sub(start));
                let remainder = &chunk[offset..];
                if !remainder.is_empty() {
                    events.push(DaemonMessage::TextChunk {
                        thread_id: thread_id.clone(),
                        chunk: remainder.to_owned(),
                    });
                }
            }
            DaemonMessage::ToolCall { .. } => {
                calls_seen += 1;
                if calls_seen > cursor.tool_calls {
                    delta_has_tool_call = true;
                    events.push(msg.clone());
                }
            }
            DaemonMessage::ToolResult { .. } => {
                results_seen += 1;
                if results_seen > cursor.tool_results {
                    events.push(msg.clone());
                }
            }
            // Info, Thinking*, ... produce no Slack output in the dataflow —
            // nothing to catch up on. (Input-like rows can't appear here:
            // the scope starts after the last one.)
            _ => {}
        }
    }

    if text_seen < cursor.text_bytes
        || calls_seen < cursor.tool_calls
        || results_seen < cursor.tool_results
    {
        tracing::warn!(
            "replayed open turn is shorter than the delivery cursor (rewound or compacted \
             during downtime); resuming with live events only"
        );
    }

    // Results for calls at-or-below the cursor pair with cards that were
    // posted (and completed-at-finalize) before the restart; they must
    // consume sentinels, not new calls' queue entries. Clamped by the turn's
    // actual call count so a rewritten history can't leave stale sentinels
    // that would swallow new calls' results.
    let orphan_results = cursor
        .tool_calls
        .min(calls_seen)
        .saturating_sub(cursor.tool_results);

    // An unresolved trailing tool call means the tool is still executing:
    // its result and the following completion round will arrive as live
    // events, so the response must not be closed synthetically. (For calls
    // skipped by the cursor this needs no round-continues flag: their
    // round's intermediate `ResponseDone` was already consumed before the
    // restart — the daemon emits it when the completion ends, before tool
    // results — so the live `ResponseDone` after the results belongs to the
    // *next* round and must behave normally.)
    let unresolved_tool = calls_seen > results_seen;
    let synthetic_dones = if unresolved_tool {
        0
    } else if in_progress {
        usize::from(delta_has_tool_call)
    } else {
        1 + usize::from(delta_has_tool_call)
    };

    ReplayDelta {
        events,
        synthetic_dones,
        orphan_results,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(chunk: &str) -> DaemonMessage {
        DaemonMessage::TextChunk {
            thread_id: Some("t1".into()),
            chunk: chunk.to_owned(),
        }
    }

    fn tool_call(name: &str) -> DaemonMessage {
        DaemonMessage::ToolCall {
            name: name.to_owned(),
            args: "{}".to_owned(),
            thread_id: Some("t1".into()),
            display_as: None,
        }
    }

    fn tool_result() -> DaemonMessage {
        DaemonMessage::ToolResult {
            segments: Vec::new(),
            thread_id: Some("t1".into()),
        }
    }

    fn user(textstr: &str) -> DaemonMessage {
        DaemonMessage::UserInputEcho {
            thread_id: Some("t1".into()),
            text: textstr.to_owned(),
        }
    }

    /// Compare event lists via their JSON form (`DaemonMessage` has no
    /// `PartialEq`).
    fn assert_events_eq(actual: &[DaemonMessage], expected: &[DaemonMessage]) {
        let a = serde_json::to_value(actual).expect("serialize actual");
        let e = serde_json::to_value(expected).expect("serialize expected");
        assert_eq!(a, e);
    }

    fn chunk_texts(delta: &ReplayDelta) -> Vec<&str> {
        delta
            .events
            .iter()
            .filter_map(|m| match m {
                DaemonMessage::TextChunk { chunk, .. } => Some(chunk.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn delta_skips_delivered_prefix_and_emits_remainder() {
        let history = vec![user("hi"), text("hello "), text("world")];
        let cursor = StreamCursor {
            text_bytes: 6, // "hello " delivered
            ..Default::default()
        };
        let delta = replay_delta(&history, &cursor, false);
        assert_eq!(chunk_texts(&delta), ["world"]);
        // Idle thread, no tool calls in delta: one final done to close.
        assert_eq!(delta.synthetic_dones, 1);
    }

    #[test]
    fn delta_slices_inside_a_replayed_chunk() {
        // Live streaming delivered partial chunks; replay has the whole text.
        let history = vec![text("hello world")];
        let cursor = StreamCursor {
            text_bytes: 6,
            ..Default::default()
        };
        let delta = replay_delta(&history, &cursor, false);
        assert_eq!(chunk_texts(&delta), ["world"]);
    }

    #[test]
    fn delta_slices_multibyte_text_on_char_boundaries() {
        // "héllo": cursor lands mid-'é' (2 bytes, offset 1..3). The slice
        // rounds up to the next boundary rather than splitting the char.
        let history = vec![text("héllo")];
        let cursor = StreamCursor {
            text_bytes: 2, // inside 'é'
            ..Default::default()
        };
        let delta = replay_delta(&history, &cursor, false);
        assert_eq!(chunk_texts(&delta), ["llo"]);
    }

    #[test]
    fn delta_trims_tool_calls_and_results_independently() {
        let history = vec![tool_call("a"), tool_result(), tool_call("b"), tool_result()];
        // Call "b" was delivered but its result wasn't.
        let cursor = StreamCursor {
            text_bytes: 0,
            tool_calls: 2,
            tool_results: 1,
        };
        let delta = replay_delta(&history, &cursor, false);
        assert_events_eq(&delta.events, &[tool_result()]);
        // The replayed result belongs to delivered call "b" (finalized card
        // from the previous process) → it pairs with a sentinel.
        assert_eq!(delta.orphan_results, 1);
        // No *delta* tool call: nothing armed the round-continues flag, so a
        // single done closes the stream.
        assert_eq!(delta.synthetic_dones, 1);
    }

    #[test]
    fn resolved_delta_tool_call_needs_an_extra_done_when_idle() {
        let history = vec![text("x"), tool_call("a"), tool_result(), text("done!")];
        let cursor = StreamCursor {
            text_bytes: 1,
            ..Default::default()
        };
        let delta = replay_delta(&history, &cursor, false);
        assert_events_eq(
            &delta.events,
            &[tool_call("a"), tool_result(), text("done!")],
        );
        // The delta's ToolCall arms the dataflow's round-continues flag; the
        // first done is consumed by it, the second closes the stream.
        assert_eq!(delta.synthetic_dones, 2);
    }

    #[test]
    fn in_flight_completion_consumes_the_armed_flag_only() {
        let history = vec![tool_call("a"), tool_result()];
        let cursor = StreamCursor::default();
        let delta = replay_delta(&history, &cursor, true);
        assert_eq!(delta.synthetic_dones, 1);
        // ... and with no delta tool call, nothing is synthesized at all.
        let delivered = StreamCursor {
            tool_calls: 1,
            tool_results: 1,
            ..Default::default()
        };
        let delta = replay_delta(&history, &delivered, true);
        assert!(delta.events.is_empty());
        assert_eq!(delta.synthetic_dones, 0);
    }

    #[test]
    fn unresolved_trailing_tool_call_defers_to_live_events() {
        let history = vec![text("let me check"), tool_call("slow_tool")];
        let cursor = StreamCursor::default();
        let delta = replay_delta(&history, &cursor, false);
        assert_events_eq(
            &delta.events,
            &[text("let me check"), tool_call("slow_tool")],
        );
        assert_eq!(delta.synthetic_dones, 0);
    }

    #[test]
    fn shrunken_history_clamps_to_empty() {
        let history = vec![text("short")];
        let cursor = StreamCursor {
            text_bytes: 1000,
            tool_calls: 3,
            tool_results: 3,
        };
        let delta = replay_delta(&history, &cursor, false);
        assert!(delta.events.is_empty());
        assert_eq!(delta.synthetic_dones, 1);
    }

    #[test]
    fn delta_scopes_to_the_open_turn() {
        // The cursor is response-scoped: earlier turns' output was delivered
        // (and their cursors dropped) long ago, so the delta must consider
        // only what follows the last input row. Here the open turn delivered
        // "par" of "partial"; nothing from the old turn may re-emit.
        let history = vec![
            user("old question"),
            text("old answer"),
            tool_call("old_tool"),
            tool_result(),
            user("new question"),
            text("partial"),
        ];
        let cursor = StreamCursor {
            text_bytes: 3, // "par" delivered
            ..Default::default()
        };
        let delta = replay_delta(&history, &cursor, true);
        assert_eq!(chunk_texts(&delta), ["tial"]);
        assert_eq!(delta.orphan_results, 0);
        assert_eq!(delta.synthetic_dones, 0);
    }

    #[test]
    fn subscription_event_starts_a_turn_like_user_input() {
        // Subagent threads wake on subscription events (child reports,
        // timers) without a user-input row; the event starts the turn.
        let history = vec![
            user("instructions"),
            text("working..."),
            DaemonMessage::SubscriptionEvent {
                name: "child report".to_owned(),
                text: "done".to_owned(),
                thread_id: Some("t1".into()),
            },
            text("resuming work"),
        ];
        let delta = replay_delta(&history, &StreamCursor::default(), false);
        assert_eq!(chunk_texts(&delta), ["resuming work"]);
    }

    #[test]
    fn orphan_results_counted_for_calls_skipped_by_the_cursor() {
        // Live incident shape (2026-10-16): 3 calls delivered, 1 result
        // delivered → two results will arrive for already-finalized cards
        // (one replayed here, one live later). Both must pair with
        // sentinels, never with new calls' queue entries.
        let history = vec![
            user("go"),
            tool_call("a"),
            tool_call("b"),
            tool_call("c"),
            tool_result(), // a's — delivered
            tool_result(), // b's — replayed orphan
                           // c's result arrives live after resume
        ];
        let cursor = StreamCursor {
            text_bytes: 0,
            tool_calls: 3,
            tool_results: 1,
        };
        let delta = replay_delta(&history, &cursor, false);
        assert_events_eq(&delta.events, &[tool_result()]);
        assert_eq!(delta.orphan_results, 2);
        // c is still executing: live events will close the response.
        assert_eq!(delta.synthetic_dones, 0);
    }

    #[test]
    fn orphan_sentinels_clamp_to_the_rewritten_turn() {
        // History rewritten under the cursor (rewind/compaction): stale
        // sentinels would swallow new calls' results — clamp to the calls
        // the turn actually has.
        let history = vec![user("go"), tool_call("only")];
        let cursor = StreamCursor {
            text_bytes: 0,
            tool_calls: 5,
            tool_results: 2,
        };
        let delta = replay_delta(&history, &cursor, false);
        assert!(delta.events.is_empty());
        // min(5, 1) - 2 saturates to zero.
        assert_eq!(delta.orphan_results, 0);
    }

    #[test]
    fn resume_state_round_trips_through_disk() {
        let path = std::env::temp_dir().join(format!(
            "slack_resume_test_{}_roundtrip.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let state = ResumeState {
            streams: vec![ResumeStream {
                thread_ts: "1.0".into(),
                subagent: Some("sub1".into()),
                session_id: "sess1".into(),
                cursor: StreamCursor {
                    text_bytes: 42,
                    tool_calls: 2,
                    tool_results: 1,
                },
            }],
            channels: [("1.0".to_owned(), "C1".to_owned())].into(),
            subagent_routes: [(
                "2.0".to_owned(),
                SubagentRoute {
                    root_thread_ts: "1.0".into(),
                    subagent_id: "sub1".into(),
                },
            )]
            .into(),
            halted: vec!["2.0".into()],
            choice_messages: [("c1".to_owned(), ("C1".to_owned(), "3.0".to_owned()))].into(),
        };
        save(&path, &state).expect("save should succeed");
        let loaded = load(&path).expect("load should succeed");
        assert_eq!(loaded.streams.len(), 1);
        assert_eq!(loaded.streams[0].thread_ts, "1.0");
        assert_eq!(loaded.streams[0].subagent.as_deref(), Some("sub1"));
        assert_eq!(loaded.streams[0].cursor.text_bytes, 42);
        assert_eq!(loaded.channels.get("1.0").map(String::as_str), Some("C1"));
        assert_eq!(loaded.halted, vec!["2.0".to_owned()]);
        assert!(loaded.choice_messages.contains_key("c1"));
        assert_eq!(loaded.subagent_routes["2.0"].subagent_id, "sub1");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_missing_file_is_empty_and_corrupt_file_is_an_error() {
        let dir = std::env::temp_dir();
        let missing = dir.join(format!(
            "slack_resume_test_{}_missing.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&missing);
        let state = load(&missing).expect("missing file loads as empty");
        assert!(state.is_empty());

        let corrupt = dir.join(format!(
            "slack_resume_test_{}_corrupt.json",
            std::process::id()
        ));
        std::fs::write(&corrupt, "{ not json").expect("write should succeed");
        assert!(
            load(&corrupt).is_err(),
            "corrupt file must surface an error"
        );
        let _ = std::fs::remove_file(&corrupt);
    }

    #[test]
    fn snapshot_and_restore_round_trip_through_runtime() {
        crate::runtime::ensure_test_init();
        let rt = crate::runtime::get();
        // Distinct ids so this test is independent of other tests sharing the
        // process-wide runtime.
        let ts = "991.1".to_owned();
        let anchor = "991.2".to_owned();

        rt.channels
            .lock()
            .expect("lock")
            .insert(ts.clone(), "C99".into());
        rt.sessions
            .lock()
            .expect("lock")
            .insert_ephemeral(ts.clone(), "sess99".into());
        rt.subagent_routes.lock().expect("lock").insert(
            anchor.clone(),
            SubagentRoute {
                root_thread_ts: ts.clone(),
                subagent_id: "sub99".into(),
            },
        );
        rt.halted_streams
            .lock()
            .expect("lock")
            .insert(anchor.clone());
        rt.stream_progress.lock().expect("lock").insert(
            (ts.clone(), None),
            StreamCursor {
                text_bytes: 7,
                tool_calls: 1,
                tool_results: 1,
            },
        );

        let state = snapshot(rt);
        assert!(state.streams.iter().any(|s| s.thread_ts == ts
            && s.subagent.is_none()
            && s.cursor.text_bytes == 7
            && s.session_id == "sess99".into()));
        assert_eq!(state.channels.get(&ts).map(String::as_str), Some("C99"));
        assert!(state.halted.contains(&anchor));

        // Restore into cleared maps and verify the resuming registry fills.
        rt.channels.lock().expect("lock").remove(&ts);
        rt.subagent_routes.lock().expect("lock").remove(&anchor);
        rt.halted_streams.lock().expect("lock").remove(&anchor);
        rt.stream_progress
            .lock()
            .expect("lock")
            .remove(&(ts.clone(), None));
        restore(rt, state);
        assert_eq!(
            rt.channels
                .lock()
                .expect("lock")
                .get(&ts)
                .map(String::as_str),
            Some("C99")
        );
        assert!(rt
            .subagent_routes
            .lock()
            .expect("lock")
            .contains_key(&anchor));
        assert!(rt.halted_streams.lock().expect("lock").contains(&anchor));
        assert_eq!(
            rt.resuming
                .lock()
                .expect("lock")
                .get(&(ts.clone(), None))
                .map(|c| c.text_bytes),
            Some(7)
        );

        // Clean up shared runtime state.
        rt.channels.lock().expect("lock").remove(&ts);
        rt.subagent_routes.lock().expect("lock").remove(&anchor);
        rt.halted_streams.lock().expect("lock").remove(&anchor);
        rt.stream_progress
            .lock()
            .expect("lock")
            .remove(&(ts.clone(), None));
        rt.resuming.lock().expect("lock").remove(&(ts, None));
    }
}
