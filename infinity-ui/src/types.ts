/* ── Protocol types matching crates/infinity-protocol/src/lib.rs ── */

export type SessionStatus =
  | "Running"
  | "Idle"
  | "Stopped"
  | "WaitingForChoice"
  | "Migrating"
  | "Archived";

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
  remote?: string | null;
}

export interface ModelInfo {
  display_name: string;
  provider_id: string;
  model_id: string;
  context_window: number;
}

/** Globally unique reference to a model: provider id + provider-scoped id. */
export interface ModelRef {
  provider_id: string;
  model_id: string;
}

export interface RemoteInfo {
  name: string;
  status: string;
}

export interface TokenUsage {
  input_tokens: number | null;
  output_tokens: number | null;
  /** Total tokens including cached input. Prefer this over input+output. */
  total_tokens?: number | null;
}

/* ── Display segments for structured tool result rendering ── */

export type DisplaySegment =
  | { type: "text"; content: string }
  | { type: "diff"; content: { path: string; patch: string } }
  | { type: "image"; content: { data: string; mediaType: string } };

/* ── Daemon → Client messages ── */

export type DaemonMessage =
  | {
      Welcome: {
        sessions: Record<string, SessionInfo>;
        available_models: ModelInfo[];
        default_model_name: string;
        default_context_window: number;
        provider_name: string;
        remotes: RemoteInfo[];
      };
    }
  | {
      Connected: {
        root_thread_id: string;
        model_name: string;
        context_window: number;
        title: string | null;
        total_tokens_used: number;
        provider_id: string;
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
        segments: DisplaySegment[];
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
  | {
      ModelSwitched: {
        thread_id: string;
        model_name: string;
        context_window: number;
        provider_id: string;
      };
    }
  | { Error: { thread_id: string | null; text: string } }
  | { UserChoiceComplete: { choice_id: string } }
  | {
      Replay: {
        history: DaemonMessage[];
        pending_choices: DaemonMessage[];
        views: Record<string, any>;
      };
    }
  | {
      ViewUpdate: { thread_id: string | null; view_type: string; content: any };
    }
  | { SessionsUpdated: { sessions: Record<string, SessionInfo> } }
  | { RemotesUpdated: { remotes: RemoteInfo[] } }
  | "DisconnectNotIdle"
  | "DetachedIdle"
  | { EmigrateResult: { root_thread_id: string; session_data: string } }
  | { MigrateStarted: { root_thread_id: string } }
  | { MigrateComplete: { root_thread_id: string; new_root_thread_id: string } }
  | { MigrateError: { root_thread_id: string; error: string } }
  | {
      DirectoryListing: {
        request_path: string;
        entries: string[];
        on: string | null;
      };
    };

/* ── Client → Daemon messages ── */

export type ClientMessage =
  | {
      CreateSession: {
        cwd: string;
        location: string | null;
        model?: ModelRef | null;
      };
    }
  | { Connect: { root_thread_id: string; thread_id: string | null } }
  | { UserInput: { root_thread_id: string; text: string } }
  | "Disconnect"
  | { SoftDetach: { root_thread_id: string } }
  | { ShutdownSession: { root_thread_id: string } }
  | { LoadSession: { target_session_id: string } }
  | { SwitchModel: { thread_id: string; model: ModelRef } }
  | { UserChoiceAnswered: { choice_id: string; selected: number } }
  | { TriggerCompaction: { root_thread_id: string } }
  | {
      RequestMigrate: {
        root_thread_id: string;
        to: string | null;
        dest_cwd: string;
      };
    }
  | {
      Emigrate: {
        root_thread_id: string;
        dest_rap_urls: Record<string, string>;
      };
    }
  | { EmigrateDone: { root_thread_id: string } }
  | { ArchiveSession: { root_thread_id: string } }
  | { ListDirectory: { path: string; on: string | null } };

/* ── Connection status ── */

export type ConnectionStatus = "connecting" | "connected" | "disconnected";

/* ── Spinner states (matching terminal) ── */

export type SpinnerState = "loading" | "thinking" | "tool";

/* ── Display items for the message list ── */

export type MessageItem =
  | { type: "user"; text: string }
  | { type: "assistant"; text: string; done: boolean }
  | { type: "thinking"; text: string; done: boolean }
  | { type: "tool_call"; name: string; displayText: string }
  | { type: "tool_result"; segments: DisplaySegment[] }
  | { type: "info"; text: string }
  | { type: "subscription"; name: string; text: string }
  | { type: "compaction" }
  | { type: "error"; text: string };
