---
sidebar_position: 6
title: Writing Custom Tools
---

# Writing Custom Tools
A **custom tool** gives an agent a Rust operation that runs inside the local process. Implement `Tool<ChannelSender>` when the operation does not need a RAP or MCP server.

The tool below looks up a build and returns its status. `execute` schedules the lookup, then returns so the agent can yield while the work runs.

```rust
use async_trait::async_trait;
use infinity_agent_core::message::{InputMessage, InputMessageContent};
use infinity_agent_core::system::local::ChannelSender;
use infinity_agent_core::tools::{Tool, ToolContext};
use infinity_agent_core::traits::InputSender;
use rig::OneOrMany;
use rig::agent::Text;
use rig::message::{ToolResult, ToolResultContent, UserContent};
use serde_json::json;

struct GetBuildStatus {
    builds: BuildClient,
}

#[async_trait]
impl Tool<ChannelSender> for GetBuildStatus {
    fn name(&self) -> &str {
        "get_build_status"
    }

    fn description(&self) -> &str {
        "Return the current status of a build."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "build_id": {
                    "type": "string",
                    "description": "Build identifier"
                }
            },
            "required": ["build_id"]
        })
    }

    fn display_script(&self) -> Option<&str> {
        Some(r#""Check build " + args.build_id"#)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        id: String,
        call_id: Option<String>,
        context: &ToolContext<ChannelSender>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let Some(build_id) = args.get("build_id").and_then(|value| value.as_str()) else {
            send_result(
                context.message_sender.clone(),
                context.group_id.clone(),
                id,
                call_id,
                "Error: build_id must be a string".to_owned(),
            )
            .await?;
            return Ok(());
        };

        let builds = self.builds.clone();
        let sender = context.message_sender.clone();
        let group_id = context.group_id.clone();
        let build_id = build_id.to_owned();

        tokio::spawn(async move {
            let text = match builds.status(&build_id).await {
                Ok(status) => format!("Build {build_id}: {status}"),
                Err(error) => format!("Error reading build {build_id}: {error}"),
            };

            if let Err(error) = send_result(sender, group_id, id.clone(), call_id, text).await {
                tracing::error!(
                    %error,
                    tool_call_id = %id,
                    "failed to deliver build status",
                );
            }
        });

        Ok(())
    }
}

async fn send_result(
    sender: ChannelSender,
    group_id: String,
    id: String,
    call_id: Option<String>,
    text: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let message = InputMessage {
        content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
            id: id.clone(),
            call_id,
            content: OneOrMany::one(ToolResultContent::Text(Text { text })),
        })),
        group_id: group_id.clone(),
        metadata: None,
        synthetic: None,
        display_as: None,
        subscription: false,
    };

    sender
        .send_to_input_queue(message, &group_id, &id)
        .await
        .map_err(Into::into)
}
```

## Implement and Register the Tool
The methods before `execute` define how the model and clients see the tool. `name` is the identifier the model calls and must be unique within the thread's toolset. `description` tells the model when to use it, and `parameters` is the JSON Schema for the argument object. `display_script` is an optional [Rhai](https://rhai.rs) expression whose `args` variable contains the tool arguments. Clients render its result, such as `Check build build-42`, instead of raw JSON.

Validate arguments again in `execute`, because the model can still produce values that do not match the schema. Return a tool result describing a recoverable input error so the model can correct its call.

The result path must preserve three values from the invocation:

- `ToolResult::id` is the `id` passed to `execute`.
- `ToolResult::call_id` preserves the optional `call_id`.
- `InputMessage::group_id` is `ToolContext::group_id`.

Send the message through `ToolContext::message_sender`. The final argument to `send_to_input_queue` is the deduplication ID. Reuse the tool-call ID when retrying this one result so the state store drops duplicate delivery.

An error returned directly from `execute` becomes a generic failed tool result. Send a descriptive error result when the model can act on the details. Once a background task has started, its failures cannot be returned from `execute`; convert operation failures into tool results and log delivery failures. `ToolContext` also includes `user_id`, the thread stack from root to current thread, and a callback URL for protocol adapters. Most local tools need only `message_sender` and `group_id`.

Use `thread_builder()` when the implementation belongs to one conversation:

```rust
let mut thread = system
    .thread_builder()
    .tool(Box::new(GetBuildStatus { builds }))
    .launch()
    .await;
```

The launched root and its subagents can call the tool. Register with `AgentSystemBuilder::tool` instead when every thread in the system should receive the same tool.

## Choose the Execution Behavior
Most tools should use the dispatched `execute` path shown above. The tool starts work, returns from `execute`, and sends its result through the input queue later. This lets the current agent slice end while network requests, subprocesses, or other asynchronous work continues.

Two trait methods change how the runtime treats a call:

| Behavior | Trait method | Use when |
|---|---|---|
| Dispatched work | Default `execute` implementation | The result arrives after asynchronous work |
| Inline result | `supports_sync()` and `execute_synchronous()` | The operation is fast and local |
| Passive wait | `is_passive()` | The unanswered call represents waiting rather than active work |

