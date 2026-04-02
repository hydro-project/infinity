import { useState, useRef, useEffect, type KeyboardEvent } from 'react';
import type { RemoteInfo } from '../types';
import css from './MigratePicker.module.css';

interface Props {
  remotes: RemoteInfo[];
  currentHost: string;
  onConfirm: (destination: string, cwd: string) => void;
  onCancel: () => void;
}

type StatusKind = 'connected' | 'connecting' | 'disconnected';

function statusKind(status: string): StatusKind {
  if (status === 'connected') return 'connected';
  if (status === 'connecting') return 'connecting';
  return 'disconnected';
}

export function MigratePicker({ remotes, currentHost, onConfirm, onCancel }: Props) {
  const [selected, setSelected] = useState<string | null>(null);
  const [cwd, setCwd] = useState('');
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (selected) inputRef.current?.focus();
  }, [selected]);

  const destinations: { name: string; status: StatusKind; isCurrent: boolean }[] = [
    { name: 'local', status: 'connected', isCurrent: currentHost === 'local' },
    ...remotes.map((r) => ({
      name: r.name,
      status: statusKind(r.status),
      isCurrent: r.name === currentHost,
    })),
  ];

  const handleSelect = (dest: { name: string; status: StatusKind; isCurrent: boolean }) => {
    if (dest.isCurrent || dest.status !== 'connected') return;
    setSelected(dest.name);
    setCwd('');
  };

  const handleKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      if (selected) {
        setSelected(null);
      } else {
        onCancel();
      }
    } else if (e.key === 'Enter' && selected) {
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
        <div className={css.title}>Migrate session to</div>
        <ul className={css.list}>
          {destinations.map((d) => (
            <li
              key={d.name}
              className={css.item}
              data-disabled={String(d.status !== 'connected' && !d.isCurrent)}
              data-current={String(d.isCurrent)}
              onClick={() => handleSelect(d)}
            >
              <span className={css.dot} data-status={d.status} />
              <span className={css.label}>{d.name}</span>
              {d.isCurrent && <span className={css.current}>current</span>}
            </li>
          ))}
        </ul>
        {selected && (
          <input
            ref={inputRef}
            className={css.input}
            value={cwd}
            onChange={(e) => setCwd(e.target.value)}
            onKeyDown={handleKeyDown}
            placeholder={`Working directory on ${selected} (empty for /)`}
            spellCheck={false}
          />
        )}
      </div>
    </div>
  );
}
