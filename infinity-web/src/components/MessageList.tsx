import { useLayoutEffect, useRef, useCallback, memo, useMemo } from "react";
import type { MessageItem as MsgItem, SpinnerState } from "../types";
import { MessageItem } from "./MessageItem";
import { InputBar, type InputBarHandle } from "./InputBar";
import { ChoicePicker } from "./ChoicePicker";
import css from "./MessageList.module.css";

interface PendingChoice {
  prompt: string;
  choices: string[];
  default: number;
}

interface Props {
  messages: MsgItem[];
  generation: number;
  spinner: SpinnerState | null;
  onSend: (text: string) => void;
  inputDisabled: boolean;
  pendingChoice: PendingChoice | null;
  onChoiceSelect: (index: number) => void;
  theme?: "light" | "dark";
}

function isAtBottom(el: HTMLElement) {
  return el.scrollHeight - el.scrollTop - el.clientHeight < 40;
}

export const MessageList = memo(function MessageList({
  messages,
  generation,
  spinner,
  onSend,
  inputDisabled,
  pendingChoice,
  onChoiceSelect,
  theme,
}: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const shouldStickRef = useRef(true);
  const inputBarRef = useRef<InputBarHandle>(null);

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

  const handleSend = useCallback(
    (text: string) => {
      onSend(text);
      wasAtBottomRef.current = true;
      shouldStickRef.current = true;
      const el = containerRef.current;
      if (el) el.scrollTop = el.scrollHeight;
    },
    [onSend],
  );

  // Cache rendered message JSX. Messages are append-only (except reset
  // which bumps generation), and only the last message can be mutated
  // in-place. So we cache everything up to length-1 and re-render only
  // the tail.
  const cacheRef = useRef<{
    gen: number;
    theme: string | undefined;
    elements: React.ReactNode[];
  }>({ gen: -1, theme: undefined, elements: [] });

  const renderedMessages = useMemo(() => {
    const cache = cacheRef.current;
    const elements = cache.elements;

    // On generation change (reset) or theme change, invalidate entire cache
    if (cache.gen !== generation || cache.theme !== theme) {
      elements.length = 0;
    } else {
      // Pop the last element — it may have been mutated in-place
      if (elements.length > 0) elements.length--;
    }

    for (let i = elements.length; i < messages.length; i++) {
      const msg = messages[i];

      if (msg.type === "tool_call") {
        const next = messages[i + 1];
        if (next?.type === "tool_result") {
          elements.push(
            <div key={i} className={css.toolBox}>
              <MessageItem item={msg} isFirst={i === 0} theme={theme} />
              <hr className={css.toolDivider} />
              <MessageItem item={next} theme={theme} />
            </div>,
          );
          // Skip the tool_result on next iteration
          i++;
          elements.push(null);
          continue;
        }
        elements.push(
          <div key={i} className={css.toolBox}>
            <MessageItem item={msg} isFirst={i === 0} theme={theme} />
          </div>,
        );
        continue;
      }
      if (msg.type === "subscription") {
        elements.push(
          <div key={i} className={css.toolBox}>
            <MessageItem item={msg} isFirst={i === 0} theme={theme} />
          </div>,
        );
        continue;
      }
      if (
        msg.type === "tool_result" &&
        i > 0 &&
        messages[i - 1]?.type === "tool_call"
      ) {
        elements.push(null);
        continue;
      }
      if (msg.type === "thinking") {
        // Collapse thinking if followed by a non-thinking message
        const next = messages[i + 1];
        const collapsed = !!(next && next.type !== "thinking");
        elements.push(
          <MessageItem
            key={i}
            item={msg}
            isFirst={i === 0}
            defaultCollapsed={collapsed}
            theme={theme}
          />,
        );
        continue;
      }
      elements.push(
        <MessageItem key={i} item={msg} isFirst={i === 0} theme={theme} />,
      );
    }

    cache.gen = generation;
    cache.theme = theme;
    return elements;
  }, [messages, generation, theme]);

  return (
    <div className={css.container} ref={containerRef} onScroll={onScroll}>
      <div className={css.inner}>{renderedMessages}</div>
      <div className={css.inputArea}>
        {pendingChoice && (
          <ChoicePicker
            prompt={pendingChoice.prompt}
            choices={pendingChoice.choices}
            defaultIndex={pendingChoice.default}
            onSelect={onChoiceSelect}
            onFocusInput={() => inputBarRef.current?.focus()}
          />
        )}
        <InputBar
          ref={inputBarRef}
          onSend={handleSend}
          disabled={inputDisabled}
          spinner={spinner}
        />
      </div>
    </div>
  );
});
