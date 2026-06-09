import React, { useEffect, useState, useRef, useCallback } from "react";

/**
 * Animated protocol diagram showing RAP's fire-and-forget async model.
 * Two swim lanes (Agent Runtime / RAP Tool) with time flowing left to right.
 * Elements are revealed by a clip rect that tracks the playhead position.
 */

const CYCLE_MS = 12000;

// Layout constants
const W = 780;
const H = 280;
const LANE_Y_RUNTIME = 40;
const LANE_Y_TOOL = 150;
const LANE_H = 36;
const LANE_LABEL_X = 12;
const TIMELINE_START = 130;
const TIMELINE_END = 750;
const WEBHOOK_Y = 270; // external event source below tool lane

// Colors — use CSS variables for theme compatibility
const C_PRIMARY = "var(--ifm-color-primary)";
const C_SUCCESS = "#28c840";
const C_EVENT = "#d29922";
const C_DIM = "var(--ifm-color-emphasis-300)";
const C_LABEL = "var(--ifm-color-emphasis-600)";
const C_ACTIVE_RUNTIME = "var(--ifm-color-primary)";

// Timeline phases (x-positions for key moments)
const T = {
  llmStart: 145,
  llmEnd: 230,
  // Subscribe call
  subSent: 230,
  toolSubStart: 260,
  toolSubEnd: 320,
  // Runtime idle while tool registers
  runtimeIdle1Start: 230,
  runtimeIdle1End: 340,
  // Tool sends "subscribed" result back
  subscribedSent: 320,
  // Runtime processes result, then idles again
  llm2Start: 340,
  llm2End: 390,
  runtimeIdle2Start: 390,
  runtimeIdle2End: 615,
  // Tool idle after sending subscribed
  toolIdleStart: 320,
  toolIdleEnd: 530,
  // Webhook arrives directly into tool (diagonal, no gap)
  webhookArrives: 530,
  toolWakeStart: 530,
  toolWakeEnd: 590,
  // Tool idle after processing webhook
  toolIdle2Start: 590,
  toolIdle2End: 800,
  // Tool sends event to runtime
  eventSent: 590,
  // Runtime wakes
  llm3Start: 615,
  llm3End: 800,
};

