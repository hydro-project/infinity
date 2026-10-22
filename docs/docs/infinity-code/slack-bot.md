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
  shortcuts:
    - name: Fork from here
      type: message
      callback_id: rewind_to_here
      description: Fork the conversation from this message into a new thread, edit it, and resend
    - name: Compact session
      type: message
      callback_id: compact_session
      description: Summarize this session's older conversation to free up context space

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
      - agent_session_stopped
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
# Start the daemon (if not running; any `infinity` run auto-starts it,
# or run it in the foreground with `infinity daemon`)
infinity

# Start the Slack bot
cargo run -p infinity-slack-bot
```

Or install it as a standalone binary:

```bash
cargo install --path crates/infinity-slack-bot
infinity-slack-bot
```

The bot is a single plain binary: the Hydro dataflow (from the `infinity-slack-dataflow` crate) is compiled into it at build time via Hydro's embedded mode and driven by the bot's own event loop. It connects to Slack via Socket Mode (WebSocket) — no public URL or ngrok required.

## Usage

**As an agent (recommended):** open the Infinity app from the top bar or the Agents section of Slack. Each conversation in the Messages tab is a separate agent thread backed by an Infinity session. Suggested prompts appear at the top of the Messages tab, threads are titled automatically from the session title, and tool calls are shown as task cards while the agent works.

**In channels:**

1. Invite the bot to a channel: `/invite @Infinity`
2. Send a message in the channel — the bot picks it up and starts a session.
3. Reply in the thread to continue the conversation.

Each Slack thread is a separate Infinity session. The mapping persists across bot restarts in `~/.infinity/slack_sessions.json`.

## Subagent Threads

When the agent spawns subagents (child threads), each subagent gets its own dedicated Slack thread so parallel output doesn't interleave:

- On a subagent's first output, the bot posts an anchor message in the channel ("🤖 Subagent `abc12345` — output streams in this thread.") and streams that subagent's text and tool-call cards into the thread under it.
- A cross-link to the subagent's thread is posted in the originating conversation thread, so you can follow along from the root.
- Replies you post inside a subagent's thread are delivered to that subagent (not the session root); approval buttons shown there answer the subagent's pending choice.

The root conversation thread keeps only the root agent's output and the session-level status indicator.

## Restarts and Continuity

The bot survives restarts without losing conversation continuity:

- **On shutdown** (SIGTERM or ctrl-c) the bot drains cleanly: it closes the Socket Mode connection with a proper close handshake (unACKed envelopes are redelivered by Slack after the restart, so a message racing the shutdown is delayed, never lost), flushes buffered text, finalizes every streaming message so nothing is left spinning, and persists the set of actively-streaming threads — with per-stream delivery cursors, subagent thread routes, and stop latches — to `~/.infinity/slack_resume.json`.
- **On startup** the bot reconnects to each persisted stream (subagent streams included — their output resumes in their existing dedicated threads) and replays the output generated since the cursor into a fresh streaming message, then continues live. Replies inside subagent threads keep routing to their subagent across restarts.
- **On a crash** (no clean shutdown) the bot falls back to the last periodic snapshot (written every 30 seconds while state changes), which may re-post or drop up to a few seconds of output around the crash.

## Stopping a Response

Slack shows a native **Stop** button on streaming responses — but only if the app subscribes to the `agent_session_stopped` bot event (included in the manifest above). Without the subscription, users see a non-interactive loading indicator with "Stop isn't available for this agent".

When you click Stop:

- Slack finalizes the streaming message immediately; the bot discards the rest of the current response for that thread (buffered text and tool-call cards included) and posts a brief "⏹️ Stopped" confirmation.
- The bot sends an `Interrupt` to the daemon, which cancels the in-flight generation for the stopped thread **and its subagents** — compute actually stops, pending tool calls are settled as "interrupted", and dangling approval buttons are dismissed.
- Stopping inside a subagent's dedicated thread interrupts only that subagent (and its own children); the root response and other subagents continue.
- Your next message in the thread resumes normal streaming.

**Note:** daemon-side interruption requires a daemon new enough to understand `Interrupt` (older daemons silently ignore it — the Slack-side stop still discards the output, and your next message interrupts the old generation for real).

## Forking a Conversation

Slack has no native rewind, so the bot provides a non-destructive one as a message shortcut: open the **⋮ menu on any message** in a session thread and pick **"Fork from here"** (the `rewind_to_here` message shortcut from the manifest above — the callback id predates the rename).

- Invoked on one of **your** messages, a dialog opens pre-filled with that message's text. Edit it and hit **Fork & send**: the daemon starts a **new session** containing the conversation from just before that message, and the bot opens a **new Slack thread** for it ("⏪ Forked from …", linking back), continuing with your edited message.
- Invoked on a **bot** message, the fork point is the user turn that produced it.
- The original conversation is **unchanged**: it keeps its session, any in-flight work keeps running, and you can keep using its thread. A "⏪ Forked into a new thread" cross-link is posted there.
- Not supported inside a subagent's dedicated thread (use the main conversation), and it needs a daemon new enough to understand `ForkSession`.

The fork point is found by counting the thread's human messages, which the bot forwarded as turns. If messages were sent while the bot was down, the count can be off; the daemon rejects out-of-range turns with an error.

## Switching Models

Use the `/model` slash command (responses are ephemeral — only you see them):

| Command | Effect |
|---------|--------|
| `/model` | Open an interactive model picker (falls back to a text listing with the current default marked) |
| `/model <provider_id>/<model_id>` | Switch the default model for new sessions |
| `/model <display name or model id>` | Same, matching by name (case-insensitive) |

The switch applies to sessions created afterwards; existing threads keep their model. The available model list is populated from the daemon after the first session is created.

## Compacting Context

Use the **Compact session** message shortcut (the **⋮** menu on any message in a session thread) to summarize the session's older conversation and free up context space. It works from the root conversation thread or from a subagent's dedicated thread — compaction is session-scoped either way. The ack is ephemeral (only you see it), and a "🗜️ Context compacted" note is posted in the session thread when done. The same note is posted when the daemon auto-compacts a session on its own.

A slash command is deliberately not used here: Slack does not deliver developer slash commands typed inside message threads, which would force channel-level session disambiguation instead of the one-click shortcut.

To enable the shortcut, add **Compact session** (type: message, callback ID `compact_session`) under **Interactivity & Shortcuts** in your [app settings](https://api.slack.com/apps) (or re-apply the manifest above — remembering that manifests replace rather than merge).

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| Bot doesn't respond | Check that the user's ID is in `allowed_users` (or leave the list empty) |
| "Cannot connect to daemon" | Start the daemon first (run `infinity` once, or `infinity daemon` in the foreground) |
| "apps.connections.open failed" | Verify `app_token` is correct and Socket Mode is enabled in app settings |
| "auth.test failed" | Verify `bot_token` is correct and the app is installed to the workspace |
| Bot responds but no streaming | The `assistant:write` scope is required for `chat.startStream` |
| Stop button missing ("Stop isn't available for this agent") | Subscribe to the `agent_session_stopped` bot event in **Event Subscriptions** (or re-apply the manifest above — remember manifests replace, don't merge) |
| "Fork from here" missing from the ⋮ message menu | Add the `rewind_to_here` message shortcut under **Interactivity & Shortcuts** (or re-apply the manifest above — remember manifests replace, don't merge) |
