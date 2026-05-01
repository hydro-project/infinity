import React, { useEffect, useState, useRef, useCallback } from "react";

interface TerminalLine {
  text: string;
  className?: string;
  delay: number; // ms after previous line
  typing?: boolean; // character-by-character typing effect
  spacer?: boolean; // render as a visible blank line
}

const TERMINAL_LINES: TerminalLine[] = [
  //   { text: '', delay: 0 },
  //   { text: '$ rap-agent', className: 'command', delay: 600, typing: true },
  //   { text: '', delay: 400 },
  {
    text: "⚡ Agent ready. What can I help with?",
    className: "dim",
    delay: 300,
  },
  { text: "", delay: 200 },
  {
    text: "> Whenever there's a large purchase on my Stripe account, look up the customer and send them a personalized thank-you email.",
    className: "command",
    delay: 400,
    typing: true,
  },
  { text: "", delay: 600 },
  { text: "", spacer: true, delay: 0 },
  {
    text: '◆ subscribe_stripe_events({ event: "charge.succeeded", min: 500 })',
    className: "tool-call",
    delay: 400,
  },
  {
    text: "✓ Subscribed to Stripe charges > $500. ID: sub_9k2m",
    className: "result",
    delay: 500,
  },
  { text: "", spacer: true, delay: 200 },
  {
    text: "◆ sleep_until_event_or_input()",
    className: "tool-call",
    delay: 300,
  },
  {
    text: "💤 Hibernating... (runtime shut down)",
    className: "sleep-text",
    delay: 400,
  },
  { text: "", delay: 4000 },
  { text: "", spacer: true, delay: 0 },
  {
    text: "⚡ Woken by subscription event",
    className: "event",
    delay: 400,
  },
  {
    text: '  charge.succeeded — { id: "ch_3Nk9x", amount: "$847.00" }',
    className: "event",
    delay: 300,
  },
  { text: "", spacer: true, delay: 400 },
  {
    text: '◆ stripe_get_charge({ charge: "ch_3Nk9x" })',
    className: "tool-call",
    delay: 300,
  },
  {
    text: '✓ { name: "Sarah Chen", items: ["Weekender Bag", "Cashmere Scarf"] }',
    className: "result",
    delay: 800,
  },
  { text: "", spacer: true, delay: 300 },
  {
    text: '◆ send_email({ to: "sarah@acme.co", body: "Enjoy your purchase Sarah! The weekender bag pairs great with your scarf." })',
    className: "tool-call",
    delay: 300,
  },
  {
    text: "💤 Hibernating... (runtime shut down)",
    className: "sleep-text",
    delay: 500,
  },
  { text: "", spacer: true, delay: 2000 },
  {
    text: "⚡ Woken by tool call result:",
    className: "event",
    delay: 400,
  },
  {
    text: "✓ Email delivered.",
    className: "result",
    delay: 300,
  },
];

const TYPING_SPEED = 18; // ms per character

export default function AnimatedTerminal({
  active,
}: {
  active?: boolean;
}): React.JSX.Element {
  const [visibleLines, setVisibleLines] = useState<number>(0);
  const [typingIndex, setTypingIndex] = useState<number | null>(null); // which line is typing
  const [typingChars, setTypingChars] = useState<number>(0); // how many chars shown
  const bodyRef = useRef<HTMLDivElement>(null);
  const cancelRef = useRef<ReturnType<typeof setTimeout>>(null);

  const clearTimer = useCallback(() => {
    if (cancelRef.current) clearTimeout(cancelRef.current);
  }, []);

  useEffect(() => {
    if (active === false) {
      clearTimer();
      setVisibleLines(0);
      setTypingIndex(null);
      setTypingChars(0);
      return;
    }

    setVisibleLines(0);
    setTypingIndex(null);
    setTypingChars(0);
    let currentLine = 0;

    function showNext() {
      if (currentLine >= TERMINAL_LINES.length) {
        return;
      }

      const line = TERMINAL_LINES[currentLine];
      const delay = line.delay;

      cancelRef.current = setTimeout(() => {
        currentLine++;
        setVisibleLines(currentLine);

        if (line.typing && line.text.length > 0) {
          // Start typing effect
          setTypingIndex(currentLine - 1);
          setTypingChars(0);
          let charIdx = 0;

          function typeNext() {
            charIdx++;
            setTypingChars(charIdx);
            if (charIdx < line.text.length) {
              cancelRef.current = setTimeout(typeNext, TYPING_SPEED);
            } else {
              // Typing done
              setTypingIndex(null);
              showNext();
            }
          }

          cancelRef.current = setTimeout(typeNext, TYPING_SPEED);
        } else {
          showNext();
        }
      }, delay);
    }

    showNext();
    return clearTimer;
  }, [clearTimer, active]);

  useEffect(() => {
    if (bodyRef.current) {
      bodyRef.current.scrollTop = bodyRef.current.scrollHeight;
    }
  }, [visibleLines, typingChars]);

  const isTyping = typingIndex !== null;

  return (
    <div
      className="terminal"
      role="img"
      aria-label="Animated terminal showing a RAP agent processing a Stripe subscription event"
    >
      <div className="terminal-header">
        <div className="terminal-dot red" />
        <div className="terminal-dot yellow" />
        <div className="terminal-dot green" />
      </div>
      <div
        className="terminal-body"
        ref={bodyRef}
        style={{ overflowY: "auto" }}
      >
        {TERMINAL_LINES.slice(0, visibleLines).map((line, i) => {
          if (line.text === "" && !line.spacer) return null;
          if (line.spacer) {
            return (
              <div
                key={`${i}`}
                className="terminal-line"
                style={{ height: "0.5em" }}
              />
            );
          }

          const isThisLineTyping = typingIndex === i;
          const displayText = isThisLineTyping
            ? line.text.slice(0, typingChars)
            : line.text;

          return (
            <div
              key={`${i}`}
              className="terminal-line"
              style={{ animationDelay: "0ms" }}
            >
              <span className={line.className}>{displayText}</span>
              {isThisLineTyping && <span className="cursor-blink" />}
            </div>
          );
        })}
        {!isTyping && visibleLines < TERMINAL_LINES.length && (
          <div className="terminal-line" style={{ opacity: 1 }}>
            <span className="cursor-blink" />
          </div>
        )}
      </div>
    </div>
  );
}
