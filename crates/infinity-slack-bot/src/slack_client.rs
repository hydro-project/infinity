use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::BoxError;

pub struct SlackClient {
    http: reqwest::Client,
    token: String,
    pub team_id: String,
    pub bot_user_id: String,
    /// Set once `assistant.threads.setTitle` reports `no_permission` so we
    /// stop re-attempting it (and spamming warnings) for the process lifetime.
    /// Setting the title requires the `assistant:write` scope and the app's
    /// Agents/AI-Apps feature to be enabled; without it the call always fails.
    title_unsupported: AtomicBool,
}

#[derive(Debug, Deserialize)]
struct SlackResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    response_metadata: Option<ResponseMetadata>,
}

/// Detailed per-field validation messages Slack returns on some errors
/// (e.g. `invalid_arguments` from `views.open`).
#[derive(Debug, Deserialize)]
struct ResponseMetadata {
    #[serde(default)]
    messages: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AuthTestResponse {
    ok: bool,
    #[serde(default)]
    team_id: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct StartStreamRequest<'a> {
    channel: &'a str,
    thread_ts: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    recipient_team_id: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct StopStreamRequest<'a> {
    channel: &'a str,
    ts: &'a str,
}

#[derive(Debug, Serialize)]
struct SetStatusRequest<'a> {
    channel_id: &'a str,
    thread_ts: &'a str,
    status: &'a str,
}

#[derive(Debug, Serialize)]
struct PostMessageRequest<'a> {
    channel: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_ts: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocks: Option<&'a serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct UpdateMessageRequest<'a> {
    channel: &'a str,
    ts: &'a str,
    text: &'a str,
    blocks: &'a serde_json::Value,
}

impl SlackClient {
    pub async fn new(token: &str) -> Result<Self, BoxError> {
        let http = reqwest::Client::new();
        let resp: AuthTestResponse = http
            .post("https://slack.com/api/auth.test")
            .bearer_auth(token)
            .send()
            .await?
            .json()
            .await?;

        if !resp.ok {
            return Err(format!("auth.test failed: {}", resp.error.unwrap_or_default()).into());
        }

        let team_id = resp.team_id.unwrap_or_default();
        let bot_user_id = resp.user_id.unwrap_or_default();
        tracing::info!("authenticated as bot {bot_user_id} in team {team_id}");

        Ok(Self {
            http,
            token: token.to_owned(),
            team_id,
            bot_user_id,
            title_unsupported: AtomicBool::new(false),
        })
    }

    async fn api_call<T: Serialize>(
        &self,
        method: &str,
        body: &T,
    ) -> Result<SlackResponse, BoxError> {
        let resp = self
            .http
            .post(format!("https://slack.com/api/{method}"))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await?
            .json::<SlackResponse>()
            .await?;

        if !resp.ok {
            let detail = resp
                .response_metadata
                .as_ref()
                .filter(|m| !m.messages.is_empty())
                .map(|m| format!(" ({})", m.messages.join("; ")))
                .unwrap_or_default();
            tracing::warn!(
                "Slack API {method} failed: {}{detail}",
                resp.error.as_deref().unwrap_or("unknown")
            );
        }
        Ok(resp)
    }

    /// Start a streaming message in a thread. Returns the message ts.
    ///
    /// `chat.startStream` requires a `recipient_team_id` when streaming into a
    /// DM / assistant thread; when the caller doesn't supply one we fall back
    /// to the bot's own workspace team id (captured at `auth.test`), which is
    /// correct for single-workspace installs.
    pub async fn start_stream(
        &self,
        channel: &str,
        thread_ts: &str,
        team_id: Option<&str>,
    ) -> Result<Option<String>, BoxError> {
        let recipient_team_id = team_id
            .filter(|t| !t.is_empty())
            .or_else(|| (!self.team_id.is_empty()).then_some(self.team_id.as_str()));
        let resp = self
            .api_call(
                "chat.startStream",
                &StartStreamRequest {
                    channel,
                    thread_ts,
                    recipient_team_id,
                },
            )
            .await?;
        Ok(resp.ts)
    }

    /// Append markdown text to an active stream.
    /// Returns the Slack error code if the API reports failure (e.g. stream expired).
    ///
    /// Text is sent as a `markdown_text` *chunk* (not the top-level
    /// `markdown_text` argument) so the stream stays in "chunk mode". This lets
    /// `task_update` chunks be interleaved with text; mixing the top-level
    /// `markdown_text` argument with chunk appends triggers
    /// `streaming_mode_mismatch`.
    pub async fn append_stream(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
    ) -> Result<Option<String>, BoxError> {
        let resp = self
            .api_call(
                "chat.appendStream",
                &serde_json::json!({
                    "channel": channel,
                    "ts": ts,
                    "chunks": [{
                        "type": "markdown_text",
                        "text": text,
                    }],
                }),
            )
            .await?;
        if resp.ok {
            Ok(None)
        } else {
            Ok(resp.error)
        }
    }

    /// Stop/finalize a streaming message.
    pub async fn stop_stream(&self, channel: &str, ts: &str) -> Result<(), BoxError> {
        self.api_call("chat.stopStream", &StopStreamRequest { channel, ts })
            .await?;
        Ok(())
    }

    /// Set a status indicator on an assistant thread.
    pub async fn set_thread_status(
        &self,
        channel: &str,
        thread_ts: &str,
        status: &str,
    ) -> Result<(), BoxError> {
        self.api_call(
            "assistant.threads.setStatus",
            &SetStatusRequest {
                channel_id: channel,
                thread_ts,
                status,
            },
        )
        .await?;
        Ok(())
    }

