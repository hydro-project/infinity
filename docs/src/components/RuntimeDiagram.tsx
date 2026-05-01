import React, { useEffect, useState, useCallback } from "react";

interface ChildNode {
  label: string;
  x: number;
}

const CHILDREN: ChildNode[] = [
  { label: "Review auth.ts", x: 100 },
  { label: "Run tests", x: 300 },
  { label: "Update docs", x: 500 },
];

const PARENT_X = 300;
const PARENT_Y = 60;
const CHILD_Y = 280;
const NODE_W = 150;
const NODE_H = 44;
const CYCLE_MS = 8000;

type Phase =
  | "idle"
  | "spawn0"
  | "spawn1"
  | "spawn2"
  | "reporting"
  | "done0"
  | "done1"
  | "done2";

const PHASE_SCHEDULE: { phase: Phase; at: number }[] = [
  { phase: "idle", at: 0 },
  { phase: "spawn0", at: 1000 },
  { phase: "spawn1", at: 2000 },
  { phase: "spawn2", at: 3000 },
  { phase: "reporting", at: 3500 },
  { phase: "done0", at: 6000 },
  { phase: "done1", at: 6500 },
  { phase: "done2", at: 7000 },
];

function phaseIndex(phase: Phase): number {
  return PHASE_SCHEDULE.findIndex((p) => p.phase === phase);
}

