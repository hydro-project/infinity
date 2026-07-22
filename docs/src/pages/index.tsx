import React, { useState, useEffect, useRef } from "react";
import Layout from "@theme/Layout";
import ProtocolDiagram from "../components/ProtocolDiagram";
import RuntimeDiagram from "../components/RuntimeDiagram";
import DesktopMini from "../components/DesktopMini";

/** Tracks whether an element is in the viewport, so diagrams animate on scroll. */
function useInView(
  threshold = 0.25,
): [React.RefObject<HTMLElement | null>, boolean] {
  const ref = useRef<HTMLElement | null>(null);
  const [inView, setInView] = useState(false);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const obs = new IntersectionObserver(
      ([entry]) => setInView(entry.isIntersecting),
      { threshold },
    );
    obs.observe(el);
    return () => obs.disconnect();
  }, [threshold]);
  return [ref, inView];
}

function LayerSection({
  id,
  name,
  subtitle,
  paragraphs,
  link,
  linkLabel,
  children,
}: {
  id: string;
  name: string;
  subtitle: string;
  paragraphs: string[];
  link: string;
  linkLabel: string;
  children: (active: boolean) => React.ReactNode;
}) {
  const [ref, inView] = useInView();
  return (
    <section className="layer-section" id={id} ref={ref}>
      <div className="layer-inner">
        <h2>{name}</h2>
        <p className="layer-subtitle">{subtitle}</p>
        <div className="layer-visual">{children(inView)}</div>
        <div className="layer-paragraphs">
          {paragraphs.map((text, i) => (
            <p key={i}>{text}</p>
          ))}
          <a href={link} className="showcase-link">
            {linkLabel}
          </a>
        </div>
      </div>
    </section>
  );
}

export default function Home(): React.JSX.Element {
  return (
    <Layout
      title="Infinity"
      description="The open-source ecosystem for agents with principled concurrency."
    >
      <main className="landing-page">
        <section className="hero-section">
          <h1>Infinity</h1>
          <p className="hero-tagline">
            The open-source ecosystem for agents with principled concurrency.
          </p>
          <div className="hero-buttons">
            <a href="/docs/infinity-runtime/overview" className="primary">
              Get Started
            </a>
            <a
              href="https://github.com/hydro-project/infinity"
              className="secondary"
            >
              GitHub →
            </a>
          </div>
          <div className="hero-layers">
            <a href="#runtime" className="hero-layer-card">
              <span className="hero-layer-name">Infinity Runtime</span>
              <span className="hero-layer-desc">
                The first serverless-native agent runtime
              </span>
              <span className="hero-layer-arrow" aria-hidden="true">
                ↓
              </span>
            </a>
            <a href="#protocol" className="hero-layer-card">
              <span className="hero-layer-name">Reactive Agent Protocol</span>
              <span className="hero-layer-desc">
                An async, event-driven successor to MCP
              </span>
              <span className="hero-layer-arrow" aria-hidden="true">
                ↓
              </span>
            </a>
            <a href="#code" className="hero-layer-card">
              <span className="hero-layer-name">Infinity Code</span>
              <span className="hero-layer-desc">
                A highly concurrent coding agent
              </span>
              <span className="hero-layer-arrow" aria-hidden="true">
                ↓
              </span>
            </a>
          </div>
        </section>

        <LayerSection
          id="runtime"
          name="Infinity Runtime"
          subtitle="The first serverless-native agent runtime"
          paragraphs={[
            "The Infinity Runtime is built around a yielding architecture: each turn runs as a short slice that loads state, runs the model, dispatches tool calls, and yields, releasing all compute and memory in between. Because a slice never blocks waiting for a tool result or an event, the runtime is the first agent runtime that runs natively on serverless platforms. On AWS Lambda, an agent waiting on a three-day CI pipeline costs the same as one that was never created.",
            "State lives in durable storage rather than process memory, so agents survive restarts, redeploys, and cold starts. The core is a Rust library that you can embed directly in your own process (the local Infinity Code daemon does exactly this) or deploy to Lambda with the included CDK constructs.",
          ]}
          link="/docs/infinity-runtime/overview"
          linkLabel="Learn more →"
        >
          {(active) => <RuntimeDiagram active={active} />}
        </LayerSection>

        <LayerSection
          id="protocol"
          name="Reactive Agent Protocol"
          subtitle="Unified semantics for async work"
          paragraphs={[
            "The runtime's yielding model rests on the Reactive Agent Protocol (RAP), a successor to MCP that makes tool calls asynchronous. RAP servers deliver results whenever they are ready, without any long-lived connections. RAP also makes subscriptions a first-class concept, so a single call can stream an ongoing series of events to the agent, which reacts to each webhook, schedule, or alert as it arrives. Existing MCP servers run unchanged through a compatibility layer.",
            "Agent work is full of things that don't finish right away: a build runs for twenty minutes, a webhook fires hours later, a human approves tomorrow, a child thread reports when it's done. RAP expresses all of them with one set of semantics, so long-running calls, real-world events, and concurrency are one mechanism instead of three subsystems.",
          ]}
          link="/docs/rap/what-is-rap"
          linkLabel="Specification →"
        >
          {(active) => <ProtocolDiagram active={active} />}
        </LayerSection>

        <LayerSection
          id="code"
          name="Infinity Code"
          subtitle="Highly concurrent coding agents"
          paragraphs={[
            "Infinity Code leverages RAP and the Infinity Runtime to put the whole stack to work on your codebase. It spawns child threads to handle independent work in parallel, and streams logs from long-running commands back through subscriptions instead of blocking on them. It hibernates while that work runs, then merges each result into a sandboxed diff you review.",
            "Sessions persist in the local daemon, so you can detach from a busy agent and reconnect later, from the terminal or the desktop UI, with full context intact.",
          ]}
          link="/docs/infinity-code/overview"
          linkLabel="Get started →"
        >
          {(active) => <DesktopMini active={active} />}
        </LayerSection>

        <section className="closing-section">
          <h2>A composable, open stack</h2>
          <p>
            Infinity is a stack you can enter at any layer. Embed the runtime
            through its Rust API, build on RAP with your own runtime, or take
            Infinity Code as a finished coding agent. Everything is open source
            and MCP-compatible, so your existing tools keep working while you
            gain async execution.
          </p>
          <div className="closing-links">
            <a href="/docs/infinity-runtime/overview">Infinity Runtime →</a>
            <a href="/docs/rap/what-is-rap">RAP Specification →</a>
            <a href="/docs/infinity-code/overview">Infinity Code →</a>
          </div>
        </section>
      </main>
    </Layout>
  );
}
