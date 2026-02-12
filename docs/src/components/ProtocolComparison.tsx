import React from 'react';

export default function ProtocolComparison(): React.JSX.Element {
  return (
    <section className="comparison-section">
      <h2>MCP is synchronous. RAP is not.</h2>
      <p>
        MCP blocks the agent while tools run. RAP lets the agent hibernate and
        resume when results arrive — whether that takes 100ms or 3 days.
      </p>
      <div className="comparison-grid">
        <div className="comparison-card">
          <span className="label mcp">MCP</span>
          <h3>Agent blocks on every tool call</h3>
          <pre>
            <code>{`agent calls tool
  ↓
agent waits... (compute running)
agent waits... (still running)
agent waits... (20 min later)
  ↓
tool returns result
agent continues`}</code>
          </pre>
        </div>
        <div className="comparison-card">
          <span className="label rap">RAP</span>
          <h3>Agent hibernates, wakes on result</h3>
          <pre>
            <code>{`agent calls tool via HTTP
tool acknowledges immediately
agent exits (zero compute)
  ⋮
  ⋮  (20 min later, tool finishes)
  ⋮
tool POSTs result to RAP agent runtime
agent wakes, continues`}</code>
          </pre>
        </div>
      </div>
    </section>
  );
}
