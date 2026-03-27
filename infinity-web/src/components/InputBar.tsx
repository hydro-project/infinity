import { useState, useRef, useCallback, type KeyboardEvent } from 'react';
import type { SpinnerState } from '../types';
import { Spinner } from './Spinner';
import css from './InputBar.module.css';

interface Props {
  onSend: (text: string) => void;
  disabled: boolean;
  spinner: SpinnerState | null;
}

export function InputBar({ onSend, disabled, spinner }: Props) {
  const [value, setValue] = useState('');
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  const submit = useCallback(() => {
    const trimmed = value.trim();
    if (!trimmed) return;
    onSend(trimmed);
    setValue('');
    // Reset height
    if (textareaRef.current) textareaRef.current.style.height = 'auto';
  }, [value, onSend]);

  const handleKeyDown = useCallback(
    (e: KeyboardEvent<HTMLTextAreaElement>) => {
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        submit();
      }
    },
    [submit],
  );

  const handleInput = useCallback(() => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = 'auto';
    el.style.height = Math.min(el.scrollHeight, 200) + 'px';
  }, []);

  return (
    <div className={css.bar}>
      {spinner && <Spinner state={spinner} />}
      <div className={css.inner}>
        <textarea
          ref={textareaRef}
          className={css.textarea}
          value={value}
          onChange={(e) => { setValue(e.target.value); handleInput(); }}
          onKeyDown={handleKeyDown}
          placeholder={disabled ? 'Connecting…' : 'Send a message…'}
          disabled={disabled}
          rows={1}
        />
        <button
          className={css.sendBtn}
          onClick={submit}
          disabled={disabled || !value.trim()}
          aria-label="Send"
        >
          <svg width="16" height="16" viewBox="0 0 16 16" fill="none">
            <path d="M2 14L14.5 8L2 2V6.5L10 8L2 9.5V14Z" fill="currentColor" />
          </svg>
        </button>
      </div>
    </div>
  );
}
