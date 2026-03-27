import { useCallback, useEffect, useRef, useState } from "react";
import { useSocket } from "./useSocket";
import { msgTag, msgPayload } from "./protocol";
import { MessageList } from "./components/MessageList";
import { SessionSidebar } from "./components/SessionSidebar";
import { CwdPicker } from "./components/CwdPicker";
import type {
  ClientMessage,
  DaemonMessage,
  SessionInfo,
  ModelInfo,
  SpinnerState,
  MessageItem,
  TokenUsage,
} from "./types";
import css from "./App.module.css";

const WS_URL = `ws://${window.location.hostname}:${import.meta.env.VITE_WS_PORT || "8080"}`;

type Theme = "light" | "dark";

function getInitialTheme(): Theme {
  const stored = localStorage.getItem("infinity-theme") as Theme | null;
  if (stored) return stored;
  return window.matchMedia("(prefers-color-scheme: light)").matches
    ? "light"
    : "dark";
}

export function App() {
  const [messages, setMessages] = useState<MessageItem[]>([]);
  const [sessions, setSessions] = useState<Record<string, SessionInfo>>({});
  const [_models, setModels] = useState<ModelInfo[]>([]);
  const [modelName, setModelName] = useState("");
  const [contextWindow, setContextWindow] = useState(0);
  const [totalTokens, setTotalTokens] = useState(0);
  const [spinner, setSpinner] = useState<SpinnerState | null>(null);
  const [sidebarOpen, setSidebarOpen] = useState(true);
  const [theme, setTheme] = useState<Theme>(getInitialTheme);
  const [cwdPickerOpen, setCwdPickerOpen] = useState(false);

  const sessionRef = useRef<string | null>(null);
  const pendingInputRef = useRef<string[]>([]);
  const sendRef = useRef<(msg: ClientMessage) => void>(() => {});
  // Track whether we're currently accumulating assistant text
  const streamingRef = useRef(false);

  // Apply theme to document
  useEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
    localStorage.setItem("infinity-theme", theme);
  }, [theme]);

  const toggleTheme = useCallback(() => {
    setTheme((t) => (t === "dark" ? "light" : "dark"));
  }, []);

  const appendMessage = useCallback((item: MessageItem) => {
    setMessages((prev) => [...prev, item]);
  }, []);

  const updateLastAssistant = useCallback((chunk: string) => {
    setMessages((prev) => {
      const last = prev[prev.length - 1];
      if (last && last.type === "assistant" && !last.done) {
        const updated = [...prev];
        updated[updated.length - 1] = { ...last, text: last.text + chunk };
        return updated;
      }
      return [...prev, { type: "assistant", text: chunk, done: false }];
    });
  }, []);

  const finishAssistant = useCallback(() => {
    setMessages((prev) => {
      const last = prev[prev.length - 1];
      if (last && last.type === "assistant" && !last.done) {
        const updated = [...prev];
        updated[updated.length - 1] = { ...last, done: true };
        return updated;
      }
      return prev;
    });
  }, []);

  const processDaemonMessage = useCallback(
    (msg: DaemonMessage) => {
      // Handle replayed messages recursively
      const processOne = (m: DaemonMessage) => {
        const tag = msgTag(m);
        switch (tag) {
          case "Welcome": {
            const p = msgPayload<{
              sessions: Record<string, SessionInfo>;
              available_models: ModelInfo[];
              default_model_name: string;
              default_context_window: number;
              provider_name: string;
            }>(m);
            setSessions(p.sessions);
            setModels(p.available_models);
            setModelName(p.default_model_name);
            setContextWindow(p.default_context_window);
            appendMessage({
              type: "info",
              text: `Using provider ${p.provider_name} (${p.default_model_name})`,
            });
            break;
          }
          case "Connected": {
            const p = msgPayload<{
              session_id: string;
              model_name: string;
              context_window: number;
              title: string | null;
              total_tokens_used: number;
            }>(m);
            sessionRef.current = p.session_id;
            setModelName(p.model_name);
            setContextWindow(p.context_window);
            setTotalTokens(p.total_tokens_used);
            // Flush pending inputs
            for (const text of pendingInputRef.current) {
              sendRef.current({
                UserInput: { session_id: p.session_id, text },
              });
            }
            pendingInputRef.current = [];
            break;
          }
          case "StartOutput": {
            const p = msgPayload<{ prefix: string | null }>(m);
            if (p.prefix === null) {
              streamingRef.current = false;
              setSpinner((prev) => (prev === "tool" ? "thinking" : "loading"));
            }
            break;
          }
          case "TextChunk": {
            const p = msgPayload<{ prefix: string | null; chunk: string }>(m);
            if (p.prefix === null) {
              setSpinner("thinking");
              const chunk = !streamingRef.current
                ? p.chunk.trimStart()
                : p.chunk;
              streamingRef.current = true;
              if (chunk) updateLastAssistant(chunk);
            }
            break;
          }
          case "ThinkingStart": {
            const p = msgPayload<{ prefix: string | null }>(m);
            if (p.prefix === null) {
              setSpinner("thinking");
            }
            break;
          }
          case "ThinkingChunk": {
            const p = msgPayload<{ prefix: string | null; chunk: string }>(m);
            if (p.prefix === null) {
              setSpinner("thinking");
              // Update thinking display
              setMessages((prev) => {
                const last = prev[prev.length - 1];
                if (last && last.type === "thinking" && !last.done) {
                  const updated = [...prev];
                  updated[updated.length - 1] = {
                    ...last,
                    text: last.text + p.chunk,
                  };
                  return updated;
                }
                return [
                  ...prev,
                  { type: "thinking", text: p.chunk, done: false },
                ];
              });
            }
            break;
          }
          case "ThinkingEnd": {
            const p = msgPayload<{ prefix: string | null }>(m);
            if (p.prefix === null) {
              setMessages((prev) => {
                const last = prev[prev.length - 1];
                if (last && last.type === "thinking" && !last.done) {
                  const updated = [...prev];
                  updated[updated.length - 1] = { ...last, done: true };
                  return updated;
                }
                return prev;
              });
            }
            break;
          }
          case "ToolCall": {
            const p = msgPayload<{
              name: string;
              args: string;
              prefix: string | null;
              display_script: string | null;
            }>(m);
            if (p.prefix === null) {
              finishAssistant();
              streamingRef.current = false;
              setSpinner("tool");
              const displayText = `${p.name}(${p.args})`;
              appendMessage({ type: "tool_call", name: p.name, displayText });
            }
            break;
          }
          case "ToolResult": {
            const p = msgPayload<{
              text: string;
              display_as: string | null;
              prefix: string | null;
            }>(m);
            if (p.prefix === null) {
              const display = p.display_as ?? p.text;
              const lines = display.split("\n");
              appendMessage({
                type: "tool_result",
                text: display,
                multiline: lines.length > 1,
              });
            }
            break;
          }
          case "ResponseDone": {
            const p = msgPayload<{
              thread_id: string | null;
              token_usage: TokenUsage | null;
            }>(m);
            if (p.thread_id === null) {
              if (p.token_usage) {
                const total =
                  (p.token_usage.input_tokens ?? 0) +
                  (p.token_usage.output_tokens ?? 0);
                setTotalTokens(total);
              }
              finishAssistant();
              streamingRef.current = false;
              setSpinner((prev) => prev === 'tool' ? prev : null);
            }
            break;
          }
          case "UserInputEcho": {
            const text = msgPayload<string>(m);
            finishAssistant();
            appendMessage({ type: "user", text });
            setSpinner("loading");
            streamingRef.current = false;
            break;
          }
          case "Info": {
            const text = msgPayload<string>(m);
            appendMessage({ type: "info", text });
            break;
          }
          case "Error": {
            const text = msgPayload<string>(m);
            appendMessage({ type: "error", text });
            break;
          }
          case "CompactionApplied": {
            const p = msgPayload<{ prefix: string | null }>(m);
            if (p.prefix === null) {
              appendMessage({ type: "compaction" });
            }
            break;
          }
          case "SubscriptionEvent": {
            const p = msgPayload<{
              name: string;
              text: string;
              prefix: string | null;
            }>(m);
            appendMessage({ type: "subscription", name: p.name, text: p.text });
            setSpinner("loading");
            break;
          }
          case "SessionsUpdated": {
            const p = msgPayload<{ sessions: Record<string, SessionInfo> }>(m);
            setSessions((prev) => ({ ...prev, ...p.sessions }));
            break;
          }
          case "Replay": {
            const p = msgPayload<{
              history: DaemonMessage[];
              pending_choices: DaemonMessage[];
            }>(m);
            for (const h of p.history) processOne(h);
            // After replay, mark any open assistant as done
            finishAssistant();
            setSpinner(null);
            for (const c of p.pending_choices) processOne(c);
            break;
          }
          // Ignored for now
          case "OAuthRequired":
          case "UserChoiceRequired":
          case "DisconnectNotIdle":
          case "DetachedIdle":
            break;
        }
      };
      processOne(msg);
    },
    [appendMessage, updateLastAssistant, finishAssistant],
  );

  const { send, status } = useSocket({
    url: WS_URL,
    onMessage: processDaemonMessage,
  });
  sendRef.current = send;

  const handleSend = useCallback(
    (text: string) => {
      if (sessionRef.current) {
        send({ UserInput: { session_id: sessionRef.current, text } });
      } else {
        pendingInputRef.current.push(text);
        setCwdPickerOpen(true);
      }
    },
    [send],
  );

  const handleSelectSession = useCallback(
    (id: string) => {
      if (sessionRef.current) {
        send({ Disconnect: { session_id: sessionRef.current } });
      }
      sessionRef.current = null;
      setMessages([]);
      setSpinner(null);
      streamingRef.current = false;
      send({ LoadSession: { target_session_id: id } });
    },
    [send],
  );

  const handleNewSession = useCallback(() => {
    if (sessionRef.current) {
      send({ Disconnect: { session_id: sessionRef.current } });
    }
    sessionRef.current = null;
    setMessages([]);
    setSpinner(null);
    streamingRef.current = false;
    setCwdPickerOpen(true);
  }, [send]);

  const handleCwdConfirm = useCallback(
    (cwd: string) => {
      setCwdPickerOpen(false);
      send({ CreateSession: { cwd } });
    },
    [send],
  );

  const handleCwdCancel = useCallback(() => {
    setCwdPickerOpen(false);
    pendingInputRef.current = [];
  }, []);

  const contextPct =
    contextWindow > 0 ? Math.min(100, (totalTokens / contextWindow) * 100) : 0;

  return (
    <div className={css.root}>
      <SessionSidebar
        sessions={sessions}
        activeSessionId={sessionRef.current}
        open={sidebarOpen}
        onSelect={handleSelectSession}
        onNew={handleNewSession}
        onClose={() => setSidebarOpen(false)}
      />
      <button
        className={css.menuBtn}
        onClick={() => setSidebarOpen((o) => !o)}
        aria-label="Toggle sessions"
      >
        {"\u2630"}
      </button>
      <div className={css.topRight}>
        <span className={css.infoPill}>
          {status !== "connected" && (
            <span className={css.dot} data-status={status} />
          )}
          {modelName}
          {modelName && " \u00b7 "}
          {Math.round(contextPct)}% context
        </span>
        <button
          className={css.themePill}
          onClick={toggleTheme}
          aria-label="Toggle theme"
        >
          {theme === "dark" ? "\u2600" : "\u263E"}
        </button>
      </div>
      <MessageList messages={messages} spinner={spinner} onSend={handleSend} inputDisabled={status !== "connected"} />
      {cwdPickerOpen && (
        <CwdPicker onConfirm={handleCwdConfirm} onCancel={handleCwdCancel} />
      )}
    </div>
  );
}
