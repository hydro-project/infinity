import React, { useEffect, useRef, useState } from "react";

/**
 * MemoryChart: measured agents-per-memory curve for the local runtime.
 *
 * Data source: `crates/infinity-agent-core/examples/agent_scale.rs`, run with
 * AGENTS=80000 TURNS=20 WAVE=4000 on an AMD EPYC 9R14 (single thread,
 * in-memory stores, scripted in-process model). Each agent runs 20 synthetic
 * turns; every turn is two completion rounds and one asynchronous tool round
 * trip through the input queue. RSS is sampled after each wave of 4,000
 * agents finishes and goes idle.
 *
 * Plotted inverted (memory on x, agents on y): how many resident agents fit
 * in a given amount of memory.
 */

// (rss_bytes, agents) pairs from the benchmark run (AMD EPYC 9R14).
// Regenerate with the command in the component doc comment.
const DATA: [number, number][] = [
  [5304320, 0],
  [692879360, 4000],
  [1101406208, 8000],
  [1507291136, 12000],
  [1915736064, 16000],
  [2319831040, 20000],
  [2724171776, 24000],
  [3128098816, 28000],
  [3540946944, 32000],
  [3944587264, 36000],
  [4348772352, 40000],
  [4753293312, 44000],
  [5157015552, 48000],
  [5561012224, 52000],
  [5965852672, 56000],
  [6385418240, 60000],
  [6790180864, 64000],
  [7194882048, 68000],
  [7598186496, 72000],
  [8002379776, 76000],
  [8406446080, 80000],
];

const GB = 1e9;

// Layout
const W = 720;
const H = 400;
const MARGIN = { top: 24, right: 96, bottom: 52, left: 76 };
const PLOT_W = W - MARGIN.left - MARGIN.right;
const PLOT_H = H - MARGIN.top - MARGIN.bottom;

const X_MAX_GB = 9.6;
const Y_MAX = 84000;

const C_LINE = "var(--ifm-color-primary)";
const C_AXIS = "var(--ifm-color-emphasis-400)";
const C_GRID = "var(--ifm-color-emphasis-200)";
const C_LABEL = "var(--ifm-color-emphasis-600)";
const C_TEXT = "var(--ifm-color-emphasis-800)";

function x(gb: number): number {
  return MARGIN.left + (gb / X_MAX_GB) * PLOT_W;
}

function y(agents: number): number {
  return MARGIN.top + PLOT_H - (agents / Y_MAX) * PLOT_H;
}

