import {themes as prismThemes} from 'prism-react-renderer';
import type {Config} from '@docusaurus/types';
import type * as Preset from '@docusaurus/preset-classic';

const config: Config = {
  title: 'Reactive Agent Protocol',
  tagline: 'The protocol for agents that never stop',
  favicon: 'img/favicon.ico',

  future: {
    v4: true,
  },

  url: 'https://reactiveagentprotocol.dev',
  baseUrl: '/',

  onBrokenLinks: 'throw',
  onBrokenMarkdownLinks: 'warn',

  i18n: {
    defaultLocale: 'en',
    locales: ['en'],
  },

  markdown: {
    mermaid: true,
  },

  themes: ['@docusaurus/theme-mermaid'],

  presets: [
    [
      'classic',
      {
        docs: {
          sidebarPath: './sidebars.ts',
        },
        blog: false,
        theme: {
          customCss: './src/css/custom.css',
        },
      } satisfies Preset.Options,
    ],
  ],

  plugins: [
    [
      '@docusaurus/plugin-content-docs',
      {
        id: 'spec',
        path: 'spec',
        routeBasePath: 'spec',
        sidebarPath: './sidebarsSpec.ts',
      },
    ],
  ],

  themeConfig: {
    colorMode: {
      defaultMode: 'dark',
      respectPrefersColorScheme: true,
    },
    navbar: {
      title: 'RAP',
      items: [
        {
          type: 'docSidebar',
          sidebarId: 'docsSidebar',
          position: 'left',
          label: 'Documentation',
        },
        {
          to: '/spec/overview',
          label: 'Specification',
          position: 'left',
          activeBaseRegex: '/spec/',
        },
      ],
    },
    footer: {
      style: 'dark',
      links: [
        {
          title: 'Learn',
          items: [
            { label: 'What is RAP?', to: '/docs/what-is-rap' },
            { label: 'Architecture', to: '/docs/about/architecture' },
            { label: 'Specification', to: '/spec/overview' },
          ],
        },
        {
          title: 'Build',
          items: [
            { label: 'Getting Started', to: '/docs/using-rap/getting-started' },
            { label: 'Build a RAP Tool', to: '/docs/using-rap/building-a-rap-tool' },
            { label: 'Infinity Runtime', to: '/docs/infinity-runtime/overview' },
          ],
        },
      ],
    },
    prism: {
      theme: prismThemes.github,
      darkTheme: prismThemes.dracula,
      additionalLanguages: ['rust', 'json', 'bash', 'typescript'],
    },
  } satisfies Preset.ThemeConfig,
};

export default config;
