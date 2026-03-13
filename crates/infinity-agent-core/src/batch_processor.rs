//! Shared batch input processing and completion logic.
//!
//! [`process_batch`] processes an iterator of input messages, emits
//! [`DisplayEvent`]s, and when any input is actionable returns a pinned
//! completion future the caller can store (CLI) or immediately `.await`
//! (Lambda).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;

use futures_util::StreamExt;
use rig::completion::{CompletionModel, ToolDefinition};
use rig::message::{AssistantContent, Message, ToolResultContent, UserContent};
use tokio::sync::{mpsc, oneshot};

use crate::event_processor::{self, CompletionAction, HistoryManager};
use crate::message::{InputMessage, InputMessageContent};
use crate::rap_notifier::RapNotifier;
use crate::tools::{Tool, ToolContext};
use crate::traits::{ConversationStore, HttpClient, InputSender, StateStore};

/// Events emitted during batch processing and completion for display purposes.
///
/// Generic over `R`, the model's streaming response type (used only in
/// [`ResponseDone`](DisplayEvent::ResponseDone) to carry usage information).
pub enum DisplayEvent<R> {
    StartOutput {
        prefix: Option<String>,
    },
    TextChunk {
        prefix: Option<String>,
        chunk: String,
    },
    ToolCall {
        name: String,
        args: serde_json::Value,
        prefix: Option<String>,
    },
    ToolResult {
        text: String,
        display_as: Option<String>,
        prefix: Option<String>,
    },
    Info(String),
    ResponseDone(Option<String>, R),
    UserInput(String),
    SubscriptionEvent {
        name: String,
        text: String,
        prefix: Option<String>,
    },
    OAuthRequired {
        auth_url: String,
    },
    ThinkingStart,
    ThinkingEnd,
    ThinkingChunk {
        prefix: Option<String>,
        chunk: String,
    },
}

/// Process a single input message: run prepare_input and emit display events.
/// Returns `Some(message_id)` if the item is ready for completion, `None` otherwise.
async fn process_input_item<C, S, R>(
    input_msg: InputMessage,
    message_id: String,
    current_history: &mut HistoryManager<C, S>,
    conversation_store: &C,
    display_tx: &mpsc::UnboundedSender<DisplayEvent<R>>,
) -> Option<String>
where
    C: ConversationStore,
    S: StateStore,
{
    let prepare_result = event_processor::prepare_input(
        input_msg.clone(),
        message_id.clone(),
        current_history,
        conversation_store,
    )
    .await;

    match prepare_result {
        Ok(event_processor::PrepareResult::Handled) => None,
        Ok(event_processor::PrepareResult::OAuthRequired { auth_url }) => {
            let _ = display_tx.send(DisplayEvent::OAuthRequired { auth_url });
            None
        }
        Err(e) => {
            let _ = display_tx.send(DisplayEvent::Info(format!("Error: {}", e)));
            None
        }
        Ok(event_processor::PrepareResult::Ready) => {
            // Echo to the terminal — subscription events vs user input.
            if let Some(synth) = input_msg.synthetic.as_ref() {
                if let InputMessageContent::User(UserContent::ToolResult(res)) = &input_msg.content
                    && let ToolResultContent::Text(text) = res.content.first()
                {
                    let orig_call = current_history.get_history().into_iter().find(|h| {
                        if let Message::Assistant { content, .. } = h
                            && let AssistantContent::ToolCall(c) = content.first()
                        {
                            c.id == synth.tool_call_id()
                        } else {
                            false
                        }
                    });

                    if let Some(h) = orig_call
                        && let Message::Assistant { content, .. } = h
                        && let AssistantContent::ToolCall(c) = content.first()
                    {
                        let _ = display_tx.send(DisplayEvent::SubscriptionEvent {
                            name: format!("{}({})", c.function.name, c.function.arguments),
                            text: text.text,
                            prefix: current_history.get_thread_nesting_prefix(),
                        });
                    }
                }
            } else if let InputMessageContent::User(UserContent::ToolResult(res)) =
                &input_msg.content
                && let ToolResultContent::Text(text) = res.content.first()
            {
                let _ = display_tx.send(DisplayEvent::ToolResult {
                    text: text.text,
                    display_as: input_msg.display_as.clone(),
                    prefix: current_history.get_thread_nesting_prefix(),
                });
            } else if let InputMessageContent::User(UserContent::Text(ref text)) = input_msg.content
            {
                let display_text = text.text.strip_prefix("<interrupt>").unwrap_or(&text.text);
                let _ = display_tx.send(DisplayEvent::UserInput(display_text.to_string()));
            }

            Some(message_id)
        }
    }
}

