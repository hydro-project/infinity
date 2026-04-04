import type {SidebarsConfig} from '@docusaurus/plugin-content-docs';

const sidebars: SidebarsConfig = {
  specSidebar: [
    'overview',
    {
      type: 'category',
      label: 'Basic Protocol',
      items: [
        'basic/lifecycle',
        'basic/transport',
        'basic/toolsets',
        'basic/tool-invocation',
        'basic/tool-result',
        'basic/thread-closure',
        'basic/tool-cancellation',
      ],
    },
    {
      type: 'category',
      label: 'Server Features',
      items: [
        'server/subscription-events',
        'server/oauth',
        'server/user-choice',
        'server/view-updates',
      ],
    },
  ],
};

export default sidebars;
