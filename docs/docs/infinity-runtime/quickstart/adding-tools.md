---
sidebar_position: 2
title: Adding Tools
---

# Adding Tools
In this tutorial, you will give your agent a **custom tool**: an implementation of the `Tool<ChannelSender>` trait that runs inside your process. Tool servers that live outside the process connect through [RAP or MCP](./connecting-rap-and-mcp.md) instead.

The tool implementation will use three more crates, so add them to `Cargo.toml`:

```toml
async-trait = "0.1"
serde_json = "1"
tracing = "0.1"
```

The example tool looks up a build and reports its status. `execute` starts the lookup and returns immediately, so the agent can yield while the work runs; the result will arrive later as a message on the input queue.

```rust,no_run
# #[derive(Clone)]
# struct BuildClient;
# impl BuildClient {
#     async fn status(&self, _build_id: &str) -> Result<String, std::io::Error> {
#         Ok("passing".to_owned())
#     }
# }
use async_trait::async_trait;
use infinity_agent_core::ThreadId;
use infinity_agent_core::message::{InputMessage, InputMessageContent};
use infinity_agent_core::system::local::ChannelSender;
use infinity_agent_core::tools::{Tool, ToolContext};
use infinity_agent_core::traits::InputSender;
use infinity_provider_protocol::message::{Text, ToolResult, ToolResultContent, UserContent};
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
    group_id: ThreadId,
    id: String,
    call_id: Option<String>,
    text: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let message = InputMessage {
        content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
            id: id.clone(),
            call_id,
            content: vec![ToolResultContent::Text(Text { text })],
        })),
        group_id,
        metadata: None,
        synthetic: None,
        display_as: None,
        subscription: false,
    };

    sender
        .send_to_input_queue(message, &id)
        .await
        .map_err(Into::into)
}
```

## Implementing the Tool
The methods before `execute` define how the model and clients see the tool. `name` is the identifier the model calls, and it must be unique within the thread's toolset. `description` tells the model when to use the tool, and `parameters` is the JSON Schema for the argument object. `display_script` is an optional [Rhai](https://rhai.rs) expression whose `args` variable contains the tool arguments; clients will render its result (such as `Check build build-42`) instead of raw JSON.

You should validate arguments again in `execute`, because the model can produce values that do not match the schema. Returning a tool result that describes a recoverable input error lets the model correct its call.

The result path must preserve three values from the invocation:

- `ToolResult::id` is the `id` passed to `execute`.
- `ToolResult::call_id` preserves the optional `call_id`.
- `InputMessage::group_id` is `ToolContext::group_id`.

The message is sent through `ToolContext::message_sender`. The final argument to `send_to_input_queue` is the deduplication ID; if you retry delivering this one result, reuse the tool-call ID so the state store will drop the duplicate.

An error returned directly from `execute` becomes a generic failed tool result, so send a descriptive error result when the model can act on the details. Once a background task has started, its failures can no longer be returned from `execute`; convert operation failures into tool results, and log delivery failures. `ToolContext` also includes `user_id`, the thread stack from root to current thread, and a callback URL for protocol adapters, but most local tools need only `message_sender` and `group_id`.

To let a conversation call the tool, register it on the thread:

```rust,no_run
# use std::sync::Arc;
# use async_trait::async_trait;
# use infinity_agent_core::stores::{InMemoryConversationStore, InMemoryStateStore};
# use infinity_agent_core::system::local::ChannelSender;
# use infinity_agent_core::system::{AgentSystemBuilder, StaticModel};
# use infinity_agent_core::tools::{Tool, ToolContext};
# use infinity_provider_bedrock::BedrockProvider;
# #[derive(Clone)]
# struct BuildClient;
# struct GetBuildStatus {
#     builds: BuildClient,
# }
# #[async_trait]
# impl Tool<ChannelSender> for GetBuildStatus {
#     fn name(&self) -> &str {
#         "get_build_status"
#     }
#     fn description(&self) -> &str {
#         "Return the current status of a build."
#     }
#     fn parameters(&self) -> serde_json::Value {
#         serde_json::json!({ "type": "object" })
#     }
#     async fn execute(
#         &self,
#         _args: serde_json::Value,
#         _id: String,
#         _call_id: Option<String>,
#         _context: &ToolContext<ChannelSender>,
#     ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
#         let _ = &self.builds;
#         Ok(())
#     }
# }
# async fn example() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
# let provider = Arc::new(BedrockProvider::from_env());
# let model = StaticModel::new(provider, "global.anthropic.claude-sonnet-4-6").await?;
# let system = AgentSystemBuilder::new_local(
#     InMemoryConversationStore::new(),
#     InMemoryStateStore::new(),
#     model,
# )
# .start();
# let builds = BuildClient;
let mut thread = system
    .thread_builder()
    .tool(Box::new(GetBuildStatus { builds }))
    .launch()
    .await;
# let _ = &mut thread;
# Ok(())
# }
```

