/* ── Protocol types matching crates/infinity-protocol/src/lib.rs ── */

export type SessionStatus = "Running" | "Idle" | "Stopped" | "WaitingForChoice";

export interface SubthreadInfo {
  thread_id: string;
  parent_thread_id: string;
  title: string | null;
}

export interface SessionInfo {
  title: string | null;
  last_updated: string;
  total_tokens_used: number;
  status: SessionStatus;
  threads: SubthreadInfo[];
}

export interface ModelInfo {
  display_name: string;
  model_id: string;
  context_window: number;
}

export interface TokenUsage {
  input_tokens: number | null;
  output_tokens: number | null;
}

/* ── Daemon → Client messages ── */

export type DaemonMessage =
  | {
      Welcome: {
        sessions: Record<string, SessionInfo>;
        available_models: ModelInfo[];
        default_model_name: string;
        default_context_window: number;
        provider_name: string;
      };
    }
  | {
      Connected: {
        session_id: string;
        model_name: string;
        context_window: number;
        title: string | null;
        total_tokens_used: number;
      };
    }
  | { StartOutput: { thread_id: string | null } }
  | { TextChunk: { thread_id: string | null; chunk: string } }
  | {
      ToolCall: {
        name: string;
        args: string;
        thread_id: string | null;
        display_as: string | null;
      };
    }
  | {
      ToolResult: {
        text: string;
        display_as: string | null;
        thread_id: string | null;
      };
    }
  | { Info: { thread_id: string | null; text: string } }
  | {
      ResponseDone: {
        thread_id: string | null;
        token_usage: TokenUsage | null;
      };
    }
  | { UserInputEcho: { thread_id: string | null; text: string } }
  | {
      SubscriptionEvent: {
        name: string;
        text: string;
        thread_id: string | null;
      };
    }
  | { OAuthRequired: { thread_id: string | null; auth_url: string } }
  | {
      UserChoiceRequired: {
        thread_id: string | null;
        id: string;
        prompt: string;
        choices: string[];
        default: number;
      };
    }
  | { ThinkingStart: { thread_id: string | null } }
  | { ThinkingEnd: { thread_id: string | null } }
  | { ThinkingChunk: { thread_id: string | null; chunk: string } }
  | { CompactionApplied: { thread_id: string | null } }
  | { Error: { thread_id: string | null; text: string } }
  | { Replay: { history: DaemonMessage[]; pending_choices: DaemonMessage[] } }
  | { SessionsUpdated: { sessions: Record<string, SessionInfo> } }
  | "DisconnectNotIdle"
  | "DetachedIdle";

/* ── Client → Daemon messages ── */

export type ClientMessage =
  | { CreateSession: { cwd: string } }
  | { Connect: { session_id: string; thread_id: string | null } }
  | { UserInput: { session_id: string; text: string } }
  | "Disconnect"
  | { SoftDetach: { session_id: string } }
  | { ShutdownSession: { session_id: string } }
  | { LoadSession: { target_session_id: string } }
  | { SwitchModel: { session_id: string; model_id: string } }
  | { UserChoiceAnswered: { choice_id: string; selected: number } }
  | { TriggerCompaction: { session_id: string } };

/* ── Spinner states (matching terminal) ── */

export type SpinnerState = "loading" | "thinking" | "tool";

/* ── Display items for the message list ── */

export type MessageItem =
  | { type: "user"; text: string }
  | { type: "assistant"; text: string; done: boolean }
  | { type: "thinking"; text: string; done: boolean }
  | { type: "tool_call"; name: string; displayText: string }
  | { type: "tool_result"; text: string; multiline: boolean }
  | { type: "info"; text: string }
  | { type: "subscription"; name: string; text: string }
  | { type: "compaction" }
  | { type: "error"; text: string };
