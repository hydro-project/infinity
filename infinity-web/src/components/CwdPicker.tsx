import { useState, useRef, useEffect, type KeyboardEvent } from 'react';
import css from './CwdPicker.module.css';

interface Props {
  onConfirm: (cwd: string) => void;
  onCancel: () => void;
}

export function CwdPicker({ onConfirm, onCancel }: Props) {
  const [value, setValue] = useState('');
  const inputRef = useRef<HTMLInputElement>(null);

  // Future: directory listing state will go here
  // const [entries, setEntries] = useState<string[]>([]);
  // const [selectedIndex, setSelectedIndex] = useState(0);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const handleKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      onCancel();
    } else if (e.key === 'Enter') {
      e.preventDefault();
      onConfirm(value.trim() || '/');
    }
    // Future: ArrowUp/ArrowDown/Tab for directory navigation
  };

  return (
    <div className={css.overlay} onClick={onCancel}>
      <div className={css.picker} onClick={(e) => e.stopPropagation()}>
        <input
          ref={inputRef}
          className={css.input}
          value={value}
          onChange={(e) => setValue(e.target.value)}
          onKeyDown={handleKeyDown}
          placeholder="Working directory (empty for /)"
          spellCheck={false}
        />
        {/* Future: directory listing will render here */}
      </div>
    </div>
  );
}