The launched root and its subagents can now call the tool. When every thread in the system should receive the same tool, register it with `AgentSystemBuilder::tool` instead; both paths accept the same `Box<dyn Tool<ChannelSender>>`.

## Choosing the Execution Behavior
Most tools should use the dispatched `execute` path shown above: they start the work, return from `execute`, and send the result through the input queue later. This means the current agent slice can end while network requests or subprocesses continue.

Two trait methods can opt out of this default: `supports_sync()` with `execute_synchronous()` returns the result inline for fast local operations, and `is_passive()` marks a pending call as an idle wait rather than active work.

### Returning an Inline Result
A **synchronous tool** returns a `ToolResult` without using the input queue. The runtime records the result and lets the model continue in the same slice, which avoids scheduling another slice for a value that is already available in memory.

```rust,no_run
# use async_trait::async_trait;
# use infinity_agent_core::system::local::ChannelSender;
# use infinity_agent_core::tools::{Tool, ToolContext};
# use infinity_provider_protocol::message::{Text, ToolResult, ToolResultContent};
# fn calculate(args: &serde_json::Value) -> String {
#     args.to_string()
# }
# struct Calculate;
# #[async_trait]
# impl Tool<ChannelSender> for Calculate {
#     fn name(&self) -> &str {
#         "calculate"
#     }
#     fn description(&self) -> &str {
#         "Evaluate an expression."
#     }
#     fn parameters(&self) -> serde_json::Value {
#         serde_json::json!({ "type": "object" })
#     }
#     async fn execute(
#         &self,
#         _args: serde_json::Value,
#         _id: String,
#         _call_id: Option<String>,
#         _context: &ToolContext<ChannelSender>,
#     ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
#         unreachable!("synchronous tools do not use the dispatched path")
#     }
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
        content: vec![ToolResultContent::Text(Text {
            text: calculate(args),
        })],
    })
}
# }
```

When `supports_sync()` returns `true`, `execute_synchronous()` must return `Some`, and the runtime will not call `execute` for that invocation. Keep the dispatched path for work that awaits an external system, because an inline tool keeps the current completion slice active.

### Allowing Events During a Wait
An unanswered non-passive call means that the agent is waiting for active work to finish. While such a call is pending, the runtime will defer subscription events and child reports so that unrelated input does not interrupt the operation.

Return `true` from `is_passive()` when the call itself represents an idle wait:

```rust,no_run
# use async_trait::async_trait;
# use infinity_agent_core::system::local::ChannelSender;
# use infinity_agent_core::tools::{Tool, ToolContext};
# struct WaitForDeploy;
# #[async_trait]
# impl Tool<ChannelSender> for WaitForDeploy {
#     fn name(&self) -> &str {
#         "wait_for_deploy"
#     }
#     fn description(&self) -> &str {
#         "Wait for the next deployment to finish."
#     }
#     fn parameters(&self) -> serde_json::Value {
#         serde_json::json!({ "type": "object" })
#     }
#     async fn execute(
#         &self,
#         _args: serde_json::Value,
#         _id: String,
#         _call_id: Option<String>,
#         _context: &ToolContext<ChannelSender>,
#     ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
#         Ok(())
#     }
fn is_passive(&self) -> bool {
    true
}
# }
```

Sleep tools are passive because user input, a subscription event, or a child report should wake the agent before the timer result arrives. A build, database, or HTTP tool is not passive because its result should settle the pending call before deferred events run.

