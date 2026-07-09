/* ── Inline SVG icons ──
   Small stroke icons (lucide-style paths) used instead of emoji / symbol
   glyphs, whose rendering depends on the platform's installed fonts (e.g.
   hosts without a color-emoji font drop 📌 entirely). SVGs render
   identically everywhere, which screenshot-based e2e tests rely on. */

import type { ReactNode } from "react";

interface IconProps {
  size?: number;
}

function Svg({ size = 16, children }: IconProps & { children: ReactNode }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
    >
      {children}
    </svg>
  );
}

/** Pushpin (sidebar / chat panel pin toggles). */
export function PinIcon({ size }: IconProps) {
  return (
    <Svg size={size}>
      <path d="M12 17v5" />
      <path d="M9 10.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V16a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V6h1a2 2 0 0 0 0-4H8a2 2 0 0 0 0 4h1z" />
    </Svg>
  );
}

/** Sun (light theme). */
export function SunIcon({ size }: IconProps) {
  return (
    <Svg size={size}>
      <circle cx="12" cy="12" r="4" />
      <path d="M12 2v2" />
      <path d="M12 20v2" />
      <path d="m4.93 4.93 1.41 1.41" />
      <path d="m17.66 17.66 1.41 1.41" />
      <path d="M2 12h2" />
      <path d="M20 12h2" />
      <path d="m6.34 17.66-1.41 1.41" />
      <path d="m19.07 4.93-1.41 1.41" />
    </Svg>
  );
}

/** Moon (dark theme). */
export function MoonIcon({ size }: IconProps) {
  return (
    <Svg size={size}>
      <path d="M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9Z" />
    </Svg>
  );
}

/** Monitor (system theme). */
export function MonitorIcon({ size }: IconProps) {
  return (
    <Svg size={size}>
      <rect width="20" height="14" x="2" y="3" rx="2" />
      <line x1="8" x2="16" y1="21" y2="21" />
      <line x1="12" x2="12" y1="17" y2="21" />
    </Svg>
  );
}

/** Overlapping squares (copy to clipboard). */
export function CopyIcon({ size }: IconProps) {
  return (
    <Svg size={size}>
      <rect width="14" height="14" x="8" y="8" rx="2" ry="2" />
      <path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2" />
    </Svg>
  );
}

/** Checkmark (copy confirmation). */
export function CheckIcon({ size }: IconProps) {
  return (
    <Svg size={size}>
      <path d="M20 6 9 17l-5-5" />
    </Svg>
  );
}

/** Chevron pointing right (collapsed section). */
export function ChevronRightIcon({ size }: IconProps) {
  return (
    <Svg size={size}>
      <path d="m9 18 6-6-6-6" />
    </Svg>
  );
}

/** Chevron pointing down (expanded section). */
export function ChevronDownIcon({ size }: IconProps) {
  return (
    <Svg size={size}>
      <path d="m6 9 6 6 6-6" />
    </Svg>
  );
}