    /// Post a regular message (fallback).
    pub async fn post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<Option<String>, BoxError> {
        let resp = self
            .api_call(
                "chat.postMessage",
                &PostMessageRequest {
                    channel,
                    text,
                    thread_ts,
                    blocks: None,
                },
            )
            .await?;
        Ok(resp.ts)
    }

    /// Post a message with Block Kit blocks.
    pub async fn post_blocks(
        &self,
        channel: &str,
        fallback_text: &str,
        blocks: &serde_json::Value,
        thread_ts: Option<&str>,
    ) -> Result<Option<String>, BoxError> {
        let resp = self
            .api_call(
                "chat.postMessage",
                &PostMessageRequest {
                    channel,
                    text: fallback_text,
                    thread_ts,
                    blocks: Some(blocks),
                },
            )
            .await?;
        Ok(resp.ts)
    }

    /// Update an existing message's text and blocks.
    pub async fn update_message(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
        blocks: &serde_json::Value,
    ) -> Result<(), BoxError> {
        self.api_call(
            "chat.update",
            &UpdateMessageRequest {
                channel,
                ts,
                text,
                blocks,
            },
        )
        .await?;
        Ok(())
    }

    /// Respond to a slash command by POSTing to its `response_url`.
    /// No bearer token is needed — the URL itself carries the authorization.
    pub async fn respond_to_command(&self, response_url: &str, text: &str) -> Result<(), BoxError> {
        let resp = self
            .http
            .post(response_url)
            .json(&serde_json::json!({
                "response_type": "ephemeral",
                "text": text,
            }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(format!("response_url POST failed with status {}", resp.status()).into());
        }
        Ok(())
    }

    /// Open a modal view via `views.open`. Requires a `trigger_id` from a
    /// recent user interaction (valid for ~3 seconds after the interaction).
    pub async fn open_view(
        &self,
        trigger_id: &str,
        view: &serde_json::Value,
    ) -> Result<(), BoxError> {
        self.api_call(
            "views.open",
            &serde_json::json!({
                "trigger_id": trigger_id,
                "view": view,
            }),
        )
        .await?;
        Ok(())
    }

    /// Set the title of an agent thread (shows in the reply bar and thread
    /// header). Requires the `assistant:write` scope and the app's Agents/AI
    /// Apps feature. If Slack reports `no_permission` we mark titling as
    /// unsupported and skip further attempts for this process.
    pub async fn set_thread_title(
        &self,
        channel: &str,
        thread_ts: &str,
        title: &str,
    ) -> Result<(), BoxError> {
        if self.title_unsupported.load(Ordering::Relaxed) {
            return Ok(());
        }
        let resp = self
            .api_call(
                "assistant.threads.setTitle",
                &serde_json::json!({
                    "channel_id": channel,
                    "thread_ts": thread_ts,
                    "title": title,
                }),
            )
            .await?;
        if !resp.ok && resp.error.as_deref() == Some("no_permission") {
            self.title_unsupported.store(true, Ordering::Relaxed);
            tracing::warn!(
                "assistant.threads.setTitle is not permitted (missing `assistant:write` \
                 scope or Agents feature disabled); disabling thread titling"
            );
        }
        Ok(())
    }

    /// Pin suggested prompts to the top of the app's Messages tab. In the
    /// Agent messaging experience no `thread_ts` is needed.
    pub async fn set_suggested_prompts(
        &self,
        channel: &str,
        title: &str,
        prompts: &serde_json::Value,
    ) -> Result<(), BoxError> {
        self.api_call(
            "assistant.threads.setSuggestedPrompts",
            &serde_json::json!({
                "channel_id": channel,
                "title": title,
                "prompts": prompts,
            }),
        )
        .await?;
        Ok(())
    }

    /// Append a task-update chunk (tool call progress) to an active stream.
    /// `status` is one of `pending`, `in_progress`, `complete`, or `error`.
    /// `details` optionally carries the tool call arguments.
    pub async fn append_stream_task(
        &self,
        channel: &str,
        ts: &str,
        task_id: &str,
        title: &str,
        status: &str,
        details: &str,
    ) -> Result<(), BoxError> {
        // Per the API, `task_update` chunk fields are flat (not nested under a
        // `task` object) and the id field is `id`. Fields are capped at 256
        // chars. `details` (the tool arguments) is included when present.
        let mut task = serde_json::json!({
            "type": "task_update",
            "id": task_id,
            "title": title,
            "status": status,
        });
        if !details.is_empty() {
            task["details"] = serde_json::Value::String(details.to_owned());
        }
        // `api_call` logs a warning if the API reports failure.
        self.api_call(
            "chat.appendStream",
            &serde_json::json!({
                "channel": channel,
                "ts": ts,
                "chunks": [task],
            }),
        )
        .await?;
        Ok(())
    }

    /// Stop/finalize a streaming message with trailing blocks (e.g. an
    /// AI-content disclaimer). Falls back to a plain stop if the blocks
    /// variant is rejected, so the stream is never left open.
    pub async fn stop_stream_with_blocks(
        &self,
        channel: &str,
        ts: &str,
        blocks: &serde_json::Value,
    ) -> Result<(), BoxError> {
        let resp = self
            .api_call(
                "chat.stopStream",
                &serde_json::json!({
                    "channel": channel,
                    "ts": ts,
                    "blocks": blocks,
                }),
            )
            .await?;
        if resp.ok {
            return Ok(());
        }
        tracing::warn!(
            "chat.stopStream with blocks failed ({}), retrying without blocks",
            resp.error.as_deref().unwrap_or("unknown")
        );
        self.stop_stream(channel, ts).await
    }
}
