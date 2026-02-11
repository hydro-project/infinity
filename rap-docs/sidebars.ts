import type {SidebarsConfig} from '@docusaurus/plugin-content-docs';

const sidebars: SidebarsConfig = {
  docsSidebar: [
    'what-is-rap',
    {
      type: 'category',
      label: 'About RAP',
      items: [
        'about/architecture',
        'about/agent-runtime',
        'about/tool-role',
      ],
    },
    {
      type: 'category',
      label: 'Using RAP',
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
        'infinity-runtime/hibernation',
        'infinity-runtime/built-in-tools',
        'infinity-runtime/threading',
      ],
    },
  ],
};

export default sidebars;
