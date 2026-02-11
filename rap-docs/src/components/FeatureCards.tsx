import React from 'react';

interface Feature {
  icon: string;
  title: string;
  description: React.ReactNode;
}

const features: Feature[] = [
  {
    icon: '🔔',
    title: 'Subscriptions',
    description: (
      <>
        Tools register ongoing subscriptions — GitHub webhooks, price feeds,
        monitoring alerts. Each event wakes the agent, which processes it in an
        isolated thread and goes back to sleep. The subscription persists across
        hibernations.
      </>
    ),
  },
  {
    icon: '⏳',
    title: 'Long-running tool calls',
    description: (
      <>
        Tool calls are fire-and-forget. The agent dispatches a request, the tool
        acknowledges immediately, and the agent hibernates. When the tool
        finishes — minutes or hours later — it POSTs the result and the agent
        resumes.
      </>
    ),
  },
  {
    icon: '💤',
    title: 'Agent hibernation',
    description: (
      <>
        When there's nothing to do, the agent shuts down completely. No idle
        process, no polling, no cost. It wakes instantly when a message arrives —
        user input, tool result, or external event. Agents can run for weeks at
        near-zero cost.
      </>
    ),
  },
];

export default function FeatureCards(): React.JSX.Element {
  return (
    <section className="features-section">
      <h2>Built for agents that live in the real world</h2>
      <div className="features-grid">
        {features.map((feature) => (
          <div className="feature-card" key={feature.title}>
            <div className="feature-icon">{feature.icon}</div>
            <h3>{feature.title}</h3>
            <p>{feature.description}</p>
          </div>
        ))}
      </div>
    </section>
  );
}
