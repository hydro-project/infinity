import { useEffect, useRef } from "react";
import type { SpinnerState } from "../types";
import css from "./Spinner.module.css";

const NUM_BARS = 10;

/* ── Color palettes matching the terminal ── */

// Hydro gradient: #0096FF → #0FDBA2 and back (16 stops)
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

const WARM: [number, number, number][] = [
  [180, 60, 20],
  [210, 80, 25],
  [240, 120, 40],
  [255, 160, 60],
  [255, 180, 80],
  [255, 160, 60],
  [240, 120, 40],
  [210, 80, 25],
];

function lerp(a: number, b: number, t: number) {
  return a + (b - a) * t;
}

interface Props {
  state: SpinnerState;
}

export function Spinner({ state }: Props) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const rafRef = useRef<number>(0);
  const startRef = useRef(performance.now());

  // Reset timer when state changes
  useEffect(() => {
    startRef.current = performance.now();
  }, [state]);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d")!;
    const barW = 8;
    const gap = 2;
    const h = 20;
    canvas.width = NUM_BARS * (barW + gap);
    canvas.height = h;

    function draw() {
      const elapsed = (performance.now() - startRef.current) / 1000;
      ctx.clearRect(0, 0, canvas!.width, canvas!.height);

      for (let i = 0; i < NUM_BARS; i++) {
        const x = i * (barW + gap);
        let r: number, g: number, b: number;
        let barH = h;

        if (state === "loading") {
          // Orange/red bouncing bars
          const phase = (elapsed * Math.PI * 2) / 0.8 + i * 0.3;
          const wave = Math.sin(phase) * 0.5 + 0.5;
          barH = Math.max(3, wave * h);
          const ci = Math.min(
            Math.floor(wave * (WARM.length - 1)),
            WARM.length - 1,
          );
          const dim = 0.3 + 0.7 * wave;
          r = WARM[ci][0] * dim;
          g = WARM[ci][1] * dim;
          b = WARM[ci][2] * dim;
        } else if (state === "thinking") {
          // Continuous sliding hydro gradient matching terminal density
          const totalW = NUM_BARS * (barW + gap);
          const grad = ctx.createLinearGradient(0, 0, totalW, 0);
          const len = HYDRO.length;
          const offset = elapsed * 12;
          // Sample at same density as old bars: 0.5 per bar-width
          const steps = 20;
          for (let s = 0; s <= steps; s++) {
            const t = s / steps;
            const pos = t * NUM_BARS * 0.5 + offset;
            const p = ((pos % len) + len) % len;
            const ia = Math.floor(p) % len;
            const ib = (ia + 1) % len;
            const frac = p - Math.floor(p);
            const cr = lerp(HYDRO[ia][0], HYDRO[ib][0], frac);
            const cg = lerp(HYDRO[ia][1], HYDRO[ib][1], frac);
            const cb = lerp(HYDRO[ia][2], HYDRO[ib][2], frac);
            grad.addColorStop(
              t,
              `rgb(${Math.round(cr)},${Math.round(cg)},${Math.round(cb)})`,
            );
          }
          ctx.fillStyle = grad;
          ctx.beginPath();
          ctx.roundRect(0, 0, totalW, h, 4);
          ctx.fill();
          rafRef.current = requestAnimationFrame(draw);
          return;
        } else {
          // tool: slow breathing blue
          const phase = (elapsed * Math.PI * 2) / 3;
          const wave = Math.sin(phase) * 0.5 + 0.5;
          r = lerp(25, 125, wave);
          g = lerp(0, 100, wave);
          b = lerp(150, 225, wave);
        }

        const y = h - barH;
        ctx.fillStyle = `rgb(${Math.round(r)},${Math.round(g)},${Math.round(b)})`;
        ctx.beginPath();
        ctx.roundRect(x, y, barW, barH, 2);
        ctx.fill();
      }

      rafRef.current = requestAnimationFrame(draw);
    }

    rafRef.current = requestAnimationFrame(draw);
    return () => cancelAnimationFrame(rafRef.current);
  }, [state]);

  return (
    <div className={css.wrapper}>
      <canvas ref={canvasRef} className={css.canvas} />
      <span className={css.label}>
        {state === "loading" && "Loading context…"}
        {state === "thinking" && "Thinking…"}
        {state === "tool" && "Running tool…"}
      </span>
    </div>
  );
}
