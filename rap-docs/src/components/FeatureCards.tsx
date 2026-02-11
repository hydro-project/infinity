import React, { useState } from 'react';

interface Feature {
  icon: string;
  title: string;
  description: React.ReactNode;
  code: string;
  link: string;
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
    code: `🔧 Tool call:  subscribe_stripe_events({
                 event: "charge.succeeded",
                 min_amount: 500
               })
📥 Result:     "Subscribed. ID: sub_9k2m"

🔧 Tool call:  sleep_until_event_or_input()
💤 Hibernating...

  ⋮  (3 hours later, a charge arrives)

⚡ Subscription event:
  charge.succeeded — $847.00 from cus_Lx8mQ

🤖 Agent:  Looking up the customer details...`,
    link: '/docs/about/subscription-events',
  },
  {
    icon: '⏳',
    title: 'Long-running tools',
    description: (
      <>
        Tool calls are fire-and-forget. The agent dispatches a request and
        goes into hibernation. When the tool finishes — minutes or hours
        later — it POSTs the result and the agent resumes. No compute is
        consumed while waiting.
      </>
    ),
    code: `🔧 Tool call:  deploy_to_staging({
                 service: "api",
                 version: "v2.4.1"
               })
📥 Ack:        HTTP 200 (tool processing async)
💤 Hibernating...

  ⋮  (12 minutes later)

📥 Result:     "Deployed api v2.4.1 to staging.
                Health checks passing. 0 errors
                in the last 60s."

🤖 Agent:  Staging looks good. Ready to
            promote to production.`,
    link: '/docs/about/architecture#hibernation',
  },
  {
    icon: '💤',
    title: 'Hibernation',
    description: (
      <>
        When there's nothing to do, the agent shuts down completely. No idle
        process, no polling, no cost. It wakes instantly when a message arrives —
        user input, tool result, or external event. Agents can run for years at
        near-zero cost.
      </>
    ),
    code: `🔧 Tool call:  sleep_until({
                 date: "2025-03-15",
                 time: "09:30",
                 timezone: "America/New_York"
               })
💤 Hibernating... (zero compute)

  ⋮  (agent is not running)
  ⋮  (no process, no container, nothing)
  ⋮  (3 days later, 9:30 AM ET)

📥 Result:     "Woke up at target time:
                2025-03-15 09:30 America/New_York"

🤖 Agent:  Market is open. Checking portfolio...`,
    link: '/docs/infinity-runtime/hibernation',
  },
];

export default function FeatureCards(): React.JSX.Element {
  const [active, setActive] = useState(0);

  function switchTab(i: number) {
    if (i === active) return;
    setActive(i);
  }

  return (
    <section className="features-section">
      <h2 style={{ fontSize: "2.5rem" }}>Built for agents that live in the real world</h2>
      <div className="features-tabbed">
        <div className="features-left">
          {features.map((feature, i) => (
            <button
              key={feature.title}
              className={`feature-tab ${i === active ? 'feature-tab--active' : ''}`}
              onClick={() => switchTab(i)}
              type="button"
            >
              <span className="feature-tab-icon">{feature.icon}</span>
              <div>
                <h3>{feature.title}</h3>
                <p>{feature.description}</p>
              </div>
            </button>
          ))}
        </div>
        <div className="features-right">
          <pre><code>{features[active].code}</code></pre>
          <a href={features[active].link} className="feature-learn-more">
            Learn more →
          </a>
        </div>
      </div>
    </section>
  );
}
