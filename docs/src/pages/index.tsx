import React from 'react';
import Layout from '@theme/Layout';
import AnimatedTerminal from '../components/AnimatedTerminal';
import FeatureCards from '../components/FeatureCards';
import ProtocolComparison from '../components/ProtocolComparison';

export default function Home(): React.JSX.Element {
  return (
    <Layout
      title="Reactive Agent Protocol"
      description="The protocol for agents that never stop. Subscriptions, long-running tool calls, and agent hibernation."
    >
      <main className="landing-page">
        <section className="hero-section">
          <div className="hero-left">
            <h1>
              <span className="gradient-text">Reactive Agent</span>
              <br />
              Protocol
            </h1>
            <p>
              The tool protocol for agents that live in the real world and run forever. Subscribe to real-time events, execute operations that take hours, and pay nothing while idle.
            </p>
            <div className="hero-buttons">
              <a href="/docs/what-is-rap" className="primary">
                Get Started
              </a>
              <a href="/spec/overview" className="secondary">
                Specification →
              </a>
            </div>
          </div>
          <div className="hero-right">
            <AnimatedTerminal />
          </div>
        </section>

        <FeatureCards />
        <ProtocolComparison />
      </main>
    </Layout>
  );
}
