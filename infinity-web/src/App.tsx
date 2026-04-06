import { useCallback, useEffect, useRef, useState } from "react";
import { useSocket } from "./useSocket";
import { msgTag, msgPayload } from "./protocol";
import { MessageList } from "./components/MessageList";
import { SessionSidebar } from "./components/SessionSidebar";
import { CwdPicker } from "./components/CwdPicker";
import { MigratePicker } from "./components/MigratePicker";
import { DiffView } from "./components/DiffView";
import type {
  ClientMessage,
  DaemonMessage,
  DisplaySegment,
  SessionInfo,
  ModelInfo,
  RemoteInfo,
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
  const [migratePickerOpen, setMigratePickerOpen] = useState(false);
  const [remotes, setRemotes] = useState<RemoteInfo[]>([]);
  const [pendingChoices, setPendingChoices] = useState<
    { id: string; prompt: string; choices: string[]; default: number }[]
  >([]);
  const [views, setViews] = useState<Record<string, any>>({});
  const [activeTab, setActiveTab] = useState<string>("chat");
  // Thread navigation: viewThreadId is the currently viewed thread (null = root).
  // threadStack tracks the path from root so we can pop back when threads close.
  // e.g. [childA, grandchildB] means we navigated root → childA → grandchildB.
  const [viewThreadId, setViewThreadId] = useState<string | null>(null);
  const threadStackRef = useRef<string[]>([]);

  const sessionRef = useRef<string | null>(null);
  const threadRef = useRef<string | null>(null);
  const pendingInputRef = useRef<string[]>([]);
  const sendRef = useRef<(msg: ClientMessage) => void>(() => {});
  // Track whether we're currently accumulating assistant text
  const streamingRef = useRef(false);

  /** Check if a message's thread_id matches the thread we're currently viewing. */
  const isForCurrentView = useCallback(
    (msgThreadId: string | null) => {
      const viewing = viewThreadId ?? threadRef.current;
      // null thread_id = root, session_id = root
      return msgThreadId === null || msgThreadId === viewing;
    },
    [viewThreadId],
  );

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
              remotes: RemoteInfo[];
            }>(m);
            setSessions(p.sessions);
            setModels(p.available_models);
            setModelName(p.default_model_name);
            setContextWindow(p.default_context_window);
            setRemotes(p.remotes ?? []);
            appendMessage({
              type: "info",
              text: `Using provider ${p.provider_name} (${p.default_model_name})`,
            });
            break;
          }
          case "Connected": {
            const p = msgPayload<{
              session_id: string;
              thread_id: string;
              model_name: string;
              context_window: number;
              title: string | null;
              total_tokens_used: number;
            }>(m);
            sessionRef.current = p.session_id;
            threadRef.current = p.thread_id;
            setModelName(p.model_name);
            setContextWindow(p.context_window);
            setTotalTokens(p.total_tokens_used);
            // Flush pending inputs
            for (const text of pendingInputRef.current) {
              sendRef.current({
                UserInput: { session_id: p.thread_id, text },
              });
            }
            pendingInputRef.current = [];
            break;
          }
          case "StartOutput": {
            const p = msgPayload<{ thread_id: string | null }>(m);
            if (isForCurrentView(p.thread_id)) {
              streamingRef.current = false;
              setSpinner((prev) => (prev === "tool" ? "thinking" : "loading"));
            }
            break;
          }
          case "TextChunk": {
            const p = msgPayload<{ thread_id: string | null; chunk: string }>(
              m,
            );
            if (isForCurrentView(p.thread_id)) {
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
            const p = msgPayload<{ thread_id: string | null }>(m);
            if (isForCurrentView(p.thread_id)) {
              setSpinner("thinking");
            }
            break;
          }
          case "ThinkingChunk": {
            const p = msgPayload<{ thread_id: string | null; chunk: string }>(
              m,
            );
            if (isForCurrentView(p.thread_id)) {
              setSpinner("thinking");
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
            const p = msgPayload<{ thread_id: string | null }>(m);
            if (isForCurrentView(p.thread_id)) {
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
              thread_id: string | null;
              display_as: string | null;
            }>(m);
            if (isForCurrentView(p.thread_id)) {
              finishAssistant();
              streamingRef.current = false;
              setSpinner("tool");
              const displayText = p.display_as ?? `${p.name}(${p.args})`;
              appendMessage({ type: "tool_call", name: p.name, displayText });
            }
            break;
          }
          case "ToolResult": {
            const p = msgPayload<{
              segments: DisplaySegment[];
              thread_id: string | null;
            }>(m);
            if (isForCurrentView(p.thread_id)) {
              appendMessage({
                type: "tool_result",
                segments: p.segments,
              });
            }
            break;
          }
          case "ResponseDone": {
            const p = msgPayload<{
              thread_id: string | null;
              token_usage: TokenUsage | null;
            }>(m);
            if (isForCurrentView(p.thread_id)) {
              if (p.token_usage) {
                const total =
                  (p.token_usage.input_tokens ?? 0) +
                  (p.token_usage.output_tokens ?? 0);
                setTotalTokens(total);
              }
              finishAssistant();
              streamingRef.current = false;
              setSpinner((prev) => (prev === "tool" ? prev : null));
            }
            break;
          }
          case "UserInputEcho": {
            const p = msgPayload<{ thread_id: string | null; text: string }>(m);
            if (isForCurrentView(p.thread_id)) {
              finishAssistant();
              appendMessage({ type: "user", text: p.text });
              setSpinner("loading");
              streamingRef.current = false;
            }
            break;
          }
          case "Info": {
            const p = msgPayload<{ thread_id: string | null; text: string }>(m);
            if (isForCurrentView(p.thread_id)) {
              appendMessage({ type: "info", text: p.text });
            }
            break;
          }
          case "Error": {
            const p = msgPayload<{ thread_id: string | null; text: string }>(m);
            if (isForCurrentView(p.thread_id)) {
              appendMessage({ type: "error", text: p.text });
            }
            break;
          }
          case "CompactionApplied": {
            const p = msgPayload<{ thread_id: string | null }>(m);
            if (isForCurrentView(p.thread_id)) {
              appendMessage({ type: "compaction" });
            }
            break;
          }
          case "SubscriptionEvent": {
            const p = msgPayload<{
              name: string;
              text: string;
              thread_id: string | null;
            }>(m);
            if (isForCurrentView(p.thread_id)) {
              appendMessage({
                type: "subscription",
                name: p.name,
                text: p.text,
              });
              setSpinner("loading");
            }
            break;
          }
          case "SessionsUpdated": {
            const p = msgPayload<{ sessions: Record<string, SessionInfo> }>(m);
            setSessions((prev) => {
              const next = { ...prev, ...p.sessions };
              const stack = threadStackRef.current;
              const sid = sessionRef.current;
              if (stack.length > 0 && sid && next[sid]) {
                const threads = next[sid].threads;
                const currentView = stack[stack.length - 1];
                if (!threads.some((t) => t.thread_id === currentView)) {
                  // Current thread gone — pop until we find one that exists.
                  while (
                    stack.length > 0 &&
                    !threads.some(
                      (t) => t.thread_id === stack[stack.length - 1],
                    )
                  ) {
                    stack.pop();
                  }
                  const newView =
                    stack.length > 0 ? stack[stack.length - 1] : null;
                  threadStackRef.current = stack;
                  setViewThreadId(newView);
                  setMessages([]);
                  setSpinner(null);
                  setPendingChoices([]);
                  setViews({});
                  setActiveTab("chat");
                  streamingRef.current = false;
                  sendRef.current({
                    Connect: { session_id: sid, thread_id: newView },
                  });
                }
              }
              return next;
            });
            break;
          }
          case "Replay": {
            const p = msgPayload<{
              history: DaemonMessage[];
              pending_choices: DaemonMessage[];
              views: Record<string, any>;
            }>(m);
            for (const h of p.history) processOne(h);
            // After replay, mark any open assistant as done
            finishAssistant();
            setSpinner(null);
            for (const c of p.pending_choices) processOne(c);
            if (p.views && Object.keys(p.views).length > 0) {
              setViews(p.views);
            }
            break;
          }
          case "ViewUpdate": {
            const p = msgPayload<{
              thread_id: string | null;
              view_type: string;
              content: any;
            }>(m);
            if (isForCurrentView(p.thread_id)) {
              setViews((prev) => ({ ...prev, [p.view_type]: p.content }));
            }
            break;
          }
          case "UserChoiceRequired": {
            const p = msgPayload<{
              thread_id: string | null;
              id: string;
              prompt: string;
              choices: string[];
              default: number;
            }>(m);
            if (isForCurrentView(p.thread_id)) {
              setPendingChoices((prev) => [...prev, p]);
            }
            break;
          }
          // Ignored for now
          case "OAuthRequired":
          case "DisconnectNotIdle":
          case "DetachedIdle":
            break;
          case "RemotesUpdated": {
            const p = msgPayload<{ remotes: RemoteInfo[] }>(m);
            setRemotes(p.remotes);
            break;
          }
          case "MigrateStarted": {
            appendMessage({ type: "info", text: "Migrating session\u2026" });
            break;
          }
          case "MigrateComplete": {
            const p = msgPayload<{ session_id: string; new_session_id: string }>(m);
            if (sessionRef.current === p.session_id) {
              sendRef.current("Disconnect");
              sessionRef.current = null;
              threadRef.current = null;
              threadStackRef.current = [];
              setViewThreadId(null);
              setMessages([]);
              setSpinner(null);
              setPendingChoices([]);
              setViews({});
              setActiveTab("chat");
              streamingRef.current = false;
              sendRef.current({ Connect: { session_id: p.new_session_id, thread_id: null } });
            }
            appendMessage({ type: "info", text: "Migration complete" });
            break;
          }
          case "MigrateError": {
            const p = msgPayload<{ session_id: string; error: string }>(m);
            appendMessage({
              type: "error",
              text: `Migration failed: ${p.error}`,
            });
            break;
          }
        }
      };
      processOne(msg);
    },
    [appendMessage, updateLastAssistant, finishAssistant, isForCurrentView],
  );

  const { send, status } = useSocket({
    url: WS_URL,
    onMessage: processDaemonMessage,
  });
  sendRef.current = send;

  const handleSend = useCallback(
    (text: string) => {
      if (threadRef.current) {
        send({ UserInput: { session_id: threadRef.current, text } });
      } else {
        pendingInputRef.current.push(text);
        setCwdPickerOpen(true);
      }
    },
    [send],
  );

  const navigateTo = useCallback(
    (sessionId: string, threadId: string | null) => {
      const switchingSession =
        threadRef.current !== null && threadRef.current !== sessionId;

      // Disconnect from current view
      if (sessionRef.current) {
        send("Disconnect");
      }

      // Build thread stack from sessions data
      if (threadId) {
        const info = sessions[sessionId];
        const stack: string[] = [];
        let cur: string | null = threadId;
        while (cur && cur !== sessionId) {
          stack.unshift(cur);
          const t = info?.threads.find((th) => th.thread_id === cur);
          cur = t?.parent_thread_id ?? null;
        }
        threadStackRef.current = stack;
      } else {
        threadStackRef.current = [];
      }

      setViewThreadId(threadId);
      setMessages([]);
      setSpinner(null);
      setPendingChoices([]);
      setViews({});
      setActiveTab("chat");
      streamingRef.current = false;

      if (switchingSession) {
        sessionRef.current = null;
        threadRef.current = null;
      }
      send({ Connect: { session_id: sessionId, thread_id: threadId } });
    },
    [send, sessions],
  );

  const handleNewSession = useCallback(() => {
    if (sessionRef.current) {
      send("Disconnect");
    }
    sessionRef.current = null;
    threadRef.current = null;
    setViewThreadId(null);
    threadStackRef.current = [];
    setMessages([]);
    setSpinner(null);
    setPendingChoices([]);
    setViews({});
    setActiveTab("chat");
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

  const handleChoiceSelect = useCallback((index: number) => {
    setPendingChoices((prev) => {
      if (prev.length === 0) return prev;
      sendRef.current({
        UserChoiceAnswered: { choice_id: prev[0].id, selected: index },
      });
      return prev.slice(1);
    });
  }, []);

  const currentHost =
    (sessionRef.current && sessions[sessionRef.current]?.remote) || "local";

  const handleMigrateConfirm = useCallback(
    (destination: string, cwd: string) => {
      setMigratePickerOpen(false);
      if (sessionRef.current) {
        send({
          RequestMigrate: {
            session_id: sessionRef.current,
            to: destination,
            dest_cwd: cwd,
          },
        });
      }
    },
    [send],
  );

  const handleMigrateCancel = useCallback(() => {
    setMigratePickerOpen(false);
  }, []);

  const contextPct =
    contextWindow > 0 ? Math.min(100, (totalTokens / contextWindow) * 100) : 0;

  const viewKeys = Object.keys(views);
  const hasViews = viewKeys.length > 0;

  return (
    <div className={css.root}>
      <SessionSidebar
        sessions={sessions}
        activeSessionId={sessionRef.current}
        activeThreadId={viewThreadId}
        open={sidebarOpen}
        remotes={remotes}
        localStatus={status}
        onSelect={navigateTo}
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
        {sessionRef.current && (
          <button
            className={css.hostPill}
            onClick={() => setMigratePickerOpen(true)}
            aria-label="Migrate session"
          >
            {currentHost}
          </button>
        )}
        <span className={css.infoPill}>
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
      <div className={css.mainContent}>
        {hasViews && (
          <nav
            className={css.viewNav}
            style={{
              paddingLeft: sidebarOpen ? undefined : "48px",
            }}
          >
            <button
              className={activeTab === "chat" ? css.viewTabActive : css.viewTab}
              onClick={() => setActiveTab("chat")}
            >
              Chat
            </button>
            {viewKeys.map((key) => (
              <button
                key={key}
                className={activeTab === key ? css.viewTabActive : css.viewTab}
                onClick={() => setActiveTab(key)}
              >
                {key.charAt(0).toUpperCase() + key.slice(1)}
              </button>
            ))}
          </nav>
        )}
        <div className={css.mainBody}>
          <div style={{ display: activeTab === "chat" || !hasViews ? undefined : "none", height: "100%" }}>
            <MessageList
              messages={messages}
              spinner={spinner}
              onSend={handleSend}
              inputDisabled={status !== "connected"}
              pendingChoice={pendingChoices[0] ?? null}
              onChoiceSelect={handleChoiceSelect}
              theme={theme}
            />
          </div>
          {views.diff && (
            <div style={{ display: activeTab === "diff" ? undefined : "none", height: "100%" }}>
              <DiffView diff={views.diff.diff} theme={theme} />
            </div>
          )}
          {activeTab !== "chat" && activeTab !== "diff" && hasViews && (
            <div style={{ padding: 24, color: "var(--text-muted)" }}>
              Unsupported view: {activeTab}
            </div>
          )}
        </div>
      </div>
      {cwdPickerOpen && (
        <CwdPicker onConfirm={handleCwdConfirm} onCancel={handleCwdCancel} />
      )}
      {migratePickerOpen && (
        <MigratePicker
          remotes={remotes}
          currentHost={currentHost}
          onConfirm={handleMigrateConfirm}
          onCancel={handleMigrateCancel}
        />
      )}
    </div>
  );
}
