---
sidebar_position: 3
title: Building a Runtime
---

# Building a Runtime

This guide walks through building a minimal RAP-compatible agent runtime in TypeScript. By the end, you'll have a working runtime that can discover tools dynamically, invoke them, handle results, and hibernate between calls.

The complete runtime is ~200 lines. We'll build it step by step.

## Setup

Create a new project and install dependencies:

```bash
mkdir my-rap-runtime && cd my-rap-runtime
npm init -y
npm install express @anthropic-ai/sdk
npm install -D typescript @types/express
npx tsc --init
```

You'll need an Anthropic API key in your environment:

```bash
export ANTHROPIC_API_KEY=your-key-here
```

## 1. Conversation state

The runtime is ephemeral: it starts, processes a message, and exits. Conversation history must be persisted to survive across invocations. For this example we'll use an in-memory store to keep things simple:

```typescript
const threads = new Map<string, { role: string; content: string }[]>();

function loadHistory(threadId: string) {
  return threads.get(threadId) || [];
}

function appendMessage(threadId: string, role: string, content: string) {
  if (!threads.has(threadId)) threads.set(threadId, []);
  threads.get(threadId)!.push({ role, content });
}
```

:::note
In-memory state is fine for prototyping, but a production runtime needs durable storage: a database, Redis, or even a file on disk. If the process crashes, in-memory history is lost. The Infinity Runtime uses Aurora DSQL; a simpler option would be SQLite or Postgres.
:::

## 2. Toolset discovery

RAP runtimes don't hardcode tool definitions. Instead, they discover tools dynamically by fetching [toolset definitions](/docs/rap/spec/basic/toolsets) from each tool server's well-known endpoint. The runtime is configured with a list of tool server base URLs and fetches `/.well-known/rap-toolset` from each on startup.

```typescript
interface RapTool {
  name: string;
  description: string;
  inputSchema: Record<string, unknown>;
  annotations?: Record<string, unknown>;
}

interface RapToolset {
  name: string;
  description?: string;
  endpoint: string;
  tools: RapTool[];
}

// Resolved tool: a tool definition paired with its endpoint
interface ResolvedTool {
  name: string;
  description: string;
  inputSchema: Record<string, unknown>;
  endpoint: string;
}
```

The loader fetches each toolset and flattens the tools into a single registry, pairing each tool with its toolset's endpoint URL:

```typescript
// Tool server base URLs; configure these for your deployment
const TOOL_SERVER_URLS = [
  'https://weather-tool.example.com',
  'https://github-tools.example.com',
];

// Session-scoped cache: toolsets are fetched once per session
const toolsetCache = new Map<string, RapToolset>();

async function loadToolsets(): Promise<ResolvedTool[]> {
  const resolved: ResolvedTool[] = [];

  for (const baseUrl of TOOL_SERVER_URLS) {
    // Use cached toolset if available (session-scoped)
    let toolset = toolsetCache.get(baseUrl);

    if (!toolset) {
      const url = `${baseUrl.replace(/\/$/, '')}/.well-known/rap-toolset`;
      const res = await fetch(url, {
        headers: { Accept: 'application/json' },
      });

      if (!res.ok) {
        console.error(`Failed to load toolset from ${url}: ${res.status}`);
        continue;
      }

      toolset = (await res.json()) as RapToolset;
      toolsetCache.set(baseUrl, toolset);
      console.log(`Loaded toolset '${toolset.name}' with ${toolset.tools.length} tools`);
    }

    for (const tool of toolset.tools) {
      resolved.push({
        name: tool.name,
        description: tool.description,
        inputSchema: tool.inputSchema,
        endpoint: toolset.endpoint,
      });
    }
  }

  // Validate uniqueness
  const names = resolved.map((t) => t.name);
  const dupes = names.filter((n, i) => names.indexOf(n) !== i);
  if (dupes.length > 0) {
    throw new Error(`Duplicate tool names across toolsets: ${dupes.join(', ')}`);
  }

  return resolved;
}
```

