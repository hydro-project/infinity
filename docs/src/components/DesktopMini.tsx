import React, { useEffect, useState, useRef, useCallback } from "react";
import { SessionSidebar, ChatView, DiffView } from "infinity-ui";
import type { SessionInfo, MessageItemType, SpinnerState } from "infinity-ui";
import "infinity-ui/theme.css";
import chatCss from "infinity-ui/components/ChatPanel.module.css";

/* ── Stub file contents (before edits) ── */

const STUB_LOGIN = `// TODO: implement login
export {}`;

const STUB_SESSION = `// TODO: implement sessions
export {}`;

const STUB_TOKEN = `// TODO: implement token generation
export {}`;

/* ── Existing diff (login system already built) ── */

const EXISTING_DIFF = [
  {
    path: "src/auth/login.ts",
    status: "modified" as const,
    oldContents: STUB_LOGIN,
    newContents: `import { db } from "../db";
import { hash, timingSafeEqual } from "../utils/crypto";
import { generateToken } from "./token";

export async function login(id: string, password: string) {
  const user = await db.query(
    "SELECT * FROM users WHERE id = $1",
    [id]
  );
  if (!user || !timingSafeEqual(user.password, hash(password))) {
    throw new Error("Invalid credentials");
  }
  return { token: generateToken(user), expiresIn: 3600 };
}

export async function getUserById(id: string) {
  return db.query("SELECT * FROM users WHERE id = $1", [id]);
}`,
  },
  {
    path: "src/auth/session.ts",
    status: "modified" as const,
    oldContents: STUB_SESSION,
    newContents: `import { db } from "../db";

export async function createSession(userId: string) {
  const token = crypto.randomUUID();
  await db.query(
    "INSERT INTO sessions (token, user_id, expires_at) VALUES ($1, $2, NOW() + interval '1 hour')",
    [token, userId]
  );
  return token;
}

export async function validateSession(token: string) {
  return db.query(
    "SELECT * FROM sessions WHERE token = $1 AND expires_at > NOW()",
    [token]
  );
}`,
  },
  {
    path: "src/auth/token.ts",
    status: "modified" as const,
    oldContents: STUB_TOKEN,
    newContents: `import { sign } from "jsonwebtoken";

const SECRET = process.env.JWT_SECRET!;

export function generateToken(user: { id: string; email: string }) {
  return sign({ sub: user.id, email: user.email }, SECRET, {
    expiresIn: "1h",
  });
}`,
  },
];

/* ── After squashing the test thread ── */

const DIFF_AFTER_TESTS = [
  ...EXISTING_DIFF,
  {
    path: "src/auth/__tests__/login.prop.test.ts",
    status: "added" as const,
    oldContents: "",
    newContents: `import fc from "fast-check";
import { login } from "../login";
import { db } from "../../db";

jest.mock("../../db");

describe("login — property-based", () => {
  it("never returns a token for wrong passwords", () => {
    fc.assert(
      fc.asyncProperty(
        fc.string({ minLength: 1 }),
        fc.string({ minLength: 1 }),
        async (id, password) => {
          (db.query as jest.Mock).mockResolvedValue(null);
          await expect(login(id, password)).rejects.toThrow();
        }
      )
    );
  });

  it("always returns a token for valid credentials", () => {
    fc.assert(
      fc.asyncProperty(
        fc.record({ id: fc.uuid(), password: fc.string({ minLength: 8 }) }),
        async ({ id, password }) => {
          (db.query as jest.Mock).mockResolvedValue({
            id, password, email: id + "@test.com"
          });
          const result = await login(id, password);
          expect(result.token).toBeDefined();
          expect(result.expiresIn).toBe(3600);
        }
      )
    );
  });
});`,
  },
];

/* ── After squashing the docs thread ── */

const DIFF_AFTER_DOCS = [
  ...DIFF_AFTER_TESTS,
  {
    path: "docs/auth.md",
    status: "added" as const,
    oldContents: "",
    newContents: `# Authentication

## Login

\`\`\`ts
import { login } from "./src/auth/login";

const { token, expiresIn } = await login(userId, password);
\`\`\`

Returns a JWT token valid for 1 hour. Throws on invalid credentials.

## Sessions

Sessions are stored in PostgreSQL with automatic expiry.

\`\`\`ts
import { createSession, validateSession } from "./src/auth/session";

const sessionToken = await createSession(userId);
const session = await validateSession(sessionToken);
\`\`\`
`,
  },
];

