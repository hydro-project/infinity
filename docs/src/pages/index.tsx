import React, { useState, useEffect, useRef } from "react";
import Layout from "@theme/Layout";
import CodeBlock from "@theme/CodeBlock";
import MemoryChart from "../components/MemoryChart";
import RuntimeDiagram from "../components/RuntimeDiagram";
import AgentTrace from "../components/AgentTrace";
import ProtocolDiagram from "../components/ProtocolDiagram";
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

// Trimmed from the real quickstart; see /docs/infinity-runtime/agent-systems.
const HERO_CODE = `let system = AgentSystemBuilder::new_local(
    InMemoryConversationStore::new(),
    InMemoryStateStore::new(),
    StaticModel::new(provider, "claude-sonnet-4-5").await?,
)
.start();

let mut thread = system.thread_builder().launch().await;
thread.send_user_text("Write a haiku about Rust").await?;

while let Some(event) = thread.recv().await {
    println!("{event:?}");
}`;

function Chapter({
  id,
  kicker,
  title,
  prose,
  link,
  linkLabel,
  children,
}: {
  id: string;
  kicker: string;
  title: string;
  prose: React.ReactNode;
  link: string;
  linkLabel: string;
  children: (active: boolean) => React.ReactNode;
}) {
  const [ref, inView] = useInView(0.2);
  return (
    <section className="chapter" id={id} ref={ref}>
      <div className="chapter-copy">
        <p className="chapter-kicker">{kicker}</p>
        <h2>{title}</h2>
        {prose}
        <a href={link} className="chapter-link">
          {linkLabel}
        </a>
      </div>
      <div className="chapter-visual">{children(inView)}</div>
    </section>
  );
}

