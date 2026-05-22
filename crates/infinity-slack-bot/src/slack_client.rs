use serde::{Deserialize, Serialize};

use crate::BoxError;

pub struct SlackClient {
    http: reqwest::Client,
    token: String,
    pub team_id: String,
    pub bot_user_id: String,
}

#[derive(Debug, Deserialize)]
struct SlackResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    ts: Option<String>,
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
struct AppendStreamRequest<'a> {
    channel: &'a str,
    ts: &'a str,
    markdown_text: &'a str,
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
            tracing::warn!(
                "Slack API {method} failed: {}",
                resp.error.as_deref().unwrap_or("unknown")
            );
        }
        Ok(resp)
    }

    /// Start a streaming message in a thread. Returns the message ts.
    pub async fn start_stream(
        &self,
        channel: &str,
        thread_ts: &str,
        team_id: Option<&str>,
    ) -> Result<Option<String>, BoxError> {
        let resp = self
            .api_call(
                "chat.startStream",
                &StartStreamRequest {
                    channel,
                    thread_ts,
                    recipient_team_id: team_id,
                },
            )
            .await?;
        Ok(resp.ts)
    }

    /// Append markdown text to an active stream.
    /// Returns the Slack error code if the API reports failure (e.g. stream expired).
    pub async fn append_stream(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
    ) -> Result<Option<String>, BoxError> {
        let resp = self
            .api_call(
                "chat.appendStream",
                &AppendStreamRequest {
                    channel,
                    ts,
                    markdown_text: text,
                },
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
}
