import { useEffect, useRef, useState } from "react";
import type { SpinnerState } from "../types";
import css from "./Spinner.module.css";

const NUM_BARS = 10;
const BAR_W = 8;
const BAR_GAP = 2;
const BAR_H = 20;
const TOTAL_W = NUM_BARS * (BAR_W + BAR_GAP);

/* ── Animation design ──
 *
 * All motion is compositor-driven CSS animation (transform/opacity only),
 * not a requestAnimationFrame canvas loop. A rAF loop repaints on every
 * vsync (120Hz on fast displays), and each paint runs the full main-thread
 * paint → commit → raster pipeline plus re-renders every `backdrop-filter`
 * glass surface on the page — measured at ~50% of a core while the chat
 * view sat otherwise idle. Compositor animations run on the GPU at native
 * refresh rate with zero per-frame JS or main-thread paint.
 *
 * Reduced motion: every animated element's *base* styles depict the t=0
 * frame of its animation; the keyframes merely override them while the
 * animation runs. Under `prefers-reduced-motion: reduce`, theme.css
 * freezes/cancels the animations (and the e2e harness additionally
 * disables animations for screenshots), so elements fall back to those
 * base styles — a deterministic static t=0 render with no JS involved.
 *
 * The constants are derived from the terminal spinner animation
 * (crates/infinity-agent-cli/src/terminal.rs, render_thinking_bar) via its
 * original canvas port, so the visuals are unchanged.
 */

/* "thinking": hydro gradient #0096FF → #0FDBA2 and back (16 stops),
 * sampled at 0.5 stops per bar-width (= 20px per stop) and slid by 12
 * stops/s. One wavelength is 16 stops = 320px, so the slide loops every
 * 320/240 = 4/3s. */
const HYDRO: [number, number, number][] = [
  [0, 150, 255],
  [1, 158, 243],
  [3, 167, 231],
  [5, 175, 220],
  [7, 184, 208],
  [9, 193, 196],
  [11, 201, 185],
  [13, 210, 173],
  [15, 219, 162],
  [13, 210, 173],
  [11, 201, 185],
  [9, 193, 196],
  [7, 184, 208],
  [5, 175, 220],
  [3, 167, 231],
  [1, 158, 243],
];
const HYDRO_UNIT_PX = TOTAL_W / (NUM_BARS * 0.5);
const HYDRO_WAVELENGTH_PX = HYDRO.length * HYDRO_UNIT_PX;
const HYDRO_PERIOD_S = HYDRO_WAVELENGTH_PX / (12 * HYDRO_UNIT_PX);
const HYDRO_GRADIENT = `linear-gradient(90deg, ${[...HYDRO, HYDRO[0]]
  .map((c, i) => `rgb(${c[0]},${c[1]},${c[2]}) ${i * HYDRO_UNIT_PX}px`)
  .join(", ")})`;

/* "tool": bars breathing rgb(lerp(25,125,w), lerp(0,100,w), lerp(150,225,w))
 * with w = sin(2πt/3)·0.5+0.5. Crossfading a bright layer over a dim layer
 * reproduces the per-channel lerp exactly; w(0) = 0.5 ascending, hence the
 * -T/4 delay against sine keyframes that start at the trough. */
const TOOL_DIM = "rgb(25,0,150)";
const TOOL_BRIGHT = "rgb(125,100,225)";
const TOOL_PERIOD_S = 3;

/* "loading": bar i bounces with wᵢ(t) = sin(2πt/0.8 + 0.3i)·0.5+0.5,
 * height max(3px, wᵢ·20px) (scaleY 0.15–1), brightening from the dim WARM
 * palette end toward orange as it rises. */
const LOADING_PERIOD_S = 0.8;
const LOADING_BAR_DELAY_S = (0.3 * LOADING_PERIOD_S) / (2 * Math.PI);
const LOADING_DIM = "rgb(54,18,6)"; // WARM[0] rgb(180,60,20) at dim 0.3
const LOADING_BRIGHT = "rgb(255,160,60)";
const loadingWave = (i: number) =>
  Math.max(0.15, Math.sin(0.3 * i) * 0.5 + 0.5);

interface Props {
  state: SpinnerState;
}

export function Spinner({ state }: Props) {
  const ref = useRef<HTMLDivElement>(null);
  const [paused, setPaused] = useState(false);

  // Pause the animations while off-screen. The slide-away chat panel hides
  // with `transform: translateX(110%)` while staying mounted; without this
  // its spinner would keep producing compositor frames invisibly.
  useEffect(() => {
    const el = ref.current;
    if (!el || typeof IntersectionObserver === "undefined") return;
    const observer = new IntersectionObserver(([entry]) => {
      setPaused(!entry.isIntersecting);
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  const bars = (color: string, opacity?: number) =>
    Array.from({ length: NUM_BARS }, (_, i) => (
      <div key={i} className={css.bar} style={{ background: color, opacity }} />
    ));

  let content: React.ReactNode;
  if (state === "thinking") {
    content = (
      <div className={css.clip}>
        <div
          className={css.strip}
          style={{
            width: TOTAL_W + HYDRO_WAVELENGTH_PX,
            backgroundImage: HYDRO_GRADIENT,
            backgroundSize: `${HYDRO_WAVELENGTH_PX}px 100%`,
            animationDuration: `${HYDRO_PERIOD_S}s`,
            ["--slide-x" as string]: `${-HYDRO_WAVELENGTH_PX}px`,
          }}
        />
      </div>
    );
  } else if (state === "tool") {
    content = (
      <>
        <div className={css.row}>{bars(TOOL_DIM)}</div>
        <div
          className={`${css.row} ${css.breathe}`}
          style={{
            opacity: 0.5, // static t=0 frame (see header comment)
            animationDuration: `${TOOL_PERIOD_S}s`,
            animationDelay: `${-TOOL_PERIOD_S / 4}s`,
          }}
        >
          {bars(TOOL_BRIGHT)}
        </div>
      </>
    );
  } else {
    const delay = (i: number) =>
      `${-(LOADING_PERIOD_S / 4 + i * LOADING_BAR_DELAY_S)}s`;
    content = (
      <div className={css.row}>
        {Array.from({ length: NUM_BARS }, (_, i) => (
          <div
            key={i}
            className={`${css.bar} ${css.bounce}`}
            style={{
              background: LOADING_DIM,
              transform: `scaleY(${loadingWave(i)})`, // static t=0 frame
              animationDuration: `${LOADING_PERIOD_S}s`,
              animationDelay: delay(i),
            }}
          >
            <div
              className={css.fade}
              style={{
                background: LOADING_BRIGHT,
                opacity: loadingWave(i), // static t=0 frame
                animationDuration: `${LOADING_PERIOD_S}s`,
                animationDelay: delay(i),
              }}
            />
          </div>
        ))}
      </div>
    );
  }

  return (
    <div className={css.wrapper}>
      <div
        ref={ref}
        key={state}
        className={css.anim}
        style={{ width: TOTAL_W, height: BAR_H }}
        data-paused={paused || undefined}
      >
        {content}
      </div>
      <span className={css.label}>
        {state === "loading" && "Loading context…"}
        {state === "thinking" && "Thinking…"}
        {state === "tool" && "Running tool…"}
      </span>
    </div>
  );
}
