import React, { useState, useEffect, useRef, useCallback } from "react";
import Layout from "@theme/Layout";
import AnimatedTerminal from "../components/AnimatedTerminal";
import RuntimeDiagram from "../components/RuntimeDiagram";
import DesktopMini from "../components/DesktopMini";
import FeatureCards from "../components/FeatureCards";
import ProtocolComparison from "../components/ProtocolComparison";
import SwarmSection from "../components/SwarmSection";

interface Tab {
  id: string;
  label: string;
  heading: string;
  description: string;
  link: string;
  linkLabel: string;
  duration: number; // ms for auto-rotation
}

const TABS: Tab[] = [
  {
    id: "code",
    label: "∞ Code",
    heading: "Infinity Code",
    description:
      "An AI coding agent for your terminal and desktop. Concurrent sessions, sandboxed execution, background agents, and live thread visualization — all powered by the Infinity Runtime.",
    link: "/docs/infinity-code/overview",
    linkLabel: "Get started →",
    duration: 25000,
  },
  {
    id: "runtime",
    label: "∞ Runtime",
    heading: "Infinity Runtime",
    description:
      "A principled agent runtime with first-class concurrency. Spawn threads, hibernate between tasks, and pay nothing while idle. Agents can run for years at near-zero cost.",
    link: "/docs/infinity-runtime/overview",
    linkLabel: "Learn more →",
    duration: 8000,
  },
  {
    id: "protocol",
    label: "∞ Protocol",
    heading: "Reactive Agent Protocol",
    description:
      "The tool protocol for agents that live in the real world. Subscribe to real-time events, execute operations that take hours, and hibernate with zero compute cost. MCP-compatible.",
    link: "/spec/overview",
    linkLabel: "Specification →",
    duration: 18000,
  },
];

function TabSwitcher({
  active,
  onSelect,
  progress,
}: {
  active: number;
  onSelect: (i: number) => void;
  progress: number;
}) {
  return (
    <div className="tab-switcher">
      {TABS.map((tab, i) => (
        <button
          key={tab.id}
          className={`tab-pill ${i === active ? "tab-pill--active" : ""}`}
          onClick={() => onSelect(i)}
          type="button"
        >
          {tab.label}
          {i === active && (
            <div
              className="tab-progress"
              style={{ transform: `scaleX(${progress})` }}
            />
          )}
        </button>
      ))}
    </div>
  );
}

function Showcase({ active }: { active: number }) {
  const tab = TABS[active];
  return (
    <div className="showcase">
      <div className="showcase-text" key={tab.id}>
        <h2>{tab.heading}</h2>
        <p>{tab.description}</p>
        <a href={tab.link} className="showcase-link">
          {tab.linkLabel}
        </a>
      </div>
      <div className="showcase-visual">
        <div
          className="showcase-pane"
          style={{ display: active === 0 ? "block" : "none" }}
        >
          <DesktopMini active={active === 0} />
        </div>
        <div
          className="showcase-pane"
          style={{ display: active === 1 ? "block" : "none" }}
        >
          <RuntimeDiagram active={active === 1} />
        </div>
        <div
          className="showcase-pane"
          style={{ display: active === 2 ? "block" : "none" }}
        >
          <AnimatedTerminal active={active === 2} />
        </div>
      </div>
    </div>
  );
}

export default function Home(): React.JSX.Element {
  const [active, setActive] = useState(0);
  const [progress, setProgress] = useState(0);
  const [locked, setLocked] = useState(false);
  const startRef = useRef(Date.now());
  const rafRef = useRef<number>(0);

  const selectTab = useCallback((i: number) => {
    setActive(i);
    setProgress(0);
    startRef.current = Date.now();
    setLocked(true);
  }, []);

  // Auto-rotation + progress bar
  useEffect(() => {
    if (locked) {
      // Unlock and advance to next tab after one full cycle
      const timer = setTimeout(() => {
        setLocked(false);
        setActive((prev) => (prev + 1) % TABS.length);
        setProgress(0);
        startRef.current = Date.now();
      }, TABS[active].duration);
      // Still animate progress while locked
      startRef.current = Date.now();
      function tick() {
        const elapsed = Date.now() - startRef.current;
        const p = Math.min(1, elapsed / TABS[active].duration);
        setProgress(p);
        if (p < 1) rafRef.current = requestAnimationFrame(tick);
      }
      rafRef.current = requestAnimationFrame(tick);
      return () => {
        clearTimeout(timer);
        cancelAnimationFrame(rafRef.current);
      };
    }

    startRef.current = Date.now();
    function tick() {
      const elapsed = Date.now() - startRef.current;
      const duration = TABS[active].duration;
      const p = Math.min(1, elapsed / duration);
      setProgress(p);
      if (p >= 1) {
        setActive((prev) => (prev + 1) % TABS.length);
        setProgress(0);
        startRef.current = Date.now();
      }
      rafRef.current = requestAnimationFrame(tick);
    }
    rafRef.current = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(rafRef.current);
  }, [active, locked]);

  return (
    <Layout
      title="Infinity"
      description="The open-source ecosystem for agents with principled concurrency."
    >
      <main className="landing-page">
        <section className="hero-section">
          <h1>
            <span className="gradient-text">Infinity</span>
          </h1>
          <p className="hero-tagline">
            The open-source ecosystem for agents with principled concurrency.
          </p>
          <div className="hero-buttons">
            <a href="/docs/what-is-rap" className="primary">
              Get Started
            </a>
            <a
              href="https://github.com/anthropics/infinity-agents"
              className="secondary"
            >
              GitHub →
            </a>
          </div>
          <TabSwitcher
            active={active}
            onSelect={selectTab}
            progress={progress}
          />
          <Showcase active={active} />
        </section>

        <FeatureCards />
        <ProtocolComparison />
        <SwarmSection />
      </main>
    </Layout>
  );
}
