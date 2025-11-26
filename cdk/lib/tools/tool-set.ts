import { Construct } from 'constructs';

/**
 * Configuration for a tool set that will be passed to the agent
 */
export interface ToolSetConfig {
  type: string;
  [key: string]: any;
}

/**
 * Abstract base class for tool sets
 */
export abstract class ToolSet extends Construct {
  /**
   * Generate the configuration object for this tool set
   */
  abstract toConfig(): ToolSetConfig;
}
