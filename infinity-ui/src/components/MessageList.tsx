import {
  useLayoutEffect,
  useRef,
  useCallback,
  useImperativeHandle,
  forwardRef,
  memo,
  useMemo,
} from "react";
import type { MessageItem as MsgItem } from "../types";
import { MessageItem } from "./MessageItem";
import css from "./MessageList.module.css";

interface Props {
  messages: MsgItem[];
  generation: number;
  theme?: "light" | "dark";
}

export interface MessageListHandle {
  scrollToBottom: () => void;
}

function isAtBottom(el: HTMLElement) {
  return el.scrollHeight - el.scrollTop - el.clientHeight < 40;
}

export const MessageList = memo(
  forwardRef<MessageListHandle, Props>(function MessageList(
    { messages, generation, theme },
    fwdRef,
  ) {
    const containerRef = useRef<HTMLDivElement>(null);

    // Before React commits DOM changes, snapshot whether we're at bottom
    const wasAtBottomRef = useRef(true);
    useLayoutEffect(() => {
      // After DOM update, if we were stuck, scroll to bottom
      if (wasAtBottomRef.current) {
        const el = containerRef.current;
        if (el) el.scrollTop = el.scrollHeight;
      }
    }, [messages]);

    const onScroll = useCallback(() => {
      const el = containerRef.current;
      if (!el) return;
      wasAtBottomRef.current = isAtBottom(el);
    }, []);

    useImperativeHandle(
      fwdRef,
      () => ({
        scrollToBottom: () => {
          wasAtBottomRef.current = true;
          const el = containerRef.current;
          if (el) el.scrollTop = el.scrollHeight;
        },
      }),
      [],
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
      </div>
    );
  }),
);
