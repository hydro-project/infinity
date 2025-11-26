import { ToolSet, ToolSetConfig } from './tool-set';
import { Tool } from './tool';
import { InfinityAgents } from './infinity-agents';

/**
 * A collection of individual tools
 */
export class CustomToolSet extends ToolSet {
  private readonly agent: InfinityAgents;
  private readonly tools: Tool[];

  constructor(agent: InfinityAgents, id: string, tools: Tool[]) {
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
