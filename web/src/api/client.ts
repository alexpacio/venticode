import type { components } from "./schema.ts";

type Schemas = components["schemas"];
export type Session = Schemas["Session"];
export type Model = Schemas["Model"];
export type Harness = Schemas["Harness"];
export type Project = Schemas["Project"];
export type Conversation = Schemas["Conversation"];
export type PermissionMode = Schemas["PermissionMode"];
export type TranscriptEvent = Schemas["Event"];
export type ApprovalDecision = Schemas["ApprovalDecision"];
export type Bot = Schemas["Bot"];
export type BotUpdate = Schemas["BotUpdate"];
export type ToolPolicy = Schemas["ToolPolicy"];
export type Mention = Schemas["Mention"];
export type Diff = Schemas["Diff"];
export type Todo = Schemas["Todo"];

export const base = "/api/v1/venticode";

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

export async function api<T>(path: string, method = "GET", body?: unknown): Promise<T> {
  const response = await fetch(base + path, {
    method,
    credentials: "same-origin",
    headers: body === undefined ? undefined : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let value: any = null;
  try { value = text ? JSON.parse(text) : null; } catch { /* non-JSON error page */ }
  if (!response.ok) throw new ApiError(response.status, value?.error?.message || response.statusText);
  return value as T;
}

export const list = <T>(path: string) => api<{ items: T[] }>(path).then((value) => value.items);
