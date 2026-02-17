/**
 * Shared get-time tool logic — manifest and processing.
 * Imported by both the Lambda handler (index.mjs) and the local server (local.mjs).
 */

export const TOOLS = [
  {
    name: 'get_time',
    description: 'Get the current time in a specified timezone or UTC.',
    inputSchema: {
      type: 'object',
      properties: {
        timezone: {
          type: 'string',
          description: "IANA timezone name (e.g., 'America/New_York', 'Europe/London'). Defaults to UTC if not specified.",
        },
      },
      required: [],
    },
  },
];

/** Build the toolset manifest with the given endpoint URL. */
export function buildManifest(endpoint) {
  return {
    name: 'get-time',
    description: 'Utility tools for getting the current time',
    endpoint,
    tools: TOOLS,
  };
}

/** Process a get_time invocation and return the result text. */
export function processGetTime(args) {
  const now = new Date();
  const timeString = now.toISOString();
  const localTime = now.toLocaleString('en-US', {
    timeZone: args?.timezone || 'UTC',
    dateStyle: 'full',
    timeStyle: 'long',
  });

  return args?.timezone
    ? `Current time in ${args.timezone}: ${localTime}`
    : `Current UTC time: ${timeString}`;
}