/* ── Diff patches for history tool results ── */

const TOKEN_PATCH = `--- a/src/auth/token.ts
+++ b/src/auth/token.ts
@@ -1,2 +1,9 @@
-// TODO: implement token generation
-export {}
+import { sign } from "jsonwebtoken";
+
+const SECRET = process.env.JWT_SECRET!;
+
+export function generateToken(user: { id: string; email: string }) {
+  return sign({ sub: user.id, email: user.email }, SECRET, {
+    expiresIn: "1h",
+  });
+}`;

const LOGIN_PATCH = `--- a/src/auth/login.ts
+++ b/src/auth/login.ts
@@ -1,2 +1,19 @@
-// TODO: implement login
-export {}
+import { db } from "../db";
+import { hash, timingSafeEqual } from "../utils/crypto";
+import { generateToken } from "./token";
+
+export async function login(id: string, password: string) {
+  const user = await db.query(
+    "SELECT * FROM users WHERE id = $1",
+    [id]
+  );
+  if (!user || !timingSafeEqual(user.password, hash(password))) {
+    throw new Error("Invalid credentials");
+  }
+  return { token: generateToken(user), expiresIn: 3600 };
+}
+
+export async function getUserById(id: string) {
+  return db.query("SELECT * FROM users WHERE id = $1", [id]);
+}`;

const SESSION_PATCH = `--- a/src/auth/session.ts
+++ b/src/auth/session.ts
@@ -1,2 +1,17 @@
-// TODO: implement sessions
-export {}
+import { db } from "../db";
+
+export async function createSession(userId: string) {
+  const token = crypto.randomUUID();
+  await db.query(
+    "INSERT INTO sessions (token, user_id, expires_at) VALUES ($1, $2, NOW() + interval '1 hour')",
+    [token, userId]
+  );
+  return token;
+}
+
+export async function validateSession(token: string) {
+  return db.query(
+    "SELECT * FROM sessions WHERE token = $1 AND expires_at > NOW()",
+    [token]
+  );
+}`;

/* ── Pre-existing chat history ── */

const HISTORY: MessageItemType[] = [
  {
    type: "user",
    text: "Implement the login and session system with JWT tokens and parameterized queries",
  },
  {
    type: "assistant",
    text: "I'll implement the auth module. All database queries will use parameterized statements.",
    done: true,
  },
  {
    type: "tool_call",
    name: "edit_file",
    displayText: "Edit src/auth/token.ts (9 lines)",
  },
  {
    type: "tool_result",
    segments: [
      {
        type: "diff",
        content: { path: "src/auth/token.ts", patch: TOKEN_PATCH },
      },
    ],
  },
  {
    type: "tool_call",
    name: "edit_file",
    displayText: "Edit src/auth/login.ts (19 lines)",
  },
  {
    type: "tool_result",
    segments: [
      {
        type: "diff",
        content: { path: "src/auth/login.ts", patch: LOGIN_PATCH },
      },
    ],
  },
  {
    type: "tool_call",
    name: "edit_file",
    displayText: "Edit src/auth/session.ts (17 lines)",
  },
  {
    type: "tool_result",
    segments: [
      {
        type: "diff",
        content: { path: "src/auth/session.ts", patch: SESSION_PATCH },
      },
    ],
  },
  {
    type: "assistant",
    text: "Done — implemented `login.ts`, `session.ts`, and `token.ts`. All queries use parameterized statements and passwords are compared with timing-safe equality.",
    done: true,
  },
];

/* ── Animation timeline ── */

interface Step {
  delay: number;
  action: (s: AnimState) => AnimState;
}

interface AnimState {
  sessions: Record<string, SessionInfo>;
  messages: MessageItemType[];
  spinner: SpinnerState | null;
  diffFiles: {
    path: string;
    status: "modified" | "added";
    oldContents: string;
    newContents: string;
  }[];
  chatVisible: boolean;
  typingInput: string;
}

