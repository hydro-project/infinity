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
          sidebarId: "rapSidebar",
          position: "left",
          label: "Reactive Agent Protocol",
        },
        {
          type: "docSidebar",
          sidebarId: "infinityRuntimeSidebar",
          position: "left",
          label: "Infinity Runtime",
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
          title: "Learn",
          items: [
            { label: "What is RAP?", to: "/docs/rap/what-is-rap" },
            { label: "Architecture", to: "/docs/rap/about/architecture" },
            { label: "Specification", to: "/docs/rap/spec/overview" },
          ],
        },
        {
          title: "Build",
          items: [
            {
              label: "Getting Started",
              to: "/docs/infinity-runtime/getting-started",
            },
            {
              label: "Build a RAP Tool",
              to: "/docs/rap/using-rap/building-a-rap-tool",
            },
            {
              label: "Infinity Runtime",
              to: "/docs/infinity-runtime/overview",
            },
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
