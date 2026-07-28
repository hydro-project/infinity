---
sidebar_position: 7
title: Slack Bot
---

# Slack Bot

The Infinity Slack Bot connects Infinity Code to a Slack workspace, letting you interact with the agent from any Slack channel or thread. Each Slack thread maps to an Infinity session — start a conversation in a channel and the agent maintains context across replies.

## Creating the Slack App

1. Go to [api.slack.com/apps](https://api.slack.com/apps) and click **Create New App** → **From a manifest**.

2. Select your workspace, then paste this manifest (YAML):

```yaml
display_information:
  name: Infinity
  description: Infinity Code AI agent
  background_color: "#1a1a2e"

features:
  app_home:
    messages_tab_enabled: true
    messages_tab_read_only_enabled: false
  bot_user:
    display_name: Infinity
    always_online: true
  agent_view:
    agent_description: Infinity Code AI agent — chat with the agent to read, write, and review code in your repositories.
  slash_commands:
    - command: /model
      description: List available models or switch the default for new sessions
      usage_hint: "[provider_id/model_id]"
      should_escape: false

oauth_config:
  scopes:
    bot:
      - channels:history
      - groups:history
      - im:history
      - mpim:history
      - chat:write
      - chat:write.public
      - assistant:write
      - commands

settings:
  event_subscriptions:
    bot_events:
      - app_home_opened
      - app_context_changed
      - message.channels
      - message.groups
      - message.im
      - message.mpim
  interactivity:
    is_enabled: true
  org_deploy_enabled: false
  socket_mode_enabled: true
  token_rotation_enabled: false
```

:::warning Manifests replace, don't merge

Applying a manifest resets any settings not present in the YAML, including ones enabled through the dashboard UI. Before applying, export your app's current manifest from **App Settings → App Manifest** and diff it against this one.

Also note that switching from `assistant_view` to `agent_view` is **irreversible** — once applied, the app cannot go back to the Assistant messaging experience.

:::

3. Click **Create**.

## Generating Tokens

You need two tokens:

### Bot Token (`xoxb-...`)

1. In your app settings, go to **OAuth & Permissions**.
2. Click **Install to Workspace** and authorize.
3. Copy the **Bot User OAuth Token** (starts with `xoxb-`).

### App-Level Token (`xapp-...`)

1. Go to **Basic Information** → **App-Level Tokens**.
2. Click **Generate Token and Scopes**.
3. Name it (e.g. `socket-mode`), add the `connections:write` scope.
4. Click **Generate** and copy the token (starts with `xapp-`).

## Configuration

Create `~/.infinity/slack.json`:

```json
{
  "bot_token": "xoxb-your-bot-token",
  "app_token": "xapp-your-app-level-token",
  "default_cwd": "/home/you/your-repo",
  "allowed_users": ["U01ABCDEF"],
  "default_model": { "provider_id": "bedrock", "model_id": "claude-sonnet-4" }
}
```

| Field | Description |
|-------|-------------|
| `bot_token` | Bot User OAuth Token (`xoxb-...`) |
| `app_token` | App-Level Token for Socket Mode (`xapp-...`) |
| `default_cwd` | Working directory for new agent sessions |
| `allowed_users` | Slack user IDs permitted to use the bot (empty = allow all) |
| `default_model` | Optional model for new sessions (omit to use the daemon's default) |

To find your Slack user ID: click your profile in Slack → **⋮** → **Copy member ID**.

## Running

Start the Infinity daemon first (if not already running), then start the Slack bot:

```bash
# Start the daemon (if not running)
infinity --daemon

# Start the Slack bot
infinity-slack-bot
```

The bot connects via Socket Mode (WebSocket) — no public URL or ngrok required.

## Usage

**As an agent (recommended):** open the Infinity app from the top bar or the Agents section of Slack. Each conversation in the Messages tab is a separate agent thread backed by an Infinity session. Suggested prompts appear at the top of the Messages tab, threads are titled automatically from the session title, and tool calls are shown as task cards while the agent works.

**In channels:**

1. Invite the bot to a channel: `/invite @Infinity`
2. Send a message in the channel — the bot picks it up and starts a session.
3. Reply in the thread to continue the conversation.

Each Slack thread is a separate Infinity session. The mapping persists across bot restarts in `~/.infinity/slack_sessions.json`.

## Switching Models

Use the `/model` slash command (responses are ephemeral — only you see them):

| Command | Effect |
|---------|--------|
| `/model` | List available models, with the current default marked |
| `/model <provider_id>/<model_id>` | Switch the default model for new sessions |
| `/model <display name or model id>` | Same, matching by name (case-insensitive) |

The switch applies to sessions created afterwards; existing threads keep their model. The available model list is populated from the daemon after the first session is created.

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| Bot doesn't respond | Check that the user's ID is in `allowed_users` (or leave the list empty) |
| "Cannot connect to daemon" | Start the daemon with `infinity --daemon` first |
| "apps.connections.open failed" | Verify `app_token` is correct and Socket Mode is enabled in app settings |
| "auth.test failed" | Verify `bot_token` is correct and the app is installed to the workspace |
| Bot responds but no streaming | The `assistant:write` scope is required for `chat.startStream` |