const SESSION_ID = "session-demo";

const INITIAL: AnimState = {
  sessions: {
    [SESSION_ID]: {
      title: "Build auth system",
      last_updated: new Date().toISOString(),
      total_tokens_used: 3200,
      status: "Idle",
      threads: [],
    },
  },
  messages: [...HISTORY],
  spinner: null,
  diffFiles: EXISTING_DIFF,
  chatVisible: false,
  typingInput: "",
};

function patchSession(s: AnimState, patch: Partial<SessionInfo>): AnimState {
  return {
    ...s,
    sessions: {
      [SESSION_ID]: { ...s.sessions[SESSION_ID], ...patch },
    },
  };
}

function finishAssistant(msgs: MessageItemType[]): MessageItemType[] {
  return msgs.map((m) =>
    m.type === "assistant" && !m.done ? { ...m, done: true } : m,
  );
}

/** Generate steps that stream an assistant message word by word. */
function streamAssistant(
  text: string,
  firstDelay: number,
  wordDelay = 60,
): Step[] {
  const words = text.split(" ");
  return words.map((_, i) => ({
    delay: i === 0 ? firstDelay : wordDelay,
    action: (s: AnimState): AnimState => {
      const partial = words.slice(0, i + 1).join(" ");
      const msgs = s.messages;
      const last = msgs[msgs.length - 1];
      if (last && last.type === "assistant" && !last.done) {
        const updated = [...msgs];
        updated[updated.length - 1] = { ...last, text: partial };
        return { ...s, messages: updated, spinner: "thinking" };
      }
      return {
        ...s,
        messages: [...msgs, { type: "assistant", text: partial, done: false }],
        spinner: "thinking",
      };
    },
  }));
}

/** Generate steps that type text into the input field char by char, then send. */
function typeAndSend(text: string, firstDelay: number, charDelay = 30): Step[] {
  const steps: Step[] = [];
  for (let i = 0; i < text.length; i++) {
    steps.push({
      delay: i === 0 ? firstDelay : charDelay,
      action: (s) => ({ ...s, typingInput: text.slice(0, i + 1) }),
    });
  }
  // "Send" — move to messages, clear input
  steps.push({
    delay: 300,
    action: (s) => ({
      ...patchSession(s, { status: "Running" }),
      messages: [...s.messages, { type: "user", text }],
      typingInput: "",
      spinner: "thinking",
    }),
  });
  return steps;
}

