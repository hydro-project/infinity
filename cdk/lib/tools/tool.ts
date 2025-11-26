import { Construct } from 'constructs';

/**
 * Configuration for a tool that will be passed to the agent
 */
export interface ToolConfig {
  type: string;
  [key: string]: any;
}

/**
 * Abstract base class for tools
 */
export abstract class Tool extends Construct {
  /**
   * Generate the configuration object for this tool
   */
  abstract toConfig(): ToolConfig;
}