export default function MemoryChart({
  active,
}: {
  active: boolean;
}): React.JSX.Element {
  // Draw the line once when the chart scrolls into view.
  const [drawn, setDrawn] = useState(false);
  const pathRef = useRef<SVGPathElement | null>(null);
  useEffect(() => {
    if (active) setDrawn(true);
  }, [active]);

  const points = DATA.map(([bytes, agents]) => [x(bytes / GB), y(agents)]);
  const path = points
    .map(
      ([px, py], i) =>
        `${i === 0 ? "M" : "L"} ${px.toFixed(1)} ${py.toFixed(1)}`,
    )
    .join(" ");
  const [endX, endY] = points[points.length - 1];
  const lastAgents = DATA[DATA.length - 1][1];
  const lastGb = DATA[DATA.length - 1][0] / GB;

  // Per-agent cost from the overall slope (excluding the empty-system base).
  const perAgentKb =
    (DATA[DATA.length - 1][0] - DATA[0][0]) / lastAgents / 1024;

  const yTicks = [0, 20000, 40000, 60000, 80000];
  const xTicks = [0, 2, 4, 6, 8];

  return (
    <figure style={{ width: "100%", maxWidth: 760, margin: "0 auto" }}>
      <svg
        viewBox={`0 0 ${W} ${H}`}
        width="100%"
        style={{ display: "block" }}
        role="img"
        aria-label={`Measured memory usage: ${lastAgents.toLocaleString()} idle agents in ${lastGb.toFixed(1)} gigabytes of RAM, about ${Math.round(perAgentKb)} kilobytes per agent`}
      >
        {/* Horizontal gridlines */}
        {yTicks.slice(1).map((t) => (
          <line
            key={t}
            x1={MARGIN.left}
            y1={y(t)}
            x2={MARGIN.left + PLOT_W}
            y2={y(t)}
            stroke={C_GRID}
            strokeWidth="1"
          />
        ))}

        {/* Axes */}
        <line
          x1={MARGIN.left}
          y1={MARGIN.top}
          x2={MARGIN.left}
          y2={MARGIN.top + PLOT_H}
          stroke={C_AXIS}
          strokeWidth="1"
        />
        <line
          x1={MARGIN.left}
          y1={MARGIN.top + PLOT_H}
          x2={MARGIN.left + PLOT_W}
          y2={MARGIN.top + PLOT_H}
          stroke={C_AXIS}
          strokeWidth="1"
        />

        {/* Y tick labels */}
        {yTicks.map((t) => (
          <text
            key={t}
            x={MARGIN.left - 8}
            y={y(t)}
            textAnchor="end"
            dominantBaseline="central"
            fill={C_LABEL}
            fontSize="12"
            fontFamily="system-ui, sans-serif"
          >
            {t === 0 ? "0" : `${t / 1000}k`}
          </text>
        ))}
        <text
          x={MARGIN.left - 52}
          y={MARGIN.top + PLOT_H / 2}
          textAnchor="middle"
          fill={C_TEXT}
          fontSize="12"
          fontWeight="600"
          fontFamily="system-ui, sans-serif"
          transform={`rotate(-90, ${MARGIN.left - 52}, ${MARGIN.top + PLOT_H / 2})`}
        >
          resident agents
        </text>

        {/* X tick labels */}
        {xTicks.map((t) => (
          <text
            key={t}
            x={x(t)}
            y={MARGIN.top + PLOT_H + 20}
            textAnchor="middle"
            fill={C_LABEL}
            fontSize="12"
            fontFamily="system-ui, sans-serif"
          >
            {t} GB
          </text>
        ))}
        <text
          x={MARGIN.left + PLOT_W / 2}
          y={MARGIN.top + PLOT_H + 42}
          textAnchor="middle"
          fill={C_TEXT}
          fontSize="12"
          fontWeight="600"
          fontFamily="system-ui, sans-serif"
        >
          process memory (RSS)
        </text>

        {/* Measured line */}
        <path
          ref={pathRef}
          d={path}
          fill="none"
          stroke={C_LINE}
          strokeWidth="2.5"
          strokeLinejoin="round"
          pathLength={1}
          strokeDasharray={1}
          strokeDashoffset={drawn ? 0 : 1}
          style={{ transition: "stroke-dashoffset 1.6s ease-out" }}
        />

        {/* Raspberry Pi reference */}
        <line
          x1={x(8)}
          y1={MARGIN.top}
          x2={x(8)}
          y2={MARGIN.top + PLOT_H}
          stroke={C_AXIS}
          strokeWidth="1"
          strokeDasharray="5 4"
        />
        <text
          x={x(8) + 16}
          y={MARGIN.top + PLOT_H / 2}
          textAnchor="middle"
          fill={C_LABEL}
          fontSize="12"
          fontFamily="system-ui, sans-serif"
          transform={`rotate(-90, ${x(8) + 16}, ${MARGIN.top + PLOT_H / 2})`}
        >
          Raspberry Pi 5 (8 GB)
        </text>

        {/* Endpoint annotation */}
        <text
          x={endX - 10}
          y={endY + 4}
          textAnchor="end"
          fill={C_TEXT}
          fontSize="13"
          fontWeight="600"
          fontFamily="system-ui, sans-serif"
          opacity={drawn ? 1 : 0}
          style={{ transition: "opacity 0.5s ease 1.4s" }}
        >
          {lastAgents.toLocaleString()} agents in {lastGb.toFixed(1)} GB
        </text>

        {/* Slope annotation */}
        <text
          x={x(4.6)}
          y={y(22000)}
          textAnchor="start"
          fill={C_LABEL}
          fontSize="12"
          fontFamily="system-ui, sans-serif"
          opacity={drawn ? 1 : 0}
          style={{ transition: "opacity 0.5s ease 1s" }}
        >
          ≈ {Math.round(perAgentKb)} KB per agent
        </text>
      </svg>
      <figcaption
        style={{
          fontSize: "0.8rem",
          color: "var(--ifm-color-emphasis-600)",
          textAlign: "center",
          marginTop: "0.5rem",
          lineHeight: 1.5,
        }}
      >
        Measured: idle resident agents after 20 tool-calling turns each
        (3,200,000 completions total), single thread, in-memory stores.{" "}
        <a href="https://github.com/hydro-project/infinity/blob/main/crates/infinity-agent-core/examples/agent_scale.rs">
          Benchmark source
        </a>
      </figcaption>
    </figure>
  );
}
