import React, { useEffect, useState, useRef } from "react";

/**
 * RuntimeDiagram — visualizes time-slicing.
 *
 * Phase 1 ("Traditional"): agents hold resources even while idle.
 * Phase 2 ("Infinity"): idle gaps collapse; active blocks bin-pack into minimal rows.
 */

const COLORS = [
  "#79c0ff",
  "#7ee787",
  "#b392f0",
  "#ffa657",
  "#f97583",
  "#56d4dd",
];
const AGENT_LABELS = [
  "Agent A",
  "Agent B",
  "Agent C",
  "Agent D",
  "Agent E",
  "Agent F",
];

interface Segment {
  start: number;
  end: number;
  active: boolean;
}

const TIMELINES: Segment[][] = [
  // Agent A
  [
    { start: 0, end: 8, active: true },
    { start: 8, end: 30, active: false },
    { start: 30, end: 38, active: true },
    { start: 38, end: 72, active: false },
    { start: 72, end: 82, active: true },
    { start: 82, end: 100, active: false },
  ],
  // Agent B
  [
    { start: 0, end: 15, active: false },
    { start: 15, end: 25, active: true },
    { start: 25, end: 55, active: false },
    { start: 55, end: 65, active: true },
    { start: 65, end: 100, active: false },
  ],
  // Agent C
  [
    { start: 0, end: 35, active: false },
    { start: 35, end: 50, active: true },
    { start: 50, end: 80, active: false },
    { start: 80, end: 92, active: true },
    { start: 92, end: 100, active: false },
  ],
  // Agent D
  [
    { start: 0, end: 5, active: false },
    { start: 5, end: 12, active: true },
    { start: 12, end: 60, active: false },
    { start: 60, end: 70, active: true },
    { start: 70, end: 85, active: false },
    { start: 85, end: 95, active: true },
    { start: 95, end: 100, active: false },
  ],
  // Agent E
  [
    { start: 0, end: 20, active: false },
    { start: 20, end: 30, active: true },
    { start: 30, end: 68, active: false },
    { start: 68, end: 78, active: true },
    { start: 78, end: 100, active: false },
  ],
  // Agent F
  [
    { start: 0, end: 42, active: false },
    { start: 42, end: 52, active: true },
    { start: 52, end: 88, active: false },
    { start: 88, end: 100, active: true },
  ],
];

// Collect all active segments with metadata
interface ActiveBlock {
  agentIdx: number;
  start: number;
  end: number;
  color: string;
  // Computed: which packed row this block goes into
  packedRow: number;
}

const ACTIVE_BLOCKS: ActiveBlock[] = [];
for (let i = 0; i < TIMELINES.length; i++) {
  for (const seg of TIMELINES[i]) {
    if (seg.active) {
      ACTIVE_BLOCKS.push({
        agentIdx: i,
        start: seg.start,
        end: seg.end,
        color: COLORS[i],
        packedRow: 0,
      });
    }
  }
}
// Sort by start time for greedy bin-packing
ACTIVE_BLOCKS.sort((a, b) => a.start - b.start);
// First-fit bin packing
const rowEnds: number[] = [];
for (const block of ACTIVE_BLOCKS) {
  let placed = false;
  for (let r = 0; r < rowEnds.length; r++) {
    if (rowEnds[r] <= block.start) {
      block.packedRow = r;
      rowEnds[r] = block.end;
      placed = true;
      break;
    }
  }
  if (!placed) {
    block.packedRow = rowEnds.length;
    rowEnds.push(block.end);
  }
}
const PACKED_ROW_COUNT = rowEnds.length;

// Layout constants
const SVG_W = 800;
const SVG_H = 300;
const LANE_H = 28;
const MARGIN_LEFT = 40;
const MARGIN_RIGHT = 60;
const TRACK_W = SVG_W - MARGIN_LEFT - MARGIN_RIGHT;

const TRAD_TOP = 30;
const TRAD_LANE_PITCH = 48;

const SLICED_TOP = TRAD_TOP;
const SLICED_LANE_PITCH = LANE_H + 6;

type Phase = "initial" | "highlight" | "sliced";

