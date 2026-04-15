import { useState, useEffect, useRef, useCallback, forwardRef, useImperativeHandle } from 'react';
import css from './ChoicePicker.module.css';

interface Props {
  prompt: string;
  choices: string[];
  defaultIndex: number;
  onSelect: (index: number) => void;
  onFocusInput?: () => void;
}

export interface ChoicePickerHandle {
  focus: () => void;
}

export const ChoicePicker = forwardRef<ChoicePickerHandle, Props>(function ChoicePicker({ prompt, choices, defaultIndex, onSelect, onFocusInput }, fwdRef) {
  const [selected, setSelected] = useState(defaultIndex);
  const ref = useRef<HTMLDivElement>(null);

  useImperativeHandle(fwdRef, () => ({ focus: () => ref.current?.focus() }), []);

  useEffect(() => { ref.current?.focus(); }, []);

  const handleKeyDown = useCallback((e: React.KeyboardEvent) => {
    switch (e.key) {
      case 'ArrowUp':
        e.preventDefault();
        setSelected(i => Math.max(0, i - 1));
        break;
      case 'ArrowDown':
        e.preventDefault();
        setSelected(i => {
          const next = i + 1;
          if (next > choices.length - 1) {
            onFocusInput?.();
            return i;
          }
          return next;
        });
        break;
      case 'Enter':
        e.preventDefault();
        onSelect(selected);
        break;
      case 'Escape':
        e.preventDefault();
        onSelect(defaultIndex);
        break;
    }
  }, [choices.length, defaultIndex, onSelect, selected, onFocusInput]);

  return (
    <div className={css.picker} ref={ref} tabIndex={-1} onKeyDown={handleKeyDown}>
      <div className={css.inner}>
        <div className={css.prompt}>{prompt}</div>
        <div className={css.choices}>
          {choices.map((choice, i) => (
            <button
              key={i}
              className={i === selected ? css.choiceSelected : css.choice}
              onClick={() => onSelect(i)}
            >
              {choice}
              {i === defaultIndex && <span className={css.defaultTag}>(default)</span>}
            </button>
          ))}
        </div>
      </div>
    </div>
  );
});
