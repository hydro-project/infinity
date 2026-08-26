import React, { useEffect, useRef, useState } from "react";

/**
 * AgentTrace: a transcript of one agent working asynchronously.
 *
 * Reads like a session log: the agent backgrounds a command, then goes idle.
 * Subscription events wake it for one short turn each, and the idle rows
 * state how long nothing ran in between.
 *
 * All lines are always rendered (revealed by opacity only), so the container
 * height never changes. Idle rows pause the reveal longer, acting out the
 * hibernation they describe.
 */

type Line =
  | {
      kind: "msg";
      tag: "user" | "agent" | "event";
      text: string;
      note?: string;
    }
  | { kind: "idle"; text: string };

/** Each line appears `delay` ms after the previous one. */
const LINES: (Line & { delay: number })[] = [
  {
    kind: "msg",
    tag: "user",
    text: "Run the test suite and write release notes for 0.4.",
    delay: 300,
  },
  {
    kind: "msg",
    tag: "agent",
    text: 'run_command("cargo test --workspace")',
    note: "returns immediately; output streams back as events",
    delay: 900,
  },
  {
    kind: "msg",
    tag: "agent",
    text: 'edit_file("docs/release-notes.md")',
    note: "keeps working while the tests run",
    delay: 900,
  },
  {
    kind: "idle",
    text: "18 minutes idle: no task, no polling, zero compute",
    delay: 1100,
  },
  {
    kind: "msg",
    tag: "event",
    text: "cargo test: exit 1, FAILED checkout::refunds (refunds.rs:214)",
    delay: 2600,
  },
  {
    kind: "msg",
    tag: "agent",
    text: 'edit_file("src/checkout/refunds.rs")',
    delay: 900,
  },
  {
    kind: "msg",
    tag: "agent",
    text: 'run_command("cargo test checkout::refunds")',
    note: "reruns the test it fixed",
    delay: 900,
  },
  { kind: "idle", text: "2 minutes idle", delay: 1100 },
  {
    kind: "msg",
    tag: "event",
    text: "cargo test: exit 0",
    delay: 2600,
  },
  {
    kind: "msg",
    tag: "agent",
    text: "Release notes drafted; all tests green after fixing the refunds rounding.",
    delay: 900,
  },
];

export default function AgentTrace({
  active,
}: {
  active: boolean;
}): React.JSX.Element {
  const [shown, setShown] = useState(0);
  const timersRef = useRef<ReturnType<typeof setTimeout>[]>([]);

  useEffect(() => {
    timersRef.current.forEach(clearTimeout);
    timersRef.current = [];
    if (!active) {
      setShown(0);
      return;
    }
    setShown(0);
    let at = 0;
    LINES.forEach((line, i) => {
      at += line.delay;
      timersRef.current.push(setTimeout(() => setShown(i + 1), at));
    });
    return () => {
      timersRef.current.forEach(clearTimeout);
    };
  }, [active]);

  return (
    <div
      className="trace"
      role="img"
      aria-label="Transcript of one agent session: the agent backgrounds a test run, keeps working on release notes while it runs, and goes idle; command output events wake it for one short turn each"
    >
      {LINES.map((line, i) => {
        const style = { opacity: i < shown ? 1 : 0 };
        if (line.kind === "idle") {
          return (
            <div key={i} className="trace-idle" style={style}>
              {line.text}
            </div>
          );
        }
        return (
          <div key={i} className="trace-line" style={style}>
            <span className={`trace-tag trace-tag-${line.tag}`}>
              {line.tag}
            </span>
            <span className="trace-text">
              {line.text}
              {line.note && <span className="trace-note"> ({line.note})</span>}
            </span>
          </div>
        );
      })}
    </div>
  );
}
