import type { components } from "./schema.ts";

type Schemas = components["schemas"];
export type Session = Schemas["Session"];
export type Model = Schemas["Model"];
export type Harness = Schemas["Harness"];
export type Project = Schemas["Project"];
export type Conversation = Schemas["Conversation"];
export type PermissionMode = Schemas["PermissionMode"];
export type Effort = Schemas["Effort"];
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

const DEFAULT_TIMEOUT_MS = 30_000;

export interface ApiOptions {
  signal?: AbortSignal;
  timeoutMs?: number;
}

const unauthorizedListeners = new Set<() => void>();
export function onUnauthorized(listener: () => void): () => void {
  unauthorizedListeners.add(listener);
  return () => {
    unauthorizedListeners.delete(listener);
  };
}

function buildSignal(callerSignal?: AbortSignal, timeoutMs = DEFAULT_TIMEOUT_MS): { signal: AbortSignal; cleanup: () => void } {
  if (typeof AbortSignal !== "undefined" && typeof AbortSignal.timeout === "function" && typeof AbortSignal.any === "function") {
    const timeoutSignal = AbortSignal.timeout(timeoutMs);
    return {
      signal: callerSignal ? AbortSignal.any([callerSignal, timeoutSignal]) : timeoutSignal,
      cleanup: () => {},
    };
  }
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(new DOMException("The operation timed out.", "TimeoutError")), timeoutMs);
  const onAbort = () => controller.abort(callerSignal?.reason);
  if (callerSignal) {
    if (callerSignal.aborted) controller.abort(callerSignal.reason);
    else callerSignal.addEventListener("abort", onAbort, { once: true });
  }
  return {
    signal: controller.signal,
    cleanup: () => {
      clearTimeout(timer);
      callerSignal?.removeEventListener("abort", onAbort);
    },
  };
}

export async function api<T>(path: string, method = "GET", body?: unknown, options?: ApiOptions): Promise<T> {
  const { signal, cleanup } = buildSignal(options?.signal, options?.timeoutMs);
  try {
    const response = await fetch(base + path, {
      method,
      credentials: "same-origin",
      signal,
      headers: body === undefined ? undefined : { "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await response.text();
    let value: any = null;
    try { value = text ? JSON.parse(text) : null; } catch { /* non-JSON error page */ }
    if (!response.ok) {
      if (response.status === 401 && path !== "/session") {
        for (const listener of unauthorizedListeners) listener();
      }
      throw new ApiError(response.status, value?.error?.message || response.statusText || `HTTP ${response.status}`);
    }
    return value as T;
  } finally {
    cleanup();
  }
}

export const list = <T>(path: string, options?: ApiOptions) =>
  api<{ items: T[] }>(path, "GET", undefined, options).then((value) => value.items);