/// Process a batch of input messages and, if any are actionable, build a
/// completion future.
///
/// Returns `Some((future, cancel_sender))` when at least one input was
/// [`PrepareResult::Ready`](event_processor::PrepareResult::Ready). The
/// caller should store the future (CLI) or immediately `.await` it (Lambda).
/// The `cancel_sender` can be used to abort the completion early.
///
/// Returns `None` when no inputs were actionable (all handled/skipped).
#[expect(clippy::too_many_arguments, reason = "shared entry point")]
pub async fn process_batch<'a, Mdl, C, S, M, H>(
    inputs: impl Iterator<Item = (InputMessage, String)>,
    current_history: &'a RefCell<HistoryManager<C, S>>,
    conversation_store: &'a C,
    display_tx: &'a mpsc::UnboundedSender<DisplayEvent<Mdl::StreamingResponse>>,
    active_group_id: &'a str,
    model: &'a Mdl,
    tool_names: &'a HashSet<String>,
    tool_defs: &'a [ToolDefinition],
    tool_registry: &'a HashMap<String, &'a dyn Tool<M>>,
    mut tool_context: ToolContext<M>,
    extra_system_prompt: &'a Option<String>,
    additional_request_params: Option<serde_json::Value>,
    model_id_override: Option<String>,
    rap_notifier: Option<&'a RapNotifier<H>>,
) -> Option<(Pin<Box<dyn Future<Output = ()> + 'a>>, oneshot::Sender<()>)>
where
    Mdl: CompletionModel,
    C: ConversationStore,
    S: StateStore,
    M: InputSender + 'static,
    H: HttpClient,
{
    let mut any_ready = false;
    let mut last_message_id = String::new();

    for (input_msg, message_id) in inputs {
        if let Some(mid) = process_input_item(
            input_msg,
            message_id,
            &mut *current_history.borrow_mut(),
            conversation_store,
            display_tx,
        )
        .await
        {
            any_ready = true;
            last_message_id = mid;
        }
    }

    // Best-effort: notify RAP tool servers about interrupted tool calls
    {
        let interrupted = current_history.borrow_mut().take_interrupted_tool_calls();
        if !interrupted.is_empty() {
            if let Some(notifier) = rap_notifier {
                for call_id in &interrupted {
                    notifier
                        .notify_tool_cancelled(active_group_id, call_id)
                        .await;
                }
            }
        }
    }

    if !any_ready {
        return None;
    }

    let (cancel_tx, cancel_rx) = oneshot::channel();

    let thread_prefix = current_history.borrow().get_thread_nesting_prefix();
    let prefix = current_history.borrow().get_thread_nesting_prefix();
    let active_thread_id = current_history.borrow().thread_id.clone();
    let completion_message_id = last_message_id;
    tool_context.group_id = active_thread_id.clone(); // might have changed due to HistoryManager::fork_new

    let fut = Box::pin(async move {
        let mut hist = current_history.borrow_mut();

        // Scope the stream so its &mut borrow of `hist` is released
        // before we call sync / execute_action.
        let action = {
            let mut stream = std::pin::pin!(event_processor::run_completion(
                model,
                &mut *hist,
                tool_names,
                tool_defs,
                tool_registry,
                &tool_context,
                &active_thread_id,
                &completion_message_id,
                extra_system_prompt.as_deref(),
                additional_request_params.as_ref(),
                model_id_override.as_deref(),
                cancel_rx,
            ));

            let mut action = None;
            let mut started = false;
            let mut any_text = false;
            let mut resp = None;

            while let Some(ev) = stream.next().await {
                match ev {
                    Ok(event_processor::CompletionEvent::Info(info)) => {
                        let _ = display_tx.send(DisplayEvent::Info(info));
                    }
                    Ok(event_processor::CompletionEvent::TextChunk(chunk)) => {
                        any_text = true;
                        if !started {
                            let _ = display_tx.send(DisplayEvent::StartOutput {
                                prefix: thread_prefix.clone(),
                            });
                            started = true;
                        }
                        let _ = display_tx.send(DisplayEvent::TextChunk {
                            prefix: thread_prefix.clone(),
                            chunk,
                        });
                    }
                    Ok(event_processor::CompletionEvent::ThinkingStart) => {
                        let _ = display_tx.send(DisplayEvent::ThinkingStart);
                    }
                    Ok(event_processor::CompletionEvent::ThinkingEnd) => {
                        let _ = display_tx.send(DisplayEvent::ThinkingEnd);
                    }
                    Ok(event_processor::CompletionEvent::ThinkingChunk(chunk)) => {
                        let _ = display_tx.send(DisplayEvent::ThinkingChunk {
                            prefix: thread_prefix.clone(),
                            chunk,
                        });
                    }
                    Ok(event_processor::CompletionEvent::SyncToolResult(res)) => {
                        if let ToolResultContent::Text(text) = res.content.first() {
                            let _ = display_tx.send(DisplayEvent::ToolResult {
                                text: text.text,
                                display_as: None,
                                prefix: prefix.clone(),
                            });
                        }
                    }
                    Ok(event_processor::CompletionEvent::Action(CompletionAction::Done(r))) => {
                        // there may be multiple `Done` if the agent synchronously loops back
                        resp = Some(r);
                    }
                    Ok(event_processor::CompletionEvent::SyncToolCall {
                        ref tool_name,
                        ref tool_args,
                    }) => {
                        let _ = display_tx.send(DisplayEvent::ToolCall {
                            name: tool_name.clone(),
                            args: tool_args.clone(),
                            prefix: thread_prefix.clone(),
                        });
                    }
                    Ok(event_processor::CompletionEvent::Action(a)) => {
                        if let event_processor::CompletionAction::ExecuteToolCall {
                            ref tool_name,
                            ref tool_args,
                            ..
                        } = a
                        {
                            let _ = display_tx.send(DisplayEvent::ToolCall {
                                name: tool_name.clone(),
                                args: tool_args.clone(),
                                prefix: thread_prefix.clone(),
                            });
                        }

                        assert!(action.is_none());
                        action = Some(a);
                    }
                    Err(e) => {
                        let _ = display_tx.send(DisplayEvent::Info(format!("Error: {}", e)));
                        break;
                    }
                }
            }

            if any_text {
                if let Some(r) = resp {
                    let _ = display_tx.send(DisplayEvent::ResponseDone(thread_prefix.clone(), r));
                }
            }

            action
        };
        // Stream dropped — hist is usable again.

        hist.sync().await.ok();

        if let Some(action) = action {
            if let Err(e) =
                event_processor::execute_action(action, tool_registry, &tool_context).await
            {
                let _ = display_tx.send(DisplayEvent::Info(format!("Error: {}", e)));
            }
        }
    });

    Some((fut, cancel_tx))
}
