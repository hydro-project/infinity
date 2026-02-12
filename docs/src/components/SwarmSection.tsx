import React from 'react';

export default function SwarmSection(): React.JSX.Element {
  return (
    <section className="swarm-section">
      <h2>Built for Swarms</h2>
      <p>
        Agents spawn child threads for parallel work. Each child reports back
        via subscriptions — the parent stays focused while the swarm fans out.
      </p>
      <div className="comparison-grid">
        <div className="swarm-blurb">
          <h3>Threaded agents, not blocked agents</h3>
          <p>
            A parent agent breaks work into sub-tasks and spawns child threads.
            Each child runs independently with its own context window, processes
            its task, and reports results back to the parent using the same
            subscription mechanism that powers real-time events.
          </p>
          <p>
            The parent never blocks. Reports arrive as subscription events —
            the parent sees them in-context and decides what to do next. Children
            can send multiple interim reports before closing, giving the parent
            a live view of swarm progress.
          </p>
          <a href="/docs/infinity-runtime/threading" className="feature-learn-more">
            Threading docs →
          </a>
        </div>
        <div className="swarm-code">
          <pre>
            <code>{`🤖 Parent:  I'll review auth.ts in a thread.

🔧 spawn_thread({
     instructions: "Review auth.ts for security issues"
   })
📥 "Child spawned: thread_a1b2"

  ⋮  (parent continues working concurrently)

── report from thread_a1b2 ──
📥 "Critical: SQL injection at line 42."

🤖 Parent:  Addressing the injection issue…

  ⋮  (parent and child continue working)

── thread_a1b2 closed ──
📥 "Review complete. 1 critical issue found."

🤖 Parent:  Great, review's done. All issues addressed.`}</code>
          </pre>
        </div>
      </div>
    </section>
  );
}