This follows the [Toolsets spec](/docs/rap/spec/basic/toolsets#loading-toolsets): toolsets are fetched from the discovery endpoint, cached for the session, and validated for name uniqueness. The runtime never loads tool definitions from local config; the tool server is the authoritative source.

## 3. Tool dispatch

When the LLM calls a tool, the runtime looks up the endpoint from the resolved tool registry and POSTs the [invocation](/docs/rap/spec/basic/tool-invocation). The tool acknowledges immediately, and the runtime does not wait for the actual result:

```typescript
async function dispatchToolCall(
  tools: ResolvedTool[],
  toolName: string,
  args: Record<string, unknown>,
  toolCallId: string,
  callbackUrl: string,
  threadId: string,
) {
  const tool = tools.find((t) => t.name === toolName);
  if (!tool) throw new Error(`Unknown tool: ${toolName}`);

  const payload = {
    operation: toolName,
    arguments: args,
    id: toolCallId,
    call_id: null,
    callback_url: callbackUrl,
    group_id: threadId,
    user_id: null,
  };

  // Fire-and-forget: we don't await the tool's actual work,
  // just the HTTP acknowledgment
  const res = await fetch(tool.endpoint, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload),
  });

  if (!res.ok) {
    throw new Error(`Tool ${toolName} returned ${res.status}`);
  }
}
```

This is the core of RAP's async model. The tool will POST its result to `callbackUrl` when it's done, which could be immediately, or hours later.

## 4. The completion loop

The runtime loads toolsets, sends conversation history and tool schemas to the LLM, and processes the response. If the LLM produces text, we accumulate it. If it calls tools, we dispatch them and exit; we don't loop waiting for results:

```typescript
import Anthropic from '@anthropic-ai/sdk';

const anthropic = new Anthropic();

async function runCompletion(threadId: string, callbackUrl: string) {
  // Load tools dynamically from tool servers
  const tools = await loadToolsets();

  const history = loadHistory(threadId);
  const messages = history.map((m) => ({
    role: m.role as 'user' | 'assistant',
    content: m.content,
  }));

  const response = await anthropic.messages.create({
    model: 'claude-sonnet-4-20250514',
    max_tokens: 4096,
    tools: tools.map((t) => ({
      name: t.name,
      description: t.description,
      input_schema: t.inputSchema as Anthropic.Tool.InputSchema,
    })),
    messages,
  });

  // Accumulate text output
  let outputText = '';

  for (const block of response.content) {
    if (block.type === 'text') {
      outputText += block.text;
    }
  }

  // Save assistant response to history
  appendMessage(threadId, 'assistant', JSON.stringify(response.content));

  // Dispatch any tool calls (fire-and-forget)
  for (const block of response.content) {
    if (block.type === 'tool_use') {
      await dispatchToolCall(
        tools,
        block.name,
        block.input as Record<string, unknown>,
        block.id,
        callbackUrl,
        threadId,
      );
    }
  }

  return outputText;
}
```

Notice: after dispatching tool calls, the function returns. The runtime exits. When the tool POSTs its result to the callback URL, the runtime starts again and runs another completion with the updated history. The toolset cache means we don't re-fetch tool definitions on every wake, only once per session.

## 5. The callback endpoint

This is the "front door" that tools POST results to. It receives tool results and subscription events, appends them to conversation history, and runs the completion loop again:

```typescript
import express from 'express';

const app = express();
app.use(express.json());

const CALLBACK_URL = process.env.CALLBACK_URL || 'http://localhost:3000/callback';

// Callback endpoint: tools POST results here
app.post('/callback', async (req, res) => {
  const { type, group_id, id, tool_call_id, text } = req.body;

  if (type === 'tool_result') {
    // Append the tool result to conversation history
    const toolResult = JSON.stringify([{
      type: 'tool_result',
      tool_use_id: id,
      content: text,
    }]);
    appendMessage(group_id, 'user', toolResult);

    // Run the completion loop again
    const output = await runCompletion(group_id, CALLBACK_URL);
    if (output) {
      console.log(`[${group_id}] ${output}`);
    }
  }

  if (type === 'subscription_event') {
    // Subscription events require synthetic tool call generation
    // to present the event to the LLM correctly.
    // See: /docs/rap/about/subscription-events#synthetic-tool-calls
    // This is not covered in this tutorial.
    console.log(`Subscription event received for ${group_id} (not handled in this example)`);
  }

  res.json({ ok: true });
});

// User input endpoint
app.post('/message', async (req, res) => {
  const { thread_id, text } = req.body;

  appendMessage(thread_id, 'user', text);
  const output = await runCompletion(thread_id, CALLBACK_URL);

  res.json({ response: output });
});

app.listen(3000, () => {
  console.log('RAP runtime listening on :3000');
});
```

## Running it

```bash
npx tsc
node dist/index.js
```

Send a message:

```bash
curl -X POST http://localhost:3000/message \
  -H 'Content-Type: application/json' \
  -d '{"thread_id": "test-1", "text": "What is the weather in Tokyo?"}'
```

The runtime discovers tools from the configured tool servers, calls the LLM, which invokes `get_weather`. The tool receives the invocation, acknowledges immediately, and later POSTs the result to `http://localhost:3000/callback`. The runtime wakes up, appends the result, runs the LLM again, and returns the final response.

## What this doesn't cover

This is a minimal runtime to demonstrate the protocol. A production runtime would add:

- **Subscription event handling**: requires generating [synthetic tool calls](/docs/rap/about/subscription-events#synthetic-tool-calls) to present events to the LLM in a way it can reason about. See [Subscription Events](/docs/rap/about/subscription-events) for the full design.
- **Concurrency control**: serialize messages within a thread (e.g. with a queue or database lock). See [Agent Runtime](/docs/rap/about/agent-runtime#interruption-model).
- **Deduplication**: queues deliver at least once. Track processed message IDs per thread so a redelivered tool result isn't appended to history twice.
- **Lifecycle notifications**: notify tool servers when a thread closes ([`POST /close_thread`](/docs/rap/spec/basic/thread-closure)) or a pending call is interrupted ([`POST /cancel_tool_call`](/docs/rap/spec/basic/tool-cancellation)), and track active subscriptions from results with `"subscription": true`.
- **Hibernation**: for a serverless deployment, replace the Express server with a Lambda triggered by SQS, and use scheduled messages for sleep. See [Agent Hibernation](/docs/rap/about/architecture#hibernation).
- **Authentication**: sign requests to tool servers with SigV4 or bearer tokens, and authenticate callback requests to prevent unauthorized message injection.
- **Streaming**: stream LLM responses to the user instead of waiting for the full completion.
- **Toolset validation**: validate fetched toolset definitions against the [schema requirements](/docs/rap/spec/basic/toolsets#validation) before making tools available to the LLM.

The [Infinity Runtime](/docs/infinity-runtime/overview) is a production-grade implementation that handles all of these.
