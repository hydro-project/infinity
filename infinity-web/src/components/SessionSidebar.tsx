import { useCallback, useRef, useState, useEffect } from 'react';
import type { SessionInfo } from '../types';
import css from './SessionSidebar.module.css';

interface Props {
  sessions: Record<string, SessionInfo>;
  activeSessionId: string | null;
  open: boolean;
  onSelect: (id: string) => void;
  onNew: () => void;
  onClose: () => void;
}

const MIN_WIDTH = 200;
const MAX_WIDTH = 480;
const DEFAULT_WIDTH = 272;

export function SessionSidebar({ sessions, activeSessionId, open, onSelect, onNew, onClose }: Props) {
  const [width, setWidth] = useState(DEFAULT_WIDTH);
  const dragging = useRef(false);

  // Publish sidebar offset as CSS variable on :root
  useEffect(() => {
    const offset = open ? width + 24 : 0; // 12px left + 12px gap
    document.documentElement.style.setProperty('--sidebar-offset', `${offset}px`);
  }, [open, width]);

  const onDragStart = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    dragging.current = true;
    const onMove = (ev: MouseEvent) => {
      if (!dragging.current) return;
      // 12px left margin, so sidebar left edge is at 12
      const newW = Math.min(MAX_WIDTH, Math.max(MIN_WIDTH, ev.clientX - 12));
      setWidth(newW);
    };
    const onUp = () => {
      dragging.current = false;
      window.removeEventListener('mousemove', onMove);
      window.removeEventListener('mouseup', onUp);
    };
    window.addEventListener('mousemove', onMove);
    window.addEventListener('mouseup', onUp);
  }, []);

  const onDragDoubleClick = useCallback(() => {
    setWidth(DEFAULT_WIDTH);
  }, []);

  const sorted = Object.entries(sessions).sort(
    ([, a], [, b]) => b.last_updated.localeCompare(a.last_updated),
  );

  return (
    <aside
      className={`${css.sidebar} ${!open ? css.hidden : ''}`}
      style={{ width }}
    >
      <div className={css.header}>
        <span className={css.title}>Sessions</span>
        <div className={css.headerActions}>
          <button className={css.newBtn} onClick={onNew}>+ New</button>
          <button className={css.collapseBtn} onClick={onClose} aria-label="Hide sidebar">{'\u2715'}</button>
        </div>
      </div>
      <div className={css.list}>
        {sorted.map(([id, info]) => (
          <button
            key={id}
            className={`${css.item} ${id === activeSessionId ? css.active : ''}`}
            onClick={() => onSelect(id)}
          >
            <span className={css.itemTitle}>
              {info.title || id.slice(0, 8)}
            </span>
            <span className={css.itemMeta}>
              <span className={css.statusDot} data-status={info.status} />
              {info.total_tokens_used.toLocaleString()} tokens
            </span>
          </button>
        ))}
        {sorted.length === 0 && (
          <div className={css.empty}>No sessions yet</div>
        )}
      </div>
      <div className={css.resizeHandle} onMouseDown={onDragStart} onDoubleClick={onDragDoubleClick} />
    </aside>
  );
}
