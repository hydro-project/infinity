/* ── Helpers for parsing serde_json-serialized Rust enums ── */

import type { DaemonMessage, ClientMessage } from "./types";

/** Parse a raw JSON string into a DaemonMessage. */
export function parseDaemonMessage(raw: string): DaemonMessage {
  return JSON.parse(raw) as DaemonMessage;
}

/** Serialize a ClientMessage to JSON string. */
export function serializeClientMessage(msg: ClientMessage): string {
  return JSON.stringify(msg);
}

/** Type-safe helper to extract the variant key from a DaemonMessage. */
export function msgTag(msg: DaemonMessage): string {
  if (typeof msg === "string") return msg;
  return Object.keys(msg)[0];
}

/** Extract the payload of a struct/tuple variant. */
export function msgPayload<T>(msg: DaemonMessage): T {
  if (typeof msg === "string") return undefined as T;
  return Object.values(msg)[0] as T;
}