A subscription setup tool should normally remain non-passive: its initial result establishes the subscription and settles the call, and later events use the subscription path described below. Marking setup as passive would allow other events to run before the runtime has recorded the subscription.

## Delivering a Stream of Events {#subscription-streams}
A **subscription tool** sends an initial result with `subscription: true`, and then sends tagged events for the same tool-call ID. This shape fits file watchers, process output, and application-internal notifications.

The following execution body starts a finite counter stream:

```rust,no_run
# use async_trait::async_trait;
# use infinity_agent_core::ThreadId;
# use infinity_agent_core::message::{
#     InputMessage, InputMessageContent, SyntheticKind, TaggedSyntheticKind,
# };
# use infinity_agent_core::system::local::ChannelSender;
# use infinity_agent_core::tools::{Tool, ToolContext};
# use infinity_agent_core::traits::InputSender;
# use infinity_provider_protocol::message::{Text, ToolResult, ToolResultContent, UserContent};
# fn subscription_result(
#     group_id: &ThreadId<str>,
#     id: &str,
#     call_id: Option<String>,
#     text: String,
#     event: Option<TaggedSyntheticKind>,
#     starts_subscription: bool,
# ) -> InputMessage {
#     InputMessage {
#         content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
#             id: id.to_owned(),
#             call_id,
#             content: vec![ToolResultContent::Text(Text { text })],
#         })),
#         group_id: group_id.to_owned(),
#         metadata: None,
#         synthetic: event.map(SyntheticKind::Tagged),
#         display_as: None,
#         subscription: starts_subscription,
#     }
# }
# struct CounterStream;
# #[async_trait]
# impl Tool<ChannelSender> for CounterStream {
#     fn name(&self) -> &str {
#         "counter_stream"
#     }
#     fn description(&self) -> &str {
#         "Stream counter updates."
#     }
#     fn parameters(&self) -> serde_json::Value {
#         serde_json::json!({ "type": "object" })
#     }
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
        .send_to_input_queue(started, &id)
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
            if let Err(error) = sender.send_to_input_queue(event, &dedup_id).await {
                tracing::error!(%error, %tool_call_id, sequence, "failed to deliver event");
                break;
            }
        }
    });

    Ok(())
}
# }
```

The `subscription_result` helper differs from `send_result` in that it accepts a synthetic event and a subscription flag:

```rust,no_run
# use infinity_agent_core::ThreadId;
# use infinity_agent_core::message::{InputMessage, InputMessageContent};
# use infinity_provider_protocol::message::{Text, ToolResult, ToolResultContent, UserContent};
use infinity_agent_core::message::{SyntheticKind, TaggedSyntheticKind};

fn subscription_result(
    group_id: &ThreadId<str>,
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
            content: vec![ToolResultContent::Text(Text { text })],
        })),
        group_id: group_id.to_owned(),
        metadata: None,
        synthetic: event.map(SyntheticKind::Tagged),
        display_as: None,
        subscription: starts_subscription,
    }
}
```

Send and await the initial ordinary tool result before scheduling events. `subscription: true` records the tool-call ID as active, so the thread can idle between events and each event will wake it on arrival. Later events use `subscription: false` and carry `TaggedSyntheticKind::SubscriptionEvent`.

Give every logical event a unique, stable deduplication ID, and reuse that ID if delivery retries. Set `final: true` on the last event so that the runtime removes the subscription from active tracking.

With `associative: false`, the runtime will process each event in a temporary child thread, which keeps an open-ended stream out of the root context. Set `associative: true` only when each event belongs directly in the subscribing thread's history.

Note that the producer task lives in the current process, and a plain local tool receives no cancellation signal when the built-in `cancel_subscription` removes runtime tracking. For an unbounded stream, keep task cancellation handles in an application-owned registry keyed by tool-call ID, and provide a local cancellation path that stops the producer. If a subscription must resume after a process restart, persist the source state separately.

Next, we will [connect external tool servers over RAP and MCP](./connecting-rap-and-mcp.md). For tools that are selected per tenant or per conversation, see [Dynamic Thread Configuration](../agent-systems/dynamic-configuration.md); the sleep, threading, and subscription tools that every agent already has are listed in [Built-in Tools](../built-in/built-in-tools.md).
