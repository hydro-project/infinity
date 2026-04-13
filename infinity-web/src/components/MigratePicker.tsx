import { useState, useRef, useEffect, useCallback, type KeyboardEvent } from 'react';
import type { RemoteInfo, ClientMessage } from '../types';
import css from './MigratePicker.module.css';

interface Props {
  remotes: RemoteInfo[];
  currentHost?: string | null;
  title?: string;
  onConfirm: (destination: string | null, cwd: string) => void;
  onCancel: () => void;
  send: (msg: ClientMessage) => void;
  directoryEntries: string[];
  onClearEntries?: () => void;
}

type StatusKind = 'connected' | 'connecting' | 'disconnected';

function statusKind(status: string): StatusKind {
  if (status === 'connected') return 'connected';
  if (status === 'connecting') return 'connecting';
  return 'disconnected';
}

export function MigratePicker({ remotes, currentHost, title = 'Migrate session to', onConfirm, onCancel, send, directoryEntries, onClearEntries }: Props) {
  const [selected, setSelected] = useState<string | null | undefined>(undefined);
  const [cwd, setCwd] = useState('');
  const [completionIndex, setCompletionIndex] = useState(-1);
  const [showDropdown, setShowDropdown] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLUListElement>(null);

  // Server already filters entries by prefix, use them directly
  const filteredEntries = directoryEntries;

  // Compute ghost text: the suffix of the first matching completion beyond what's typed
  const ghostSuffix = (() => {
    if (filteredEntries.length === 0 || cwd === '') return '';
    const target = completionIndex >= 0 ? filteredEntries[completionIndex] : filteredEntries[0];
    if (!target || !target.startsWith(cwd)) return '';
    return target.slice(cwd.length);
  })();

  // Clear stale directory entries from previous picker instances on mount
  useEffect(() => {
    onClearEntries?.();
  }, []);  // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (selected !== undefined) inputRef.current?.focus();
  }, [selected]);

  // Auto-request completions when input changes
  useEffect(() => {
    if (selected === undefined) return;
    const on = selected;  // null = local, string = remote name
    if (cwd.endsWith('/') || cwd === '') {
      send({ ListDirectory: { path: cwd || '/', on } });
    } else if (cwd.length > 0) {
      send({ ListDirectory: { path: cwd, on } });
    }
  }, [cwd, selected, send]);

  // Scroll selected completion into view
  useEffect(() => {
    if (completionIndex >= 0 && listRef.current) {
      const item = listRef.current.children[completionIndex] as HTMLElement | undefined;
      item?.scrollIntoView({ block: 'nearest' });
    }
  }, [completionIndex]);

  const destinations: { name: string | null; displayName: string; status: StatusKind; isCurrent: boolean }[] = [
    { name: null, displayName: 'local', status: 'connected', isCurrent: currentHost !== undefined && currentHost == null },
    ...remotes.map((r) => ({
      name: r.name,
      displayName: r.name,
      status: statusKind(r.status),
      isCurrent: currentHost !== undefined && r.name === currentHost,
    })),
  ];

  const handleSelect = (dest: { name: string | null; status: StatusKind; isCurrent: boolean }) => {
    if (dest.isCurrent || dest.status !== 'connected') return;
    setSelected(dest.name);
    setCwd('');
    setShowDropdown(false);
  };

  const acceptCompletion = useCallback((entry: string) => {
    setCwd(entry);
    setShowDropdown(false);
    setCompletionIndex(-1);
    inputRef.current?.focus();
  }, []);

  const handleKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      if (showDropdown) {
        setShowDropdown(false);
        setCompletionIndex(-1);
      } else if (selected !== undefined) {
        setSelected(undefined);
      } else {
        onCancel();
      }
    } else if (e.key === 'Tab') {
      e.preventDefault();
      if (ghostSuffix) {
        // Accept the ghost text
        const target = completionIndex >= 0 ? filteredEntries[completionIndex] : filteredEntries[0];
        if (target) {
          acceptCompletion(target);
        }
      } else if (filteredEntries.length > 1) {
        // Show dropdown for multiple options
        setShowDropdown(true);
        setCompletionIndex(-1);
      }
    } else if (e.key === 'Enter') {
      e.preventDefault();
      if (showDropdown && completionIndex >= 0 && completionIndex < filteredEntries.length) {
        acceptCompletion(filteredEntries[completionIndex]);
      } else if (selected !== undefined) {
        onConfirm(selected, cwd.trim() || '/');
      }
    } else if (e.key === 'ArrowDown') {
      if (filteredEntries.length > 1) {
        e.preventDefault();
        setShowDropdown(true);
        setCompletionIndex(i => (i + 1) % filteredEntries.length);
      }
    } else if (e.key === 'ArrowUp') {
      if (filteredEntries.length > 1) {
        e.preventDefault();
        setShowDropdown(true);
        setCompletionIndex(i => (i <= 0 ? filteredEntries.length - 1 : i - 1));
      }
    }
  };

  const handleInputChange = (val: string) => {
    setCwd(val);
    setShowDropdown(false);
    setCompletionIndex(-1);
  };

  const handleOverlayKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      onCancel();
    }
  };

  return (
    <div className={css.overlay} onClick={onCancel} onKeyDown={handleOverlayKeyDown}>
      <div className={css.picker} onClick={(e) => e.stopPropagation()}>
        <div className={css.title}>{title}</div>
        <ul className={css.list}>
          {destinations.map((d) => (
            <li
              key={d.displayName}
              className={css.item}
              data-disabled={String(d.status !== 'connected' && !d.isCurrent)}
              data-current={String(d.isCurrent)}
              onClick={() => handleSelect(d)}
            >
              <span className={css.dot} data-status={d.status} />
              <span className={css.label}>{d.displayName}</span>
              {d.isCurrent && <span className={css.current}>current</span>}
            </li>
          ))}
        </ul>
        {selected !== undefined && (
          <div className={css.inputWrapper}>
            <div className={css.inputRow}>
              <input
                ref={inputRef}
                className={css.input}
                value={cwd}
                onChange={(e) => handleInputChange(e.target.value)}
                onKeyDown={handleKeyDown}
                placeholder={`Working directory on ${selected ?? 'local'} (Tab to complete)`}
                spellCheck={false}
              />
              {ghostSuffix && (
                <span className={css.ghost} aria-hidden>
                  <span className={css.ghostHidden}>{cwd}</span>
                  <span className={css.ghostText}>{ghostSuffix}</span>
                </span>
              )}
            </div>
            {showDropdown && filteredEntries.length > 0 && (
              <ul ref={listRef} className={css.completions}>
                {filteredEntries.map((entry, i) => (
                  <li
                    key={entry}
                    className={css.completionItem}
                    data-selected={String(i === completionIndex)}
                    onClick={() => acceptCompletion(entry)}
                  >
                    {entry}
                  </li>
                ))}
              </ul>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
