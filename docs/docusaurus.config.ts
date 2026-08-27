import { themes as prismThemes } from "prism-react-renderer";
import type { Config } from "@docusaurus/types";
import type * as Preset from "@docusaurus/preset-classic";

const config: Config = {
  title: "Infinity",
  tagline: "The open-source ecosystem for agents with principled concurrency",
  favicon: "img/favicon.ico",

  future: {
    v4: true,
  },

  url: "https://reactiveagentprotocol.dev",
  baseUrl: "/",

  onBrokenLinks: "throw",
  onBrokenMarkdownLinks: "warn",

  i18n: {
    defaultLocale: "en",
    locales: ["en"],
  },

  markdown: {
    mermaid: true,
  },

  themes: ["@docusaurus/theme-mermaid"],

  presets: [
    [
      "classic",
      {
        docs: {
          sidebarPath: "./sidebars.ts",
          sidebarCollapsed: false,
        },
        blog: false,
        theme: {
          customCss: "./src/css/custom.css",
        },
      } satisfies Preset.Options,
    ],
  ],

  plugins: ["./plugins/transpile-deps"],

  themeConfig: {
    colorMode: {
      defaultMode: "dark",
      respectPrefersColorScheme: true,
    },
    navbar: {
      title: "Infinity",
      items: [
        {
          type: "docSidebar",
          sidebarId: "infinityRuntimeSidebar",
          position: "left",
          label: "Infinity Runtime",
        },
        {
          type: "docSidebar",
          sidebarId: "rapSidebar",
          position: "left",
          label: "Reactive Agent Protocol",
        },
        {
          type: "docSidebar",
          sidebarId: "infinityCodeSidebar",
          position: "left",
          label: "Infinity Code",
        },
        {
          href: "https://github.com/hydro-project/infinity",
          position: "right",
          className: "header-github-link",
          "aria-label": "GitHub Repository",
        },
        {
          href: "https://discord.gg/QXKwMNA6RS",
          position: "right",
          className: "header-discord-link",
          "aria-label": "Discord server",
        },
      ],
    },
    footer: {
      style: "dark",
      links: [
        {
          title: "Infinity Runtime",
          items: [
            {
              label: "Quickstart",
              to: "/docs/infinity-runtime/agent-systems/building-a-system",
            },
            {
              label: "Architecture",
              to: "/docs/infinity-runtime/architecture",
            },
            {
              label: "Deploy on AWS Lambda",
              to: "/docs/infinity-runtime/deploying-on-lambda",
            },
          ],
        },
        {
          title: "Reactive Agent Protocol",
          items: [
            { label: "What is RAP?", to: "/docs/rap/what-is-rap" },
            {
              label: "Build a RAP Tool",
              to: "/docs/rap/using-rap/building-a-rap-tool",
            },
            { label: "Specification", to: "/docs/rap/spec/overview" },
          ],
        },
        {
          title: "Infinity Code",
          items: [
            { label: "Get Started", to: "/docs/infinity-code/overview" },
            {
              label: "Background Agents",
              to: "/docs/infinity-code/background-agents",
            },
            { label: "Slack Bot", to: "/docs/infinity-code/slack-bot" },
          ],
        },
      ],
      copyright: `Infinity is a <a href="https://hydro.run">Hydro</a> project co-led by open-source developers from the <a href="https://sky.cs.berkeley.edu">Sky Computing Lab</a> at UC Berkeley, Amazon Web Services, and various participating institutions.`,
    },
    prism: {
      theme: prismThemes.github,
      darkTheme: prismThemes.dracula,
      additionalLanguages: ["rust", "json", "bash", "typescript"],
    },
  } satisfies Preset.ThemeConfig,
};

export default config;