### Return an Inline Result
A **synchronous tool** returns a `ToolResult` without using the input queue. The runtime records the result and lets the model continue in the same slice. This avoids scheduling another slice for a value already available in memory.

```rust
fn supports_sync(&self) -> bool {
    true
}

async fn execute_synchronous(
    &self,
    args: &serde_json::Value,
    id: &str,
    call_id: Option<&str>,
    _context: &ToolContext<ChannelSender>,
) -> Option<ToolResult> {
    Some(ToolResult {
        id: id.to_owned(),
        call_id: call_id.map(str::to_owned),
        content: OneOrMany::one(ToolResultContent::Text(Text {
            text: calculate(args),
        })),
    })
}
```

When `supports_sync()` returns `true`, `execute_synchronous()` must return `Some`. The runtime does not call `execute` for that invocation. Keep the dispatched path for work that awaits an external system, because an inline tool keeps the current completion slice active.

### Allow Events During a Wait
An unanswered non-passive call means the agent is waiting for active work to finish. While that call is pending, the runtime defers subscription events and child reports so unrelated input does not interrupt the operation.

Return `true` from `is_passive()` when the call itself represents an idle wait:

```rust
fn is_passive(&self) -> bool {
    true
}
```

Sleep tools are passive because user input, a subscription event, or a child report should wake the agent before the timer result arrives. A build, database, or HTTP tool is not passive because its result should settle the pending call before deferred events run.

A subscription setup tool should normally remain non-passive. Its initial result establishes the subscription and settles the call; later events use the subscription path described below. Marking setup as passive would allow other events to run before the runtime has recorded that subscription.

## Deliver a Stream of Events
<a id="subscription-streams"></a>
A **subscription tool** sends an initial result with `subscription: true`, then sends tagged events for the same tool-call ID. This fits file watchers, process output, and application-internal notifications.

The following execution body starts a finite counter stream:

```rust
async fn execute(
    &self,
    args: serde_json::Value,
    id: String,
    call_id: Option<String>,
    context: &ToolContext<ChannelSender>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let updates = args
        .get("updates")
        .and_then(serde_json::Value::as_u64)
        .filter(|updates| *updates > 0)
        .ok_or("updates must be a positive integer")?;

    let started = subscription_result(
        &context.group_id,
        &id,
        call_id.clone(),
        format!("Subscribed to {updates} counter updates."),
        None,
        true,
    );
    context
        .message_sender
        .send_to_input_queue(started, &context.group_id, &id)
        .await?;

    let sender = context.message_sender.clone();
    let group_id = context.group_id.clone();
    let tool_call_id = id;

    tokio::spawn(async move {
        for sequence in 1..=updates {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;

            let event = subscription_result(
                &group_id,
                &tool_call_id,
                call_id.clone(),
                format!("Counter update: {sequence}"),
                Some(TaggedSyntheticKind::SubscriptionEvent {
                    tool_call_id: tool_call_id.clone(),
                    associative: false,
                    r#final: sequence == updates,
                }),
                false,
            );
            let dedup_id = format!("{tool_call_id}:counter:{sequence}");
            if let Err(error) = sender
                .send_to_input_queue(event, &group_id, &dedup_id)
                .await
            {
                tracing::error!(%error, %tool_call_id, sequence, "failed to deliver event");
                break;
            }
        }
    });

    Ok(())
}
```

The helper differs from `send_result` by accepting a synthetic event and subscription flag:

```rust
use infinity_agent_core::message::{SyntheticKind, TaggedSyntheticKind};

fn subscription_result(
    group_id: &str,
    id: &str,
    call_id: Option<String>,
    text: String,
    event: Option<TaggedSyntheticKind>,
    starts_subscription: bool,
) -> InputMessage {
    InputMessage {
        content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
            id: id.to_owned(),
            call_id,
            content: OneOrMany::one(ToolResultContent::Text(Text { text })),
        })),
        group_id: group_id.to_owned(),
        metadata: None,
        synthetic: event.map(SyntheticKind::Tagged),
        display_as: None,
        subscription: starts_subscription,
    }
}
```

Send and await the initial ordinary tool result before scheduling events. `subscription: true` records the tool-call ID as active; the thread idles between events, and each event wakes it on arrival. Later events use `subscription: false` and carry `TaggedSyntheticKind::SubscriptionEvent`.

Give every logical event a unique, stable deduplication ID. Reuse that ID if delivery retries. Set `final: true` on the last event so the runtime removes the subscription from active tracking.

With `associative: false`, the runtime processes each event in a temporary child thread. This keeps an open-ended stream out of the root context. Set `associative: true` only when each event belongs directly in the subscribing thread's history.

The producer task lives in the current process. A plain local tool also has no automatic cancellation signal when the built-in `cancel_subscription` removes runtime tracking. For an unbounded stream, keep task cancellation handles in an application-owned registry keyed by tool-call ID, and provide a local cancellation path that stops the producer. Persist source state separately when a subscription must resume after process restart.