function ProductSection({
  id,
  name,
  subtitle,
  prose,
  link,
  linkLabel,
  alt,
  children,
}: {
  id: string;
  name: string;
  subtitle: string;
  prose: React.ReactNode;
  link: string;
  linkLabel: string;
  /** Render on the alternate (surface) background. */
  alt?: boolean;
  children: (active: boolean) => React.ReactNode;
}) {
  const [ref, inView] = useInView(0.2);
  return (
    <section
      className={alt ? "product-section product-alt" : "product-section"}
      id={id}
      ref={ref}
    >
      <div className="product-inner">
        <h2>{name}</h2>
        <p className="product-subtitle">{subtitle}</p>
        <div className="product-visual">{children(inView)}</div>
        <div className="product-prose">
          {prose}
          <a href={link} className="chapter-link">
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
      description="A Rust framework for building massively concurrent agentic systems, light enough to fit 75k agents in the memory of a Raspberry Pi."
    >
      <main className="landing-page">
        <section className="hero-section">
          <div className="hero-copy">
            <h1>Infinity</h1>
            <p className="hero-tagline">
              A Rust framework for building <b>massively concurrent</b> agentic
              systems, light enough to fit{" "}
              <b>75k agents in the memory of a Raspberry Pi</b>.
            </p>
            <p className="hero-subline">
              Infinity does for agents what async did for threads. Instead of
              blocking on slow tools, Infinity agents run them concurrently,
              yield while they wait, and cost nothing until the next event
              arrives.
            </p>
            <div className="hero-buttons">
              <a
                href="/docs/infinity-runtime/agent-systems/building-a-system"
                className="primary"
              >
                Get Started
              </a>
              <a
                href="https://github.com/hydro-project/infinity"
                className="secondary"
              >
                GitHub →
              </a>
            </div>
          </div>
          <div className="hero-code">
            <CodeBlock language="rust">{HERO_CODE}</CodeBlock>
          </div>
        </section>

        <div className="story">
          <Chapter
            id="scale"
            kicker="Scale"
            title="75k agents on a Raspberry Pi"
            prose={
              <>
                <p>
                  In Infinity, an idle agent is pure data: between turns, an
                  agent is just its conversation history, with no task, no
                  stack, and no open connection. After twenty tool-calling
                  turns, an agent occupies about 103 KB, so 75k of them fit in 8
                  GB of RAM.
                </p>
                <p>
                  Agents spend most of their lives waiting: builds run for
                  twenty minutes, webhooks fire hours later, humans reply
                  tomorrow. Runtimes that hold a process per agent pay for all
                  of that time; Infinity hibernates a waiting agent for free and
                  wakes it on the next message.
                </p>
              </>
            }
            link="/docs/infinity-runtime/architecture"
            linkLabel="How the runtime works →"
          >
            {(active) => <MemoryChart active={active} />}
          </Chapter>

          <Chapter
            id="asynchrony"
            kicker="Asynchrony"
            title="Agents that never block"
            prose={
              <>
                <p>
                  Infinity gives agents primitives for asynchrony. A tool call
                  can stay open as a subscription and stream events, so a shell
                  command delivers each chunk of output as it happens;{" "}
                  <code>spawn_thread</code> forks a subagent that works in
                  parallel; <code>sleep</code> awaits a timer or the next event
                  without polling.
                </p>
                <p>
                  Like actor systems, Infinity organizes agents into{" "}
                  <b>agent systems</b>: pools of concurrent agents that share
                  nothing and communicate through in-order messages to each
                  agent's mailbox. The runtime intelligently schedules the whole
                  pool the way an async executor schedules tasks, so thousands
                  of agents make progress on a few threads.
                </p>
              </>
            }
            link="/docs/infinity-runtime/agent-systems/overview"
            linkLabel="The Agent System API →"
          >
            {(active) => <AgentTrace active={active} />}
          </Chapter>

          <Chapter
            id="serverless"
            kicker="Serverless"
            title="Scale to zero is the default"
            prose={
              <>
                <p>
                  Because agent turns never block, Infinity is perfect for
                  serverless environments: on AWS Lambda, each SQS FIFO delivery
                  triggers one step of the runtime, which loads state, runs the
                  completion, dispatches tool calls, and exits.
                </p>
                <p>
                  Infinity agents can run <i>forever</i> with{" "}
                  <i>near-zero cost</i>: an agent waiting on a three-day CI
                  pipeline costs exactly nothing until something happens. The
                  included CDK constructs deploy the whole stack in a few lines,
                  and the same agent code runs unchanged on your laptop and in
                  the cloud.
                </p>
              </>
            }
            link="/docs/infinity-runtime/deploying-on-lambda"
            linkLabel="Deploy on AWS Lambda →"
          >
            {(active) => <RuntimeDiagram active={active} />}
          </Chapter>
        </div>

        <ProductSection
          id="rap"
          name="Reactive Agent Protocol"
          subtitle="Asynchronous tools over the network"
          alt
          prose={
            <>
              <p>
                With the <b>Reactive Agent Protocol</b> (RAP), you can serve
                tools over the network without holding a connection open: the
                runtime invokes a tool with one POST carrying a callback URL,
                and the server delivers results or subscription events whenever
                they are ready. Tool servers scale like ordinary web services,
                and they can serve hibernating agents that currently have no
                process at all.
              </p>
              <p>
                Infinity supports MCP out of the box: stdio and HTTP MCP servers
                connect in-process, and existing MCP servers run unchanged
                through a compatibility layer. Anyone can implement the open RAP
                specification in their own runtime or tool server.
              </p>
            </>
          }
          link="/docs/rap/what-is-rap"
          linkLabel="Read the RAP spec →"
        >
          {(active) => <ProtocolDiagram active={active} />}
        </ProductSection>

        <ProductSection
          id="code"
          name="Infinity Code"
          subtitle="A coding agent built for concurrent work"
          prose={
            <>
              <p>
                Infinity Code is a coding harness built on the runtime, and you
                extend it with the same RAP and MCP tools as any other Infinity
                agent. It boots instantly, and because sessions live in a
                daemon, the interface responds with zero latency even when the
                agent runs remotely. The agent runs builds and tests in the
                background while their output streams in, edits in parallel
                threads with stacked sandboxes, and hands you each result as a
                diff to review.
              </p>
            </>
          }
          link="/docs/infinity-code/overview"
          linkLabel="Get Infinity Code →"
        >
          {(active) => <DesktopMini active={active} />}
        </ProductSection>

        <section className="closing-section">
          <h2>Enter the stack at any layer</h2>
          <p>
            Embed the runtime through its Rust API, deploy it on Lambda with the
            CDK constructs, build tool servers on RAP, or take Infinity Code as
            a finished coding agent. Everything is open source and
            MCP-compatible, so your existing tools keep working while your
            agents stop paying to wait.
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
