import React from "react";
import Layout from "@theme/Layout";
import AnimatedTerminal from "../components/AnimatedTerminal";
import FeatureCards from "../components/FeatureCards";
import ProtocolComparison from "../components/ProtocolComparison";
import SwarmSection from "../components/SwarmSection";

function InfinityCodeShowcase(): React.JSX.Element {
  return (
    <section className="showcase-section">
      <h2>Meet Infinity Code</h2>
      <p className="showcase-tagline">
        An AI coding agent for your terminal and desktop, powered by RAP. Spawn
        threads, hibernate between tasks, and let the agent work in remote
        sandboxes.
      </p>
      <div className="showcase-grid">
        <div className="showcase-card">
          <div className="showcase-img-wrapper">
            {/* TODO: replace with actual screenshot */}
            <img
              src="/img/screenshot-terminal-ui.png"
              alt="Infinity Code terminal UI — a TUI for chatting with the agent, viewing threads, and inspecting sandbox changes"
              className="showcase-img"
            />
          </div>
          <h3>Infinity CLI</h3>
          <p>
            A native terminal interface — no tmux required. Write code, spawn
            background agents, and manage remotes with ease.
          </p>
        </div>
        <div className="showcase-card">
          <div className="showcase-img-wrapper">
            {/* TODO: replace with actual screenshot */}
            <img
              src="/img/screenshot-desktop-ui.png"
              alt="Infinity Code desktop UI — a native app for managing agent sessions and viewing live thread activity"
              className="showcase-img"
            />
          </div>
          <h3>Infinity Desktop</h3>
          <p>
            A native desktop interface for concurrent sessions, watching live
            thread activity, and reviewing agent changes visually.
          </p>
        </div>
      </div>
      <div className="showcase-cta">
        <a href="/docs/infinity-code/overview" className="primary">
          Get started with Infinity Code →
        </a>
      </div>
    </section>
  );
}

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
              The tool protocol for agents that live in the real world and run
              forever. Subscribe to real-time events, execute operations that
              take hours, and pay nothing while idle.
            </p>
            <p className="hero-subtext">
              Powering <a href="/docs/infinity-code/overview">Infinity Code</a>{" "}
              — an AI coding agent for your terminal and desktop.
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

        <InfinityCodeShowcase />
        <FeatureCards />
        <ProtocolComparison />
        <SwarmSection />
      </main>
    </Layout>
  );
}
