import { Streamdown } from "streamdown";
import "streamdown/styles.css";
import { PatchDiff } from "@pierre/diffs/react";
import { useState, memo } from "react";
import type { MessageItem as MsgItem, DisplaySegment } from "../types";
import css from "./MessageItem.module.css";

interface Props {
  item: MsgItem;
  isFirst?: boolean;
  defaultCollapsed?: boolean;
  theme?: "light" | "dark";
}

export const MessageItem = memo(function MessageItem({
  item,
  isFirst,
  defaultCollapsed,
  theme,
}: Props) {
  switch (item.type) {
    case "user":
      return (
        <div
          className={`${css.user} ${css.markdown} ${isFirst ? "" : css.userSep}`}
        >
          <Streamdown
            controls={{ code: false, table: false }}
            linkSafety={{ enabled: false }}
            animated={false}
            isAnimating={false}
          >
            {item.text}
          </Streamdown>
        </div>
      );

    case "assistant":
      return (
        <div className={`${css.assistant} ${css.markdown}`}>
          <Streamdown
            controls={{ code: false, table: false }}
            linkSafety={{ enabled: false }}
            animated={{
              stagger: 0,
            }}
            isAnimating={!item.done}
          >
            {item.text}
          </Streamdown>
        </div>
      );

    case "thinking":
      return (
        <ThinkingBlock text={item.text} defaultCollapsed={defaultCollapsed} />
      );

    case "tool_call":
      return (
        <div className={css.toolCall}>
          <span className={css.toolIcon}>{"\u25C6"}</span>
          <span className={css.toolName}>{item.displayText}</span>
        </div>
      );

    case "tool_result": {
      // Find the first supported segment type to render
      const seg = item.segments.find(
        (s): s is DisplaySegment =>
          s.type === "diff" || s.type === "text" || s.type === "image",
      );
      if (!seg) return null;

      if (seg.type === "image") {
        return (
          <div className={css.toolResultImage}>
            <img
              src={`data:${seg.content.mediaType};base64,${seg.content.data}`}
              alt="tool result"
              data-testid="tool-result-image"
            />
          </div>
        );
      }

      if (seg.type === "diff") {
        return (
          <>
            {seg.content.patch.length > 0 && (
              <div className={css.toolResultDiff}>
                <PatchDiff
                  patch={seg.content.patch}
                  options={{
                    diffStyle: "unified",
                    themeType: theme ?? "system",
                  }}
                />
              </div>
            )}
            {seg.content.patch.length == 0 && (
              <div className={css.toolResult}>
                <span className={css.checkIcon}>{"\u2713"}</span>
                <span>Empty Diff</span>
              </div>
            )}
          </>
        );
      }

      // text segment
      const lines = seg.content.split("\n");
      if (lines.length <= 1) {
        return (
          <div className={css.toolResult}>
            <span className={css.checkIcon}>{"\u2713"}</span>
            <span>{lines[0]}</span>
          </div>
        );
      }
      return (
        <div className={css.toolResultMulti}>
          <div className={css.toolResult}>
            <span className={css.checkIcon}>{"\u2713"}</span>
            <span>{lines[0]}</span>
          </div>
          <pre className={css.toolResultPre}>{lines.slice(1).join("\n")}</pre>
        </div>
      );
    }

    case "info":
      return <div className={css.info}>{item.text}</div>;

    case "error":
      return <div className={css.error}>{item.text}</div>;

    case "subscription": {
      const lines = item.text.split("\n");
      return (
        <div>
          <div className={css.subHeader}>
            <span className={css.subIcon}>{"\u26A1"}</span>
            <span className={css.subName}>{item.name}</span>
          </div>
          <hr
            style={{
              border: "none",
              borderTop: "1px solid var(--border)",
              margin: "0 -12px",
            }}
          />
          {lines.length <= 1 ? (
            <div className={css.subBody}>{lines[0]}</div>
          ) : (
            <pre className={css.subBody}>{item.text}</pre>
          )}
        </div>
      );
    }

    case "compaction":
      return (
        <div className={css.compaction}>{"\u2726"} Compaction applied</div>
      );
  }
});

function ThinkingBlock({
  text,
  defaultCollapsed,
}: {
  text: string;
  defaultCollapsed?: boolean;
}) {
  // null = use default, true/false = user explicitly toggled
  const [userChoice, setUserChoice] = useState<boolean | null>(null);
  const collapsed = userChoice !== null ? userChoice : !!defaultCollapsed;

  return (
    <div className={css.thinking}>
      <button
        className={css.thinkingToggle}
        onClick={() =>
          setUserChoice((prev) => (prev !== null ? !prev : !collapsed))
        }
      >
        <span
          className={`${css.thinkingChevron} ${collapsed ? css.chevronCollapsed : ""}`}
        >
          {"\u25BE"}
        </span>
        <span className={css.thinkingLabel}>Thinking</span>
      </button>
      <div
        className={`${css.thinkingBody} ${collapsed ? css.thinkingCollapsed : ""}`}
      >
        <div className={css.thinkingBodyInner}>{text}</div>
      </div>
    </div>
  );
}
