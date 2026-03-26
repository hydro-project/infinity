import type {SidebarsConfig} from '@docusaurus/plugin-content-docs';

const sidebars: SidebarsConfig = {
  docsSidebar: [
    'what-is-rap',
    {
      type: 'category',
      label: 'About RAP',
      items: [
        'about/architecture',
        'about/rap-servers',
        'about/subscription-events',
        'about/agent-runtime',
        'about/mcp-compatibility',
      ],
    },
    {
      type: 'category',
      label: 'Develop with RAP',
      items: [
        'using-rap/getting-started',
        'using-rap/building-a-rap-tool',
        'using-rap/building-a-runtime',
      ],
    },
    {
      type: 'category',
      label: 'Infinity Runtime',
      items: [
        'infinity-runtime/overview',
        'infinity-runtime/cloud-deployment',
        'infinity-runtime/built-in-tools',
        'infinity-runtime/threading',
      ],
    },
    {
      type: 'category',
      label: 'Infinity Code',
      items: [
        'infinity-code/quickstart',
        'infinity-code/coding-with-jj',
        'infinity-code/coding-with-git',

        'infinity-code/background-agents',
        'infinity-code/configuring-mcp',
        'infinity-code/rap-servers',
      ],
    },
  ],
};

export default sidebars;
