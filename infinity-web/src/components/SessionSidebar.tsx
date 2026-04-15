import { useCallback, useRef, useState, useEffect } from "react";
import type { SessionInfo, SubthreadInfo, RemoteInfo } from "../types";
import type { ConnectionStatus } from "../useSocket";
import css from "./SessionSidebar.module.css";

function CopyThreadId({ id }: { id: string }) {
  const bare = id.includes("/") ? id.split("/").pop()! : id;
  const short = bare.slice(0, 8);
  const [copied, setCopied] = useState(false);
  const copy = (e: React.MouseEvent) => {
    e.stopPropagation();
    navigator.clipboard.writeText(bare);
    setCopied(true);
    setTimeout(() => setCopied(false), 1200);
  };
  return (
    <span className={css.threadId}>
      <code className={css.threadIdText}>{short}</code>
      <button className={css.threadIdCopy} onClick={copy} aria-label="Copy thread ID">
        {copied ? "✓" : "⧉"}
      </button>
    </span>
  );
}

interface Props {
  sessions: Record<string, SessionInfo>;
  activeSessionId: string | null;
  activeThreadId: string | null;
  pinned: boolean;
  visible: boolean;
  remotes: RemoteInfo[];
  localStatus: ConnectionStatus;
  onSelect: (sessionId: string, threadId: string | null) => void;
  onNew: () => void;
  onTogglePin: () => void;
  onWidthChange: (width: number) => void;
  onDragStateChange: (dragging: boolean) => void;
}

function ThreadTree({
  threads,
  parentId,
  sessionId,
  activeThreadId,
  onSelect,
  depth,
}: {
  threads: SubthreadInfo[];
  parentId: string;
  sessionId: string;
  activeThreadId: string | null;
  onSelect: (sessionId: string, threadId: string | null) => void;
  depth: number;
}) {
  const children = threads.filter((t) => t.parent_thread_id === parentId);
  if (children.length === 0) return null;
  return (
    <div
      className={css.threadList}
      style={{ paddingLeft: depth > 0 ? 12 : 16 }}
    >
      {children.map((t) => (
        <div key={t.thread_id}>
          <button
            className={`${css.threadItem} ${activeThreadId === t.thread_id ? css.active : ""}`}
            onClick={(e) => {
              e.stopPropagation();
              onSelect(sessionId, t.thread_id);
            }}
          >
            <span className={css.threadLine} />
            <span className={css.threadTitle}>
              {t.title || t.thread_id.slice(0, 8)}
            </span>
          </button>
          <ThreadTree
            threads={threads}
            parentId={t.thread_id}
            sessionId={sessionId}
            activeThreadId={activeThreadId}
            onSelect={onSelect}
            depth={depth + 1}
          />
        </div>
      ))}
    </div>
  );
}

const MIN_WIDTH = 200;
const MAX_WIDTH = 480;
const DEFAULT_WIDTH = 272;

export function SessionSidebar({
  sessions,
  activeSessionId,
  activeThreadId,
  pinned,
  visible,
  remotes,
  localStatus,
  onSelect,
  onNew,
  onTogglePin,
  onWidthChange,
  onDragStateChange,
}: Props) {
  const [width, setWidth] = useState(DEFAULT_WIDTH);
  const dragging = useRef(false);

  // Publish sidebar offset as CSS variable on :root
  useEffect(() => {
    const offset = pinned ? width + 12 : 0;
    document.documentElement.style.setProperty(
      "--sidebar-offset",
      `${offset}px`,
    );
    onWidthChange(width);
  }, [pinned, width, onWidthChange]);

  const onDragStart = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    dragging.current = true;
    onDragStateChange(true);
    const onMove = (ev: MouseEvent) => {
      if (!dragging.current) return;
      // 12px left margin, so sidebar left edge is at 12
      const newW = Math.min(MAX_WIDTH, Math.max(MIN_WIDTH, ev.clientX - 12));
      setWidth(newW);
    };
    const onUp = () => {
      dragging.current = false;
      onDragStateChange(false);
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  }, [onDragStateChange]);

  const onDragDoubleClick = useCallback(() => {
    setWidth(DEFAULT_WIDTH);
  }, []);

  const sorted = Object.entries(sessions)
    .filter(([, info]) => info.status !== "Archived")
    .sort(([, a], [, b]) => b.last_updated.localeCompare(a.last_updated));

      return (
        <aside
          className={`${css.sidebar} ${!visible && !pinned ? css.hidden : ""}`}
          style={{ width }}
      >
        <div className={css.header}>
          <span className={css.title}>Sessions</span>
          <div className={css.headerActions}>
            <button className={css.newBtn} onClick={onNew}>
              + New
            </button>
            <button
              className={css.collapseBtn}
              onClick={onTogglePin}
              aria-label={pinned ? "Unpin sidebar" : "Pin sidebar"}
              data-pinned={pinned}
            >
                {"\u{1F4CC}"}
            </button>
          </div>
        </div>
      {localStatus !== "connected" && (
        <div className={css.remoteBanner}>
          <div
            className={css.remoteBannerItem}
            data-status={localStatus === "connecting" ? "connecting" : "disconnected"}
          >
            <span
              className={css.remoteBannerDot}
              data-status={localStatus === "connecting" ? "connecting" : "disconnected"}
            />
            local: {localStatus}
          </div>
        </div>
      )}
      {remotes.some((r) => r.status !== "connected") && (
        <div className={css.remoteBanner}>
          {remotes
            .filter((r) => r.status !== "connected")
            .map((r) => (
              <div
                key={r.name}
                className={css.remoteBannerItem}
                data-status={r.status === "connecting" ? "connecting" : "disconnected"}
              >
                <span
                  className={css.remoteBannerDot}
                  data-status={r.status === "connecting" ? "connecting" : "disconnected"}
                />
                {r.name}: {r.status}
              </div>
            ))}
        </div>
      )}
      <div className={css.list}>
        {sorted.map(([id, info]) => (
          <div key={id}>
            <button
              className={`${css.item} ${id === activeSessionId && !activeThreadId ? css.active : ""}`}
              onClick={() => onSelect(id, null)}
            >
              <span className={css.itemTitle}>
                <span className={css.itemTitleText}>
                  {info.title ||
                    (id.includes("/")
                      ? id.split("/").pop()!.slice(0, 8)
                      : id.slice(0, 8))}
                </span>
                {info.remote && (
                  <span className={css.remotePill}>{info.remote}</span>
                )}
              </span>
              <span className={css.itemMeta}>
                <span className={css.statusDot} data-status={info.status} />
                {info.total_tokens_used.toLocaleString()} tokens
                <CopyThreadId id={id} />
              </span>
            </button>
            {info.threads && info.threads.length > 0 && (
              <ThreadTree
                threads={info.threads}
                parentId={id}
                sessionId={id}
                activeThreadId={activeThreadId}
                onSelect={onSelect}
                depth={0}
              />
            )}
          </div>
        ))}
        {sorted.length === 0 && (
          <div className={css.empty}>No sessions yet</div>
        )}
      </div>
      <div
        className={css.resizeHandle}
        onMouseDown={onDragStart}
        onDoubleClick={onDragDoubleClick}
      />
    </aside>
  );
}