export default function RuntimeDiagram({
  active,
}: {
  active: boolean;
}): React.JSX.Element {
  const [phase, setPhase] = useState<Phase>("initial");
  const timerRef = useRef<ReturnType<typeof setTimeout>[]>([]);

  useEffect(() => {
    timerRef.current.forEach(clearTimeout);
    timerRef.current = [];

    if (!active) {
      setPhase("initial");
      return;
    }

    setPhase("initial");

    const schedule = (fn: () => void, ms: number) => {
      timerRef.current.push(setTimeout(fn, ms));
    };

    // Phase 1: show initial (2.5s)
    // Phase 2: highlight idle in red (3s)
    // Phase 3: collapse to sliced (5s)
    schedule(() => setPhase("highlight"), 2500);
    schedule(() => setPhase("sliced"), 5500);
    // Cycle back
    schedule(() => setPhase("initial"), 10500);

    const interval = setInterval(() => {
      setPhase("initial");
      schedule(() => setPhase("highlight"), 2500);
      schedule(() => setPhase("sliced"), 5500);
      schedule(() => setPhase("initial"), 10500);
    }, 11000);

    return () => {
      timerRef.current.forEach(clearTimeout);
      clearInterval(interval);
    };
  }, [active]);

  const isHighlight = phase === "highlight";
  const isSliced = phase === "sliced";

  return (
    <div
      style={{
        width: "100%",
        maxWidth: 900,
        margin: "0 auto",
        padding: "16px 16px",
      }}
    >
      {/* Header label */}
      <div
        style={{
          display: "flex",
          justifyContent: "center",
          alignItems: "center",
          marginBottom: 8,
        }}
      >
        <span
          style={{
            fontSize: 13,
            fontWeight: 600,
            color: isSliced
              ? "#28c840"
              : isHighlight
                ? "#f85149"
                : "var(--ifm-color-emphasis-600)",
            fontFamily: "system-ui, sans-serif",
            transition: "color 0.5s ease",
          }}
        >
          {isSliced
            ? "∞ Infinity — Time-Sliced"
            : isHighlight
              ? "Traditional — Wasted Resources"
              : "Traditional Agent Runtime"}
        </span>
      </div>

      <svg
        viewBox={`0 0 ${SVG_W} ${SVG_H}`}
        width="100%"
        style={{ overflow: "visible", display: "block" }}
      >
        {/* Time axis (at top) */}
        <line
          x1={MARGIN_LEFT}
          y1={12}
          x2={SVG_W - MARGIN_RIGHT}
          y2={12}
          stroke="var(--ifm-color-emphasis-400)"
          strokeWidth="1"
        />
        <text
          x={MARGIN_LEFT + 5}
          y={4}
          textAnchor="start"
          fill="var(--ifm-color-emphasis-600)"
          fontSize="11"
          fontFamily="system-ui, sans-serif"
        >
          time →
        </text>

        {/* Resource axis label */}
        <text
          x={12}
          y={TRAD_TOP + 60}
          textAnchor="middle"
          fill="var(--ifm-color-emphasis-600)"
          fontSize="11"
          fontFamily="system-ui, sans-serif"
          transform={`rotate(-90, 12, ${TRAD_TOP + 60})`}
        >
          ← compute × memory
        </text>

        {/* Traditional: idle segments (dashed outlines) */}
        {TIMELINES.map((timeline, agentIdx) => {
          const color = COLORS[agentIdx];
          const tradY = TRAD_TOP + agentIdx * TRAD_LANE_PITCH;
          return (
            <g key={`idle-${agentIdx}`}>
              {timeline
                .filter((seg) => !seg.active)
                .map((seg, segIdx) => {
                  const x = MARGIN_LEFT + (seg.start / 100) * TRACK_W;
                  const w = ((seg.end - seg.start) / 100) * TRACK_W;
                  return (
                    <rect
                      key={segIdx}
                      x={x}
                      y={tradY}
                      width={w}
                      height={LANE_H}
                      rx={4}
                      fill={isHighlight ? "rgba(248,81,73,0.15)" : "none"}
                      stroke={isHighlight ? "#f85149" : color}
                      strokeWidth="1.5"
                      strokeDasharray={isHighlight ? "0" : "4 3"}
                      opacity={isSliced ? 0 : isHighlight ? 0.8 : 0.3}
                      style={{
                        transition: "all 0.6s cubic-bezier(0.4, 0, 0.2, 1)",
                      }}
                    />
                  );
                })}
            </g>
          );
        })}

        {/* Active blocks — animate between traditional position and packed position */}
        {ACTIVE_BLOCKS.map((block, idx) => {
          const x = MARGIN_LEFT + (block.start / 100) * TRACK_W;
          const w = ((block.end - block.start) / 100) * TRACK_W;
          const tradY = TRAD_TOP + block.agentIdx * TRAD_LANE_PITCH;
          const slicedY = SLICED_TOP + block.packedRow * SLICED_LANE_PITCH;
          const offset = isSliced ? slicedY - tradY : 0;

          return (
            <rect
              key={idx}
              x={x}
              y={tradY}
              width={w}
              height={LANE_H}
              rx={4}
              fill={block.color}
              opacity={0.85}
              style={{
                transform: `translateY(${offset}px)`,
                transition: "transform 0.8s cubic-bezier(0.4, 0, 0.2, 1)",
              }}
            />
          );
        })}

        {/* Agent labels (traditional view) */}
        {AGENT_LABELS.map((label, agentIdx) => {
          const tradY = TRAD_TOP + agentIdx * TRAD_LANE_PITCH;
          return (
            <text
              key={agentIdx}
              x={SVG_W - MARGIN_RIGHT + 8}
              y={tradY + LANE_H / 2 + 1}
              textAnchor="start"
              dominantBaseline="central"
              fill={COLORS[agentIdx]}
              fontSize="11"
              fontWeight="500"
              fontFamily="system-ui, sans-serif"
              opacity={isSliced ? 0 : 1}
              style={{
                transition: "opacity 0.8s cubic-bezier(0.4, 0, 0.2, 1)",
              }}
            >
              {label}
            </text>
          );
        })}

        {/* Annotation for sliced view */}
        {isSliced && (
          <g>
            <line
              x1={SVG_W - MARGIN_RIGHT + 8}
              y1={SLICED_TOP}
              x2={SVG_W - MARGIN_RIGHT + 8}
              y2={SLICED_TOP + PACKED_ROW_COUNT * SLICED_LANE_PITCH}
              stroke="#28c840"
              strokeWidth="1.5"
              opacity={0.6}
            />
            <text
              x={SVG_W - MARGIN_RIGHT}
              y={SLICED_TOP + PACKED_ROW_COUNT * SLICED_LANE_PITCH + 24}
              textAnchor="end"
              fill="#28c840"
              fontSize="11"
              fontWeight="500"
              fontFamily="system-ui, sans-serif"
              opacity={0.8}
            >
              ↕ only active slices use compute
            </text>
          </g>
        )}
      </svg>
    </div>
  );
}
