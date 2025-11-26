import { ToolSet, ToolSetConfig } from './tool-set';
import { Tool } from './tool';
import { InfinityAgent } from '..';

/**
 * A collection of individual tools
 */
export class CustomToolSet extends ToolSet {
  private readonly agent: InfinityAgent;
  private readonly tools: Tool[];

  constructor(agent: InfinityAgent, id: string, tools: Tool[]) {
    super(agent, id);
    this.agent = agent;
    this.tools = tools;

    // Register this tool set with the agent
    agent.registerToolSet(this.toConfig());
  }

  toConfig(): ToolSetConfig {
    return {
      type: 'vec',
      tools: this.tools.map((tool) => tool.toConfig()),
    };
  }
}
