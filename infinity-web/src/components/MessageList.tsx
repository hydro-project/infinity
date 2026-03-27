import { useLayoutEffect, useRef, useCallback } from 'react';
import type { MessageItem as MsgItem, SpinnerState } from '../types';
import { MessageItem } from './MessageItem';
import { InputBar } from './InputBar';
import { ChoicePicker } from './ChoicePicker';
import css from './MessageList.module.css';

interface PendingChoice {
  prompt: string;
  choices: string[];
  default: number;
}

interface Props {
  messages: MsgItem[];
  spinner: SpinnerState | null;
  onSend: (text: string) => void;
  inputDisabled: boolean;
  pendingChoice: PendingChoice | null;
  onChoiceSelect: (index: number) => void;
}

function isAtBottom(el: HTMLElement) {
  return el.scrollHeight - el.scrollTop - el.clientHeight < 40;
}

export function MessageList({ messages, spinner, onSend, inputDisabled, pendingChoice, onChoiceSelect }: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const shouldStickRef = useRef(true);

  // Before React commits DOM changes, snapshot whether we're at bottom
  const wasAtBottomRef = useRef(true);
  useLayoutEffect(() => {
    // After DOM update, if we were stuck, scroll to bottom
    if (wasAtBottomRef.current) {
      const el = containerRef.current;
      if (el) el.scrollTop = el.scrollHeight;
    }
  }, [messages, pendingChoice]);

  const onScroll = useCallback(() => {
    const el = containerRef.current;
    if (!el) return;
    wasAtBottomRef.current = isAtBottom(el);
    shouldStickRef.current = wasAtBottomRef.current;
  }, []);

  const handleSend = useCallback((text: string) => {
    onSend(text);
    wasAtBottomRef.current = true;
    shouldStickRef.current = true;
    const el = containerRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [onSend]);

  // Determine which thinking blocks should default to collapsed:
  // any thinking block followed by a non-thinking message
  const thinkingDefaultCollapsed = new Set<number>();
  for (let i = 0; i < messages.length; i++) {
    if (messages[i].type === 'thinking') {
      const next = messages[i + 1];
      if (next && next.type !== 'thinking') {
        thinkingDefaultCollapsed.add(i);
      }
    }
  }

  return (
    <div className={css.container} ref={containerRef} onScroll={onScroll}>
      <div className={css.inner}>
        {messages.map((msg, i) => {
          if (msg.type === 'tool_call') {
            const next = messages[i + 1];
            if (next?.type === 'tool_result') return (
              <div key={i} className={css.toolBox}>
                <MessageItem item={msg} isFirst={i === 0} />
                <hr className={css.toolDivider} />
                <MessageItem item={next} />
              </div>
            );
            return (
              <div key={i} className={css.toolBox}>
                <MessageItem item={msg} isFirst={i === 0} />
              </div>
            );
          }
          if (msg.type === 'subscription') {
            return (
              <div key={i} className={css.toolBox}>
                <MessageItem item={msg} isFirst={i === 0} />
              </div>
            );
          }
          if (msg.type === 'tool_result' && i > 0 && messages[i - 1]?.type === 'tool_call') {
            return null;
          }
          if (msg.type === 'thinking') {
            return <MessageItem key={i} item={msg} isFirst={i === 0} defaultCollapsed={thinkingDefaultCollapsed.has(i)} />;
          }
          return <MessageItem key={i} item={msg} isFirst={i === 0} />;
        })}
      </div>
      <div className={css.inputArea}>
        {pendingChoice ? (
          <ChoicePicker
            prompt={pendingChoice.prompt}
            choices={pendingChoice.choices}
            defaultIndex={pendingChoice.default}
            onSelect={onChoiceSelect}
          />
        ) : (
          <InputBar onSend={handleSend} disabled={inputDisabled} spinner={spinner} />
        )}
      </div>
    </div>
  );
}