export default function RuntimeDiagram({
  active,
}: {
  active: boolean;
}): React.JSX.Element {
  const [phase, setPhase] = useState<Phase>("idle");

  const runCycle = useCallback(() => {
    const timers: ReturnType<typeof setTimeout>[] = [];
    for (const { phase: p, at } of PHASE_SCHEDULE) {
      timers.push(setTimeout(() => setPhase(p), at));
    }
    return timers;
  }, []);

  useEffect(() => {
    if (!active) {
      setPhase("idle");
      return;
    }
    let timers = runCycle();
    const interval = setInterval(() => {
      timers.forEach(clearTimeout);
      timers = runCycle();
    }, CYCLE_MS);
    return () => {
      timers.forEach(clearTimeout);
      clearInterval(interval);
    };
  }, [runCycle, active]);

  const currentIdx = phaseIndex(phase);
  const childVisible = (i: number) =>
    currentIdx >= phaseIndex(("spawn" + i) as Phase);
  const childDone = (i: number) =>
    currentIdx >= phaseIndex(("done" + i) as Phase);
  const showPulses = currentIdx >= phaseIndex("reporting");

  return (
    <div style={{ width: "100%", maxWidth: 600, margin: "0 auto" }}>
      <svg
        viewBox="0 0 600 380"
        width="100%"
        height="100%"
        style={{ overflow: "visible" }}
      >
        <defs>
          <style>{`
            .rt-node {
              transition: opacity 0.4s ease, transform 0.4s ease;
            }
            .rt-line {
              transition: opacity 0.4s ease;
            }
            @keyframes rt-pulse-up {
              0% { offset-distance: 100%; opacity: 0; }
              10% { opacity: 1; }
              90% { opacity: 1; }
              100% { offset-distance: 0%; opacity: 0; }
            }
            @keyframes rt-glow {
              0%, 100% { opacity: 0.4; }
              50% { opacity: 0.8; }
            }
          `}</style>
          {CHILDREN.map((child, i) => (
            <path
              key={`path-${i}`}
              id={`conn-${i}`}
              d={`M ${PARENT_X} ${PARENT_Y + NODE_H} C ${PARENT_X} ${(PARENT_Y + CHILD_Y) / 2}, ${child.x} ${(PARENT_Y + CHILD_Y) / 2}, ${child.x} ${CHILD_Y}`}
              fill="none"
            />
          ))}
        </defs>

        {/* Connection lines */}
        {CHILDREN.map((child, i) => (
          <use
            key={`line-${i}`}
            href={`#conn-${i}`}
            stroke="var(--ifm-color-emphasis-300)"
            strokeWidth="2"
            className="rt-line"
            style={{ opacity: childVisible(i) ? 1 : 0 }}
          />
        ))}

        {/* Pulse dots traveling up connections */}
        {CHILDREN.map((_, i) =>
          showPulses && childVisible(i) && !childDone(i) ? (
            <React.Fragment key={`pulses-${i}`}>
              <circle
                r="5"
                fill="var(--ifm-color-primary)"
                style={{
                  offsetPath: `path("M ${CHILDREN[i].x} ${CHILD_Y} C ${CHILDREN[i].x} ${(PARENT_Y + CHILD_Y) / 2}, ${PARENT_X} ${(PARENT_Y + CHILD_Y) / 2}, ${PARENT_X} ${PARENT_Y + NODE_H}")`,
                  animation: `rt-pulse-up 1.5s ease-in-out infinite`,
                  animationDelay: `${i * 0.4}s`,
                }}
              />
              <circle
                r="5"
                fill="var(--ifm-color-primary)"
                style={{
                  offsetPath: `path("M ${CHILDREN[i].x} ${CHILD_Y} C ${CHILDREN[i].x} ${(PARENT_Y + CHILD_Y) / 2}, ${PARENT_X} ${(PARENT_Y + CHILD_Y) / 2}, ${PARENT_X} ${PARENT_Y + NODE_H}")`,
                  animation: `rt-pulse-up 1.5s ease-in-out infinite`,
                  animationDelay: `${i * 0.4 + 0.75}s`,
                }}
              />
            </React.Fragment>
          ) : null,
        )}

        {/* Parent node */}
        <g>
          <rect
            x={PARENT_X - NODE_W / 2}
            y={PARENT_Y}
            width={NODE_W}
            height={NODE_H}
            rx={10}
            fill="var(--ifm-background-surface-color)"
            stroke="var(--ifm-color-primary)"
            strokeWidth="2"
          />
          {/* Gentle glow */}
          <rect
            x={PARENT_X - NODE_W / 2}
            y={PARENT_Y}
            width={NODE_W}
            height={NODE_H}
            rx={10}
            fill="none"
            stroke="var(--ifm-color-primary)"
            strokeWidth="4"
            style={{ animation: "rt-glow 2s ease-in-out infinite" }}
          />
          <text
            x={PARENT_X}
            y={PARENT_Y + NODE_H / 2 + 1}
            textAnchor="middle"
            dominantBaseline="central"
            fill="var(--ifm-font-color-base)"
            fontSize="14"
            fontWeight="600"
            fontFamily="-apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif"
          >
            Parent
          </text>
        </g>

        {/* Child nodes */}
        {CHILDREN.map((child, i) => {
          const visible = childVisible(i);
          const done = childDone(i);
          return (
            <g
              key={`child-${i}`}
              className="rt-node"
              style={{
                opacity: visible ? 1 : 0,
                transform: visible ? "translateY(0)" : "translateY(-20px)",
              }}
            >
              <rect
                x={child.x - NODE_W / 2}
                y={CHILD_Y}
                width={NODE_W}
                height={NODE_H}
                rx={10}
                fill="var(--ifm-background-surface-color)"
                stroke={done ? "#28c840" : "var(--ifm-color-emphasis-200)"}
                strokeWidth="2"
                style={{ transition: "stroke 0.3s ease" }}
              />
              <text
                x={child.x}
                y={CHILD_Y + (done ? 17 : NODE_H / 2 + 1)}
                textAnchor="middle"
                dominantBaseline="central"
                fill="var(--ifm-font-color-base)"
                fontSize="12"
                fontWeight="500"
                fontFamily="-apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif"
              >
                {child.label}
              </text>
              {done && (
                <text
                  x={child.x}
                  y={CHILD_Y + 32}
                  textAnchor="middle"
                  dominantBaseline="central"
                  fill="#28c840"
                  fontSize="11"
                  fontWeight="600"
                  fontFamily="-apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif"
                >
                  done ✓
                </text>
              )}
            </g>
          );
        })}
      </svg>
    </div>
  );
}