export default function ProtocolDiagram({
  active,
}: {
  active: boolean;
}): React.JSX.Element {
  const [progress, setProgress] = useState(0);
  const startRef = useRef(0);
  const rafRef = useRef(0);

  const stop = useCallback(() => {
    cancelAnimationFrame(rafRef.current);
  }, []);

  const runCycle = useCallback(() => {
    startRef.current = Date.now();
    function tick() {
      const p = Math.min(1, (Date.now() - startRef.current) / CYCLE_MS);
      setProgress(p);
      if (p < 1) rafRef.current = requestAnimationFrame(tick);
    }
    rafRef.current = requestAnimationFrame(tick);
  }, []);

  useEffect(() => {
    if (!active) {
      stop();
      setProgress(0);
      return;
    }
    runCycle();
    return stop;
  }, [active, runCycle, stop]);

  const playheadX = TIMELINE_START + progress * (W - TIMELINE_START);

  return (
    <div style={{ width: "100%", maxWidth: 1100, margin: "0 auto" }}>
      <svg
        viewBox={`0 0 ${W} ${H}`}
        width="100%"
        style={{ display: "block" }}
        role="img"
        aria-label="Animated diagram showing RAP protocol fire-and-forget async flow"
      >
        <defs>
          <marker
            id="arrow-down"
            markerWidth="8"
            markerHeight="6"
            refX="4"
            refY="3"
            orient="auto"
          >
            <path d="M0,0 L8,3 L0,6" fill={C_PRIMARY} />
          </marker>
          <marker
            id="arrow-up"
            markerWidth="8"
            markerHeight="6"
            refX="4"
            refY="3"
            orient="auto"
          >
            <path d="M0,0 L8,3 L0,6" fill={C_SUCCESS} />
          </marker>
          <marker
            id="arrow-event"
            markerWidth="8"
            markerHeight="6"
            refX="4"
            refY="3"
            orient="auto"
          >
            <path d="M0,0 L8,3 L0,6" fill={C_EVENT} />
          </marker>
          {/* Mask with soft fade at the reveal edge */}
          <linearGradient
            id="reveal-fade"
            gradientUnits="userSpaceOnUse"
            x1={playheadX - 15}
            y1="0"
            x2={playheadX}
            y2="0"
          >
            <stop offset="0" stopColor="white" stopOpacity="1" />
            <stop offset="1" stopColor="white" stopOpacity="0" />
          </linearGradient>
          <mask id="reveal-mask">
            <rect
              x="0"
              y="0"
              width={Math.max(0, playheadX - 15)}
              height={H}
              fill="white"
            />
            <rect
              x={Math.max(0, playheadX - 15)}
              y="0"
              width="15"
              height={H}
              fill="url(#reveal-fade)"
            />
          </mask>
        </defs>

        {/* ─── Always visible: lane labels, baselines, time axis ─── */}
        <text
          x={LANE_LABEL_X}
          y={LANE_Y_RUNTIME + LANE_H / 2}
          dominantBaseline="central"
          fill={C_LABEL}
          fontSize="11"
          fontWeight="600"
          fontFamily="system-ui, sans-serif"
        >
          Agent Runtime
        </text>
        <text
          x={LANE_LABEL_X}
          y={LANE_Y_TOOL + LANE_H / 2}
          dominantBaseline="central"
          fill={C_LABEL}
          fontSize="11"
          fontWeight="600"
          fontFamily="system-ui, sans-serif"
        >
          RAP Tool
        </text>
        <text
          x={LANE_LABEL_X}
          y={WEBHOOK_Y}
          dominantBaseline="central"
          fill={C_LABEL}
          fontSize="10"
          fontWeight="500"
          fontFamily="system-ui, sans-serif"
          opacity="0.7"
        >
          External
        </text>
        <line
          x1={TIMELINE_START}
          y1={LANE_Y_RUNTIME + LANE_H / 2}
          x2={TIMELINE_END}
          y2={LANE_Y_RUNTIME + LANE_H / 2}
          stroke={C_DIM}
          strokeWidth="1"
          strokeDasharray="4 4"
          opacity="0.4"
        />
        <line
          x1={TIMELINE_START}
          y1={LANE_Y_TOOL + LANE_H / 2}
          x2={TIMELINE_END}
          y2={LANE_Y_TOOL + LANE_H / 2}
          stroke={C_DIM}
          strokeWidth="1"
          strokeDasharray="4 4"
          opacity="0.4"
        />
        <line
          x1={TIMELINE_START}
          y1={H - 20}
          x2={TIMELINE_END - 10}
          y2={H - 20}
          stroke={C_DIM}
          strokeWidth="1"
          markerEnd="url(#arrow-down)"
        />
        <text
          x={(TIMELINE_START + TIMELINE_END) / 2}
          y={H - 8}
          textAnchor="middle"
          fill={C_DIM}
          fontSize="9"
          fontFamily="system-ui, sans-serif"
        >
          time →
        </text>

        {/* ─── Masked group: revealed by playhead with soft edge ─── */}
        <g mask="url(#reveal-mask)">
          {/* Runtime: LLM decides to subscribe */}
          <rect
            x={T.llmStart}
            y={LANE_Y_RUNTIME}
            width={T.llmEnd - T.llmStart}
            height={LANE_H}
            rx={4}
            fill={C_ACTIVE_RUNTIME}
            opacity={0.15}
            stroke={C_ACTIVE_RUNTIME}
            strokeWidth="1.5"
          />
          <text
            x={(T.llmStart + T.llmEnd) / 2}
            y={LANE_Y_RUNTIME + LANE_H / 2 + 1}
            textAnchor="middle"
            dominantBaseline="central"
            fill={C_PRIMARY}
            fontSize="9"
            fontWeight="500"
            fontFamily="system-ui, sans-serif"
          >
            LLM
          </text>

          {/* Subscribe arrow (diagonal down-right) */}
          <line
            x1={T.subSent}
            y1={LANE_Y_RUNTIME + LANE_H}
            x2={T.toolSubStart}
            y2={LANE_Y_TOOL}
            stroke={C_PRIMARY}
            strokeWidth="1.5"
            markerEnd="url(#arrow-down)"
          />
          <text
            x={(T.subSent + T.toolSubStart) / 2 + 8}
            y={(LANE_Y_RUNTIME + LANE_H + LANE_Y_TOOL) / 2 - 4}
            fill={C_PRIMARY}
            fontSize="8"
            fontFamily="system-ui, sans-serif"
            dominantBaseline="central"
          >
            subscribe
          </text>

          {/* Tool runs briefly to register subscription */}
          <rect
            x={T.toolSubStart}
            y={LANE_Y_TOOL}
            width={T.toolSubEnd - T.toolSubStart}
            height={LANE_H}
            rx={4}
            fill={C_SUCCESS}
            opacity={0.15}
            stroke={C_SUCCESS}
            strokeWidth="1.5"
          />
          <text
            x={(T.toolSubStart + T.toolSubEnd) / 2}
            y={LANE_Y_TOOL + LANE_H / 2 + 1}
            textAnchor="middle"
            dominantBaseline="central"
            fill={C_SUCCESS}
            fontSize="9"
            fontWeight="500"
            fontFamily="system-ui, sans-serif"
          >
            register
          </text>

          {/* Tool sends "subscribed" result back (diagonal up-right) */}
          <line
            x1={T.subscribedSent}
            y1={LANE_Y_TOOL}
            x2={T.llm2Start}
            y2={LANE_Y_RUNTIME + LANE_H}
            stroke={C_SUCCESS}
            strokeWidth="1.5"
            markerEnd="url(#arrow-up)"
          />
          <text
            x={(T.subscribedSent + T.llm2Start) / 2 + 8}
            y={(LANE_Y_RUNTIME + LANE_H + LANE_Y_TOOL) / 2 + 4}
            fill={C_SUCCESS}
            fontSize="8"
            fontFamily="system-ui, sans-serif"
            dominantBaseline="central"
          >
            subscribed
          </text>

          {/* Runtime processes result */}
          <rect
            x={T.llm2Start}
            y={LANE_Y_RUNTIME}
            width={T.llm2End - T.llm2Start}
            height={LANE_H}
            rx={4}
            fill={C_ACTIVE_RUNTIME}
            opacity={0.15}
            stroke={C_ACTIVE_RUNTIME}
            strokeWidth="1.5"
          />
          <text
            x={(T.llm2Start + T.llm2End) / 2}
            y={LANE_Y_RUNTIME + LANE_H / 2 + 1}
            textAnchor="middle"
            dominantBaseline="central"
            fill={C_PRIMARY}
            fontSize="9"
            fontWeight="500"
            fontFamily="system-ui, sans-serif"
          >
            LLM
          </text>

          {/* Runtime idle while tool registers */}
          <rect
            x={T.runtimeIdle1Start}
            y={LANE_Y_RUNTIME}
            width={T.runtimeIdle1End - T.runtimeIdle1Start}
            height={LANE_H}
            rx={4}
            fill="none"
            stroke="var(--ifm-color-emphasis-500)"
            strokeWidth="1.5"
            strokeDasharray="4 3"
            opacity="0.8"
          />
          <text
            x={(T.runtimeIdle1Start + T.runtimeIdle1End) / 2}
            y={LANE_Y_RUNTIME + LANE_H / 2 + 1}
            textAnchor="middle"
            dominantBaseline="central"
            fill="var(--ifm-color-emphasis-700)"
            fontSize="9"
            fontFamily="system-ui, sans-serif"
          >
            💤
          </text>

          {/* Runtime idles after processing */}
          <rect
            x={T.runtimeIdle2Start}
            y={LANE_Y_RUNTIME}
            width={T.runtimeIdle2End - T.runtimeIdle2Start}
            height={LANE_H}
            rx={4}
            fill="none"
            stroke="var(--ifm-color-emphasis-500)"
            strokeWidth="1.5"
            strokeDasharray="4 3"
            opacity="0.8"
          />
          <text
            x={(T.runtimeIdle2Start + T.runtimeIdle2End) / 2}
            y={LANE_Y_RUNTIME + LANE_H / 2 + 1}
            textAnchor="middle"
            dominantBaseline="central"
            fill="var(--ifm-color-emphasis-700)"
            fontSize="9"
            fontFamily="system-ui, sans-serif"
          >
            💤
          </text>

          {/* Tool idle */}
          <rect
            x={T.toolIdleStart}
            y={LANE_Y_TOOL}
            width={T.toolIdleEnd - T.toolIdleStart}
            height={LANE_H}
            rx={4}
            fill="none"
            stroke="var(--ifm-color-emphasis-500)"
            strokeWidth="1.5"
            strokeDasharray="4 3"
            opacity="0.8"
          />
          <text
            x={(T.toolIdleStart + T.toolIdleEnd) / 2}
            y={LANE_Y_TOOL + LANE_H / 2 + 1}
            textAnchor="middle"
            dominantBaseline="central"
            fill="var(--ifm-color-emphasis-700)"
            fontSize="9"
            fontFamily="system-ui, sans-serif"
          >
            💤
          </text>

          {/* Webhook arrives from external (diagonal up into tool start) */}
          <line
            x1={T.webhookArrives - 20}
            y1={WEBHOOK_Y}
            x2={T.toolWakeStart}
            y2={LANE_Y_TOOL + LANE_H}
            stroke={C_EVENT}
            strokeWidth="1.5"
            markerEnd="url(#arrow-event)"
          />
          <text
            x={T.webhookArrives - 5}
            y={(LANE_Y_TOOL + LANE_H + WEBHOOK_Y) / 2 + 4}
            fill={C_EVENT}
            fontSize="8"
            fontFamily="system-ui, sans-serif"
            dominantBaseline="central"
          >
            webhook
          </text>

          {/* Tool wakes to process */}
          <rect
            x={T.toolWakeStart}
            y={LANE_Y_TOOL}
            width={T.toolWakeEnd - T.toolWakeStart}
            height={LANE_H}
            rx={4}
            fill={C_EVENT}
            opacity={0.15}
            stroke={C_EVENT}
            strokeWidth="1.5"
          />
          <text
            x={(T.toolWakeStart + T.toolWakeEnd) / 2}
            y={LANE_Y_TOOL + LANE_H / 2 + 1}
            textAnchor="middle"
            dominantBaseline="central"
            fill={C_EVENT}
            fontSize="9"
            fontWeight="500"
            fontFamily="system-ui, sans-serif"
          >
            process
          </text>

          {/* Tool idle after processing */}
          <rect
            x={T.toolIdle2Start}
            y={LANE_Y_TOOL}
            width={T.toolIdle2End - T.toolIdle2Start}
            height={LANE_H}
            rx={4}
            fill="none"
            stroke="var(--ifm-color-emphasis-500)"
            strokeWidth="1.5"
            strokeDasharray="4 3"
            opacity="0.8"
          />
          <text
            x={(T.toolIdle2Start + T.toolIdle2End) / 2}
            y={LANE_Y_TOOL + LANE_H / 2 + 1}
            textAnchor="middle"
            dominantBaseline="central"
            fill="var(--ifm-color-emphasis-700)"
            fontSize="9"
            fontFamily="system-ui, sans-serif"
          >
            💤
          </text>

          {/* Tool sends event to runtime (diagonal up-right) */}
          <line
            x1={T.eventSent}
            y1={LANE_Y_TOOL}
            x2={T.llm3Start}
            y2={LANE_Y_RUNTIME + LANE_H}
            stroke={C_EVENT}
            strokeWidth="1.5"
            markerEnd="url(#arrow-event)"
          />
          <text
            x={(T.eventSent + T.llm3Start) / 2 + 8}
            y={(LANE_Y_RUNTIME + LANE_H + LANE_Y_TOOL) / 2 + 4}
            fill={C_EVENT}
            fontSize="8"
            fontFamily="system-ui, sans-serif"
            dominantBaseline="central"
          >
            event
          </text>

          {/* Runtime wakes to handle event */}
          <rect
            x={T.llm3Start}
            y={LANE_Y_RUNTIME}
            width={T.llm3End - T.llm3Start}
            height={LANE_H}
            rx={4}
            fill={C_ACTIVE_RUNTIME}
            opacity={0.15}
            stroke={C_ACTIVE_RUNTIME}
            strokeWidth="1.5"
          />
          <text
            x={(T.llm3Start + T.llm3End) / 2}
            y={LANE_Y_RUNTIME + LANE_H / 2 + 1}
            textAnchor="middle"
            dominantBaseline="central"
            fill={C_PRIMARY}
            fontSize="9"
            fontWeight="500"
            fontFamily="system-ui, sans-serif"
          >
            LLM
          </text>
        </g>

        {/* ─── Playhead (outside clip) ─── */}
        {active && (
          <rect
            x={playheadX}
            y={LANE_Y_RUNTIME - 15}
            width="2"
            height={WEBHOOK_Y - LANE_Y_RUNTIME + 30}
            fill={C_PRIMARY}
            opacity={
              progress > 0.9 ? 0.25 * (1 - (progress - 0.9) / 0.1) : 0.25
            }
            rx={1}
          />
        )}
      </svg>
    </div>
  );
}