const STEPS: Step[] = [
  // Trigger initial scroll by touching messages after layout
  {
    delay: 0,
    action: (s) => ({ ...s, messages: [...s.messages] }),
  },
  // Chat panel slides in
  {
    delay: 1000,
    action: (s) => ({ ...s, chatVisible: true }),
  },
  // User types and sends message
  ...typeAndSend(
    "Add property-based tests and write docs for this module",
    800,
  ),
  // Agent responds (word by word)
  ...streamAssistant(
    "I'll spawn two threads — one for property-based tests and one for documentation — so they run in parallel.",
    1500,
  ),
  // Spawn test thread
  {
    delay: 1200,
    action: (s) => ({
      ...s,
      messages: [
        ...finishAssistant(s.messages),
        {
          type: "tool_call",
          name: "spawn_thread",
          displayText:
            'spawn_thread({"instructions":"Write property-based tests for the auth module using fast-check"})',
        },
      ],
      spinner: "tool",
    }),
  },
  {
    delay: 600,
    action: (s) => ({
      ...patchSession(s, {
        total_tokens_used: 4100,
        threads: [
          {
            thread_id: "thread_tests",
            parent_thread_id: SESSION_ID,
            title: "Write property tests",
          },
        ],
      }),
      messages: [
        ...s.messages,
        {
          type: "tool_result",
          segments: [
            { type: "text", content: "Child thread spawned: thread_tests" },
          ],
        },
      ],
      spinner: "thinking",
    }),
  },
  // Spawn docs thread
  {
    delay: 400,
    action: (s) => ({
      ...s,
      messages: [
        ...s.messages,
        {
          type: "tool_call",
          name: "spawn_thread",
          displayText:
            'spawn_thread({"instructions":"Write documentation for the auth module"})',
        },
      ],
      spinner: "tool",
    }),
  },
  {
    delay: 600,
    action: (s) => ({
      ...patchSession(s, {
        total_tokens_used: 4400,
        threads: [
          ...s.sessions[SESSION_ID].threads,
          {
            thread_id: "thread_docs",
            parent_thread_id: SESSION_ID,
            title: "Write auth docs",
          },
        ],
      }),
      messages: [
        ...s.messages,
        {
          type: "tool_result",
          segments: [
            { type: "text", content: "Child thread spawned: thread_docs" },
          ],
        },
      ],
      spinner: "thinking",
    }),
  },
  // Agent says it'll wait (word by word)
  ...streamAssistant(
    "Both threads are running. Let me sleep until they finish.",
    500,
  ),
  // Agent calls sleep_until_event_or_input
  {
    delay: 400,
    action: (s) => ({
      ...s,
      messages: [
        ...finishAssistant(s.messages),
        {
          type: "tool_call",
          name: "sleep_until_event_or_input",
          displayText: "Sleep Until Event",
        },
      ],
      spinner: "tool",
    }),
  },
  // Test thread reports back (after a visible wait)
  {
    delay: 3500,
    action: (s) => ({
      ...patchSession(s, { total_tokens_used: 5200 }),
      messages: [
        ...s.messages,
        {
          type: "subscription",
          name: "thread_tests closed",
          text: "Created property-based tests in src/auth/__tests__/login.prop.test.ts. Tests cover invalid credentials (never returns token) and valid credentials (always returns token with correct expiry).",
        },
      ],
      spinner: "loading",
    }),
  },
  // Squash test thread
  {
    delay: 800,
    action: (s) => ({
      ...patchSession(s, {
        threads: s.sessions[SESSION_ID].threads.filter(
          (t) => t.thread_id !== "thread_tests",
        ),
      }),
      messages: [
        ...s.messages,
        {
          type: "tool_call",
          name: "squash_sandbox",
          displayText: 'squash_sandbox({"from_thread_id":"thread_tests"})',
        },
      ],
      spinner: "tool",
    }),
  },
  {
    delay: 700,
    action: (s) => ({
      ...patchSession(s, { total_tokens_used: 5500 }),
      messages: [
        ...s.messages,
        {
          type: "tool_result",
          segments: [
            { type: "text", content: "Squashed changes from thread_tests." },
          ],
        },
      ],
      spinner: "thinking",
      diffFiles: DIFF_AFTER_TESTS,
    }),
  },
  // Agent sleeps again waiting for docs thread
  {
    delay: 500,
    action: (s) => ({
      ...s,
      messages: [
        ...s.messages,
        {
          type: "tool_call",
          name: "sleep_until_event_or_input",
          displayText: "Sleep Until Event",
        },
      ],
      spinner: "tool",
    }),
  },
  // Docs thread reports back (after another wait)
  {
    delay: 4000,
    action: (s) => ({
      ...patchSession(s, { total_tokens_used: 6100 }),
      messages: [
        ...s.messages,
        {
          type: "subscription",
          name: "thread_docs closed",
          text: "Created docs/auth.md with usage examples for login and session APIs.",
        },
      ],
      spinner: "loading",
    }),
  },
  // Squash docs thread
  {
    delay: 800,
    action: (s) => ({
      ...patchSession(s, {
        threads: s.sessions[SESSION_ID].threads.filter(
          (t) => t.thread_id !== "thread_docs",
        ),
      }),
      messages: [
        ...s.messages,
        {
          type: "tool_call",
          name: "squash_sandbox",
          displayText: 'squash_sandbox({"from_thread_id":"thread_docs"})',
        },
      ],
      spinner: "tool",
    }),
  },
  {
    delay: 700,
    action: (s) => ({
      ...patchSession(s, { total_tokens_used: 6400 }),
      messages: [
        ...s.messages,
        {
          type: "tool_result",
          segments: [
            { type: "text", content: "Squashed changes from thread_docs." },
          ],
        },
      ],
      spinner: "thinking",
      diffFiles: DIFF_AFTER_DOCS,
    }),
  },
  // Final message (word by word)
  ...streamAssistant(
    "All done. Added property-based tests and documentation for the auth module.",
    1000,
  ),
  // Mark done
  {
    delay: 200,
    action: (s) => ({
      ...patchSession(s, { total_tokens_used: 6800, status: "Idle" }),
      messages: finishAssistant(s.messages),
      spinner: null,
    }),
  },
];

