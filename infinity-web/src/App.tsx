import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useSocket } from "./useSocket";
import {
  msgTag,
  msgPayload,
  MessageList,
  SessionSidebar,
  MigratePicker,
  DiffView,
  Spinner,
} from "infinity-ui";
import type {
  ClientMessage,
  DaemonMessage,
  DisplaySegment,
  SessionInfo,
  ModelInfo,
  RemoteInfo,
  SpinnerState,
  MessageItemType as MessageItem,
  TokenUsage,
} from "infinity-ui";
import css from "./App.module.css";
import chatCss from "infinity-ui/src/components/ChatPanel.module.css";

const WS_URL = import.meta.env.VITE_WS_PORT
  ? `ws://${window.location.hostname}:${import.meta.env.VITE_WS_PORT}`
  : `ws://${window.location.host}`;

type Theme = "light" | "dark" | "system";

function getInitialTheme(): Theme {
  return (localStorage.getItem("infinity-theme") as Theme) ?? "system";
}

function resolveTheme(t: Theme): "light" | "dark" {
  if (t !== "system") return t;
  return window.matchMedia("(prefers-color-scheme: light)").matches
    ? "light"
    : "dark";
}

export function App() {
  const [msgState, setMsgState] = useState<{
    messages: MessageItem[];
    gen: number;
  }>({ messages: [], gen: 0 });
  const messages = msgState.messages;
  const setMessages = useCallback(
    (updater: MessageItem[] | ((prev: MessageItem[]) => MessageItem[])) => {
      setMsgState((prev) => ({
        ...prev,
        messages:
          typeof updater === "function" ? updater(prev.messages) : updater,
      }));
    },
    [],
  );
  const resetMessages = useCallback(() => {
    setMsgState((prev) => ({ messages: [], gen: prev.gen + 1 }));
  }, []);
  const [sessions, setSessions] = useState<Record<string, SessionInfo>>({});
  const [_models, setModels] = useState<ModelInfo[]>([]);
  const [modelName, setModelName] = useState("");
  const [contextWindow, setContextWindow] = useState(0);
  const [totalTokens, setTotalTokens] = useState(0);
  const [spinner, setSpinner] = useState<SpinnerState | null>(null);
  const [sidebarPinned, setSidebarPinned] = useState(true);
  const [sidebarHover, setSidebarHover] = useState(false);
  const [sidebarWidth, setSidebarWidth] = useState(272);
  const [theme, setTheme] = useState<Theme>(getInitialTheme);
  const [resolved, setResolved] = useState<"light" | "dark">(() =>
    resolveTheme(getInitialTheme()),
  );
  const [newSessionPickerOpen, setNewSessionPickerOpen] = useState(false);
  const [migratePickerOpen, setMigratePickerOpen] = useState(false);
  const [remotes, setRemotes] = useState<RemoteInfo[]>([]);
  const [pendingChoices, setPendingChoices] = useState<
    { id: string; prompt: string; choices: string[]; default: number }[]
  >([]);
  const [views, setViews] = useState<Record<string, any>>({});
  const [activeTab, setActiveTab] = useState<string | null>(null);
  const [chatPinned, setChatPinned] = useState(false);
  const [chatHover, setChatHover] = useState(false);
  const [chatPanelWidth, setChatPanelWidth] = useState(420);
  const [dirEntries, setDirEntries] = useState<string[]>([]);
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
  const chatSpinnerPortalRef = useRef<HTMLDivElement>(null);
  const sidebarDragging = useRef(false);
  // Track whether we've received Connected for the current session
  const [sessionConnected, setSessionConnected] = useState(false);
  const connectRetryRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const clearConnectRetry = useCallback(() => {
    if (connectRetryRef.current) {
      clearInterval(connectRetryRef.current);
      connectRetryRef.current = null;
    }
  }, []);

  /** Send a Connect message and start a retry timer. */
  const sendConnect = useCallback(
    (sessionId: string, threadId: string | null) => {
      clearConnectRetry();
      setSessionConnected(false);
      sendRef.current({
        Connect: { session_id: sessionId, thread_id: threadId },
      });
      connectRetryRef.current = setInterval(() => {
        sendRef.current({
          Connect: { session_id: sessionId, thread_id: threadId },
        });
      }, 5000);
    },
    [clearConnectRetry],
  );

  useEffect(() => clearConnectRetry, [clearConnectRetry]);

  /** Check if a message's thread_id matches the thread we're currently viewing. */
  const isForCurrentView = useCallback(
    (msgThreadId: string | null) => {
      const viewing = viewThreadId ?? threadRef.current;
      // null thread_id = root, session_id = root
      return msgThreadId === null || msgThreadId === viewing;
    },
    [viewThreadId],
  );

  // Sync resolved theme from preference + system
  useEffect(() => {
    setResolved(resolveTheme(theme));
    localStorage.setItem("infinity-theme", theme);
    if (theme === "system") {
      const mq = window.matchMedia("(prefers-color-scheme: light)");
      const onChange = () => setResolved(resolveTheme("system"));
      mq.addEventListener("change", onChange);
      return () => mq.removeEventListener("change", onChange);
    }
  }, [theme]);

  // Apply resolved theme to document
  useEffect(() => {
    document.documentElement.setAttribute("data-theme", resolved);
  }, [resolved]);

  const toggleTheme = useCallback(() => {
    setTheme((t) =>
      t === "dark" ? "light" : t === "light" ? "system" : "dark",
    );
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
            // Re-connect to the session we were viewing before the WS dropped
            if (sessionRef.current) {
              sendConnect(sessionRef.current, viewThreadId);
            }
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
            clearConnectRetry();
            setSessionConnected(true);
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
                  resetMessages();
                  setSpinner(null);
                  setPendingChoices([]);
                  setViews({});
                  setActiveTab(null);
                  streamingRef.current = false;
                  sendConnect(sid, newView);
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
          case "UserChoiceComplete": {
            const p = msgPayload<{ choice_id: string }>(m);
            setPendingChoices((prev) =>
              prev.filter((c) => c.id !== p.choice_id),
            );
            break;
          }
          case "DirectoryListing": {
            const p = msgPayload<{ request_path: string; entries: string[] }>(
              m,
            );
            setDirEntries(p.entries);
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
            const p = msgPayload<{
              session_id: string;
              new_session_id: string;
            }>(m);
            if (sessionRef.current === p.session_id) {
              sendRef.current("Disconnect");
              sessionRef.current = null;
              threadRef.current = null;
              threadStackRef.current = [];
              setViewThreadId(null);
              resetMessages();
              setSpinner(null);
              setPendingChoices([]);
              setViews({});
              setActiveTab(null);
              streamingRef.current = false;
              sendConnect(p.new_session_id, null);
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
    [
      appendMessage,
      updateLastAssistant,
      finishAssistant,
      isForCurrentView,
      sendConnect,
      clearConnectRetry,
    ],
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
        setNewSessionPickerOpen(true);
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
      resetMessages();
      setSpinner(null);
      setPendingChoices([]);
      setViews({});
      setActiveTab(null);
      streamingRef.current = false;

      if (switchingSession) {
        sessionRef.current = null;
        threadRef.current = null;
      }
      sendConnect(sessionId, threadId);
    },
    [send, sessions, sendConnect],
  );

  const handleArchiveSession = useCallback(() => {
    if (!sessionRef.current) return;
    send({ ArchiveSession: { session_id: sessionRef.current } });
    sessionRef.current = null;
    threadRef.current = null;
    setViewThreadId(null);
    threadStackRef.current = [];
    resetMessages();
    setSpinner(null);
    setPendingChoices([]);
    setViews({});
    setActiveTab(null);
    streamingRef.current = false;
    clearConnectRetry();
    setSessionConnected(false);
  }, [send, clearConnectRetry]);

  const handleNewSession = useCallback(() => {
    if (sessionRef.current) {
      send("Disconnect");
    }
    sessionRef.current = null;
    threadRef.current = null;
    setViewThreadId(null);
    threadStackRef.current = [];
    resetMessages();
    setSpinner(null);
    setPendingChoices([]);
    setViews({});
    setActiveTab(null);
    streamingRef.current = false;
    clearConnectRetry();
    setSessionConnected(false);
    setNewSessionPickerOpen(true);
  }, [send, clearConnectRetry]);

  const handleNewSessionConfirm = useCallback(
    (destination: string | null, cwd: string) => {
      setNewSessionPickerOpen(false);
      send({ CreateSession: { cwd, location: destination } });
    },
    [send],
  );

  const handleNewSessionCancel = useCallback(() => {
    setNewSessionPickerOpen(false);
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
    (sessionRef.current && sessions[sessionRef.current]?.remote) ?? null;

  const handleMigrateConfirm = useCallback(
    (destination: string | null, cwd: string) => {
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

  const handleToggleSidebarPin = useCallback(() => {
    setSidebarPinned((p) => {
      if (p) setSidebarHover(true);
      return !p;
    });
  }, []);

  const handleSidebarDragState = useCallback((d: boolean) => {
    sidebarDragging.current = d;
  }, []);

  const handleToggleChatPin = useCallback(() => {
    setChatPinned((p) => {
      if (p) setChatHover(true);
      return !p;
    });
  }, []);

  const chatDragging = useRef(false);
  const onChatDragStart = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    chatDragging.current = true;
    const onMove = (ev: MouseEvent) => {
      if (!chatDragging.current) return;
      const newW = Math.min(
        700,
        Math.max(300, window.innerWidth - ev.clientX - 12),
      );
      setChatPanelWidth(newW);
    };
    const onUp = () => {
      chatDragging.current = false;
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  }, []);

  const contextPct =
    contextWindow > 0 ? Math.min(100, (totalTokens / contextWindow) * 100) : 0;

  const viewKeys = Object.keys(views);
  const hasViews = viewKeys.length > 0;
  const chatVisible = chatPinned || chatHover || pendingChoices.length > 0;
  const chatPanelOffset = hasViews && chatPinned ? chatPanelWidth + 16 : 24;

  // Auto-select first view tab when views first appear
  useEffect(() => {
    if (hasViews && activeTab === null) {
      setActiveTab(viewKeys[0]);
    }
  }, [hasViews]);

  // Edge hover zones for unpinned panels, with velocity-based flick detection
  const EDGE_SIZE = 32;
  const DEHOVER_BUFFER = 40;
  const VELOCITY_THRESHOLD = 6400; // px/s
  const SMOOTHING = 0.3; // exponential moving average factor
  const lastMouseRef = useRef({ x: 0, t: 0, vx: 0 });
  useEffect(() => {
    const onMove = (e: MouseEvent) => {
      const now = performance.now();
      const dt = (now - lastMouseRef.current.t) / 1000;
      const dx = e.clientX - lastMouseRef.current.x;
      const instantVx = dt > 0 ? dx / dt : 0;
      const vx =
        lastMouseRef.current.t === 0
          ? instantVx
          : SMOOTHING * instantVx + (1 - SMOOTHING) * lastMouseRef.current.vx;
      lastMouseRef.current = { x: e.clientX, t: now, vx };

      if (!sidebarPinned) {
        if (
          e.clientX <= EDGE_SIZE ||
          (vx < -VELOCITY_THRESHOLD && e.clientX <= sidebarWidth + 12)
        ) {
          setSidebarHover(true);
        } else if (
          !sidebarDragging.current &&
          e.clientX > sidebarWidth + 12 + DEHOVER_BUFFER
        ) {
          setSidebarHover(false);
        }
      }
      if (hasViews && !chatPinned) {
        const fromRight = window.innerWidth - e.clientX;
        if (
          fromRight <= EDGE_SIZE ||
          (vx > VELOCITY_THRESHOLD && fromRight <= chatPanelWidth + 12)
        ) {
          setChatHover(true);
        } else if (
          !chatDragging.current &&
          fromRight > chatPanelWidth + 12 + DEHOVER_BUFFER
        ) {
          setChatHover(false);
        }
      }
    };
    window.addEventListener("mousemove", onMove);
    return () => window.removeEventListener("mousemove", onMove);
  }, [sidebarPinned, chatPinned, hasViews, sidebarWidth, chatPanelWidth]);

  return (
    <div
      className={css.root}
      style={
        {
          "--chat-panel-offset": `${chatPanelOffset}px`,
          "--chat-panel-width": `${chatPanelWidth}px`,
        } as React.CSSProperties
      }
    >
      <SessionSidebar
        sessions={sessions}
        activeSessionId={sessionRef.current}
        activeThreadId={viewThreadId}
        pinned={sidebarPinned}
        visible={sidebarHover}
        remotes={remotes}
        localStatus={status}
        onSelect={navigateTo}
        onNew={handleNewSession}
        onTogglePin={handleToggleSidebarPin}
        onWidthChange={setSidebarWidth}
        onDragStateChange={handleSidebarDragState}
      />
      <div className={css.topRight}>
        {sessionRef.current && (
          <button
            className={css.hostPill}
            onClick={() => setMigratePickerOpen(true)}
            aria-label="Migrate session"
          >
            {currentHost ?? "local"}
          </button>
        )}
        <span className={css.infoPill}>
          {modelName}
          {modelName && " \u00b7 "}
          {Math.round(contextPct)}% context
        </span>
        {sessionRef.current && (
          <button
            className={css.archivePill}
            onClick={handleArchiveSession}
            aria-label="Archive session"
          >
            <svg
              width="16"
              height="16"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <rect x="2" y="3" width="20" height="5" rx="1" />
              <path d="M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8" />
              <path d="M10 12h4" />
            </svg>
          </button>
        )}
        <button
          className={css.themePill}
          onClick={toggleTheme}
          aria-label="Toggle theme"
        >
          {theme === "dark"
            ? "\u263E"
            : theme === "light"
              ? "\u2600"
              : "\uD83D\uDCBB"}
        </button>
        <div ref={chatSpinnerPortalRef} className={css.chatSpinnerPortal} />
      </div>
      <div className={css.mainContent}>
        {hasViews && (
          <nav className={css.viewNav}>
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
          {!hasViews && (
            <div style={{ height: "100%" }}>
              <MessageList
                messages={messages}
                generation={msgState.gen}
                spinner={spinner}
                onSend={handleSend}
                inputDisabled={status !== "connected" || !sessionConnected}
                pendingChoice={pendingChoices[0] ?? null}
                onChoiceSelect={handleChoiceSelect}
                theme={resolved}
              />
            </div>
          )}
          {views.diff && (
            <div
              style={{
                display: activeTab === "diff" ? undefined : "none",
                height: "100%",
              }}
            >
              <DiffView files={views.diff.files ?? []} theme={resolved} />
            </div>
          )}
          {hasViews && activeTab !== "diff" && (
            <div style={{ padding: 24, color: "var(--text-muted)" }}>
              Unsupported view: {activeTab}
            </div>
          )}
        </div>
      </div>
      {hasViews && (
        <div
          className={`${chatCss.chatPanel} ${!chatVisible ? chatCss.chatPanelHidden : ""}`}
        >
          <div className={chatCss.chatPanelHeader}>
            <span className={chatCss.chatPanelTitle}>Chat</span>
            <button
              className={chatCss.chatPanelClose}
              onClick={handleToggleChatPin}
              aria-label={chatPinned ? "Unpin chat" : "Pin chat"}
              data-pinned={chatPinned}
            >
              {"\uD83D\uDCCC"}
            </button>
          </div>
          <div className={chatCss.chatPanelBody}>
            <MessageList
              messages={messages}
              generation={msgState.gen}
              spinner={spinner}
              onSend={handleSend}
              inputDisabled={status !== "connected" || !sessionConnected}
              pendingChoice={pendingChoices[0] ?? null}
              onChoiceSelect={handleChoiceSelect}
              theme={resolved}
            />
          </div>
          <div
            className={chatCss.chatPanelResize}
            onMouseDown={onChatDragStart}
            onDoubleClick={() => setChatPanelWidth(420)}
          />
        </div>
      )}
      {hasViews &&
        !chatPinned &&
        chatSpinnerPortalRef.current &&
        createPortal(
          <div
            onClick={() => setChatPinned(true)}
            style={{ cursor: "pointer" }}
          >
            {spinner ? (
              <Spinner state={spinner} />
            ) : (
              <div className={css.chatIdleDot} />
            )}
          </div>,
          chatSpinnerPortalRef.current,
        )}
      {newSessionPickerOpen && (
        <MigratePicker
          remotes={remotes}
          title="New session on"
          onConfirm={handleNewSessionConfirm}
          onCancel={handleNewSessionCancel}
          send={send}
          directoryEntries={dirEntries}
          onClearEntries={() => {
            setDirEntries([]);
          }}
        />
      )}
      {migratePickerOpen && (
        <MigratePicker
          remotes={remotes}
          currentHost={currentHost}
          onConfirm={handleMigrateConfirm}
          onCancel={handleMigrateCancel}
          send={send}
          directoryEntries={dirEntries}
          onClearEntries={() => {
            setDirEntries([]);
          }}
        />
      )}
    </div>
  );
}
