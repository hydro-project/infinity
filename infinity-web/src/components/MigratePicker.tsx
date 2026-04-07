import { useState, useRef, useEffect, type KeyboardEvent } from 'react';
import type { RemoteInfo } from '../types';
import css from './MigratePicker.module.css';

interface Props {
  remotes: RemoteInfo[];
  currentHost?: string | null;
  title?: string;
  onConfirm: (destination: string | null, cwd: string) => void;
  onCancel: () => void;
}

type StatusKind = 'connected' | 'connecting' | 'disconnected';

function statusKind(status: string): StatusKind {
  if (status === 'connected') return 'connected';
  if (status === 'connecting') return 'connecting';
  return 'disconnected';
}

export function MigratePicker({ remotes, currentHost, title = 'Migrate session to', onConfirm, onCancel }: Props) {
  const [selected, setSelected] = useState<string | null | undefined>(undefined);
  const [cwd, setCwd] = useState('');
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (selected !== undefined) inputRef.current?.focus();
  }, [selected]);

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
  };

  const handleKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      if (selected !== undefined) {
        setSelected(undefined);
      } else {
        onCancel();
      }
    } else if (e.key === 'Enter' && selected !== undefined) {
      e.preventDefault();
      onConfirm(selected, cwd.trim() || '/');
    }
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
          <input
            ref={inputRef}
            className={css.input}
            value={cwd}
            onChange={(e) => setCwd(e.target.value)}
            onKeyDown={handleKeyDown}
            placeholder={`Working directory on ${selected ?? 'local'} (empty for /)`}
            spellCheck={false}
          />
        )}
      </div>
    </div>
  );
}