/* ── Main component ── */

const noop = () => {};

export default function DesktopMini({
  active,
}: {
  active: boolean;
}): React.JSX.Element {
  const [state, setState] = useState<AnimState>(INITIAL);
  const [gen, setGen] = useState(0);
  const cancelRef = useRef<ReturnType<typeof setTimeout>[]>([]);

  const clearTimers = useCallback(() => {
    for (const t of cancelRef.current) clearTimeout(t);
    cancelRef.current = [];
  }, []);

  useEffect(() => {
    if (!active) {
      setState(INITIAL);
      setGen((g) => g + 1);
      clearTimers();
      return;
    }
    setState(INITIAL);
    setGen((g) => g + 1);
    let cumulative = 0;
    for (const step of STEPS) {
      cumulative += step.delay;
      const t = setTimeout(() => {
        setState((prev) => step.action(prev));
      }, cumulative);
      cancelRef.current.push(t);
    }
    return clearTimers;
  }, [clearTimers, active]);

  // Resolve theme from docusaurus data-theme attribute
  const [theme, setTheme] = useState<"light" | "dark">("dark");
  useEffect(() => {
    const el = document.documentElement;
    const update = () =>
      setTheme(el.getAttribute("data-theme") === "light" ? "light" : "dark");
    update();
    const obs = new MutationObserver(update);
    obs.observe(el, { attributes: true, attributeFilter: ["data-theme"] });
    return () => obs.disconnect();
  }, []);

  /*
   * The wrapper uses `transform: scale(1)` which creates a new containing
   * block. This makes `position: fixed` children (SessionSidebar, chat panel)
   * position relative to this container instead of the viewport — so the real
   * components work exactly as in the app with zero overrides.
   */
  return (
    <div
      style={{
        position: "relative",
        width: "100%",
        height: 700,
        borderRadius: 12,
        overflow: "hidden",
        border: "1px solid var(--border)",
        background: "var(--bg)",
        transform: "scale(1)",
        fontSize: 13,
        fontFamily: "var(--font-sans)",
        boxShadow: "0 25px 50px -12px rgba(0,0,0,0.3)",
        color: "var(--text)",
      }}
    >
      {/* Sidebar — uses position:fixed, contained by transform */}
      <SessionSidebar
        sessions={state.sessions}
        activeSessionId={SESSION_ID}
        activeThreadId={null}
        pinned={true}
        visible={true}
        remotes={[]}
        localStatus="connected"
        onSelect={noop}
        onNew={noop}
        onTogglePin={noop}
        onWidthChange={noop}
        onDragStateChange={noop}
        embedded
      />

      {/* Main content area — offset to account for sidebar */}
      <div
        style={{
          position: "absolute",
          top: 0,
          left: 284,
          right: 0,
          bottom: 0,
        }}
      >
        <DiffView files={state.diffFiles} theme={theme} />
      </div>

      {/* Chat panel — uses position:fixed, contained by transform */}
      <div
        className={`${chatCss.chatPanel} ${!state.chatVisible ? chatCss.chatPanelHidden : ""}`}
      >
        <div className={chatCss.chatPanelHeader}>
          <span className={chatCss.chatPanelTitle}>Chat</span>
        </div>
        <div className={chatCss.chatPanelBody}>
          <ChatView
            messages={state.messages}
            generation={gen}
            spinner={state.spinner}
            onSend={noop}
            inputDisabled={false}
            pendingChoice={null}
            onChoiceSelect={noop}
            theme={theme}
            embeddedInput={state.typingInput || undefined}
          />
        </div>
      </div>
    </div>
  );
}
