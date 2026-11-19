import type { Diff, Todo, TranscriptEvent } from "./api/client.ts";

export const MAX_TOOL_OUTPUT_CHARS = 256 * 1024;
export const TOOL_OUTPUT_TRUNCATED_MARKER = "\n… [output troncato / output truncated]\n";

export interface ToolBlock {
  kind: "tool";
  id: string;
  operation: string;
  target: string;
  output: string;
  status: "running" | "waiting" | "ok" | "error" | "denied" | "cancelled";
  interactive?: "terminal" | "agent";
  summary?: string;
  diff?: Diff;
  approval?: { id: string; state: string; summary?: string };
}
export type Block =
  | { kind: "user"; id: string; content: string }
  | { kind: "text"; id: string; content: string }
  | { kind: "error"; id: string; message: string }
  | ToolBlock;

export interface Transcript {
  blocks: Block[];
  todos: Todo[];
  changes: Array<{ path: string; additions: number; deletions: number }>;
  usage?: TranscriptEvent["usage"];
  running: boolean;
  hasRunEvent: boolean;
  questions: ReadonlyMap<string, TranscriptEvent>;
  plans: ReadonlyMap<string, TranscriptEvent>;
  goals: ReadonlyMap<string, TranscriptEvent>;
}

export interface TranscriptState {
  blocks: Block[];
  tools: Map<string, ToolBlock>;
  activeTools: Set<ToolBlock>;
  truncatedTools: Set<ToolBlock>;
  changes: Map<string, { path: string; additions: number; deletions: number }>;
  questions: Map<string, TranscriptEvent>;
  plans: Map<string, TranscriptEvent>;
  goals: Map<string, TranscriptEvent>;
  todos: Todo[];
  usage?: TranscriptEvent["usage"];
  running: boolean;
  hasRunEvent: boolean;
  lastSequence: number | null;
}

export function createTranscriptState(): TranscriptState {
  return {
    blocks: [],
    tools: new Map(),
    activeTools: new Set(),
    truncatedTools: new Set(),
    changes: new Map(),
    questions: new Map(),
    plans: new Map(),
    goals: new Map(),
    todos: [],
    usage: undefined,
    running: false,
    hasRunEvent: false,
    lastSequence: null,
  };
}

function indexReference(map: Map<string, TranscriptEvent>, event: TranscriptEvent) {
  if (event.reference_id && !map.has(event.reference_id)) map.set(event.reference_id, event);
  if (event.message_id && !map.has(event.message_id)) map.set(event.message_id, event);
}

/** Incrementally applies one ordered transcript event to mutable state in O(1). */
export function applyEvent(state: TranscriptState, event: TranscriptEvent): TranscriptState {
  if (state.lastSequence !== null && event.sequence <= state.lastSequence) return state;
  state.lastSequence = event.sequence;
  const last = state.blocks.at(-1);
  switch (event.type) {
    case "user_message":
      indexReference(state.goals, event);
      state.blocks.push({ kind: "user", id: event.message_id || String(event.sequence), content: event.content || "" });
      break;
    case "text": {
      // Assistant text resumes as a new block after tool calls, like a terminal transcript.
      const msgId = event.message_id || `seq-${event.sequence}`;
      if (last?.kind === "text" && last.id.startsWith(msgId + ":")) last.content += event.delta || "";
      else state.blocks.push({ kind: "text", id: `${msgId}:${event.sequence}`, content: event.delta || "" });
      break;
    }
    case "tool_call": {
      const id = event.call_id || `tool-seq-${event.sequence}`;
      const block: ToolBlock = {
        kind: "tool",
        id,
        operation: event.operation || "",
        target: event.target || "",
        output: "",
        status: "running",
      };
      if (event.call_id) state.tools.set(event.call_id, block);
      state.activeTools.add(block);
      state.blocks.push(block);
      break;
    }
    case "tool_output": {
      if (!event.call_id) break;
      const block = state.tools.get(event.call_id);
      if (block) {
        if (event.interactive) block.interactive = event.interactive;
        const chunk = event.data || "";
        if (chunk && !state.truncatedTools.has(block)) {
          const remaining = MAX_TOOL_OUTPUT_CHARS - block.output.length;
          if (chunk.length <= remaining) {
            block.output += chunk;
          } else {
            block.output = block.output + chunk.slice(0, Math.max(0, remaining)) + TOOL_OUTPUT_TRUNCATED_MARKER;
            state.truncatedTools.add(block);
          }
        }
      }
      break;
    }
    case "tool_result": {
      if (!event.call_id) break;
      const block = state.tools.get(event.call_id);
      if (!block) break;
      const status = event.status || "ok";
      Object.assign(block, { status, summary: event.summary, diff: event.diff });
      state.activeTools.delete(block);
      if (event.diff && event.status === "ok") {
        const entry = state.changes.get(event.diff.path) || { path: event.diff.path, additions: 0, deletions: 0 };
        entry.additions += event.diff.additions;
        entry.deletions += event.diff.deletions;
        state.changes.set(entry.path, entry);
      }
      break;
    }
    case "approval": {
      if (!event.call_id) break;
      const block = state.tools.get(event.call_id);
      if (!block) break;
      const approvalId = event.approval_id || block.approval?.id;
      if (!approvalId) break;
      block.approval = { id: approvalId, state: event.approval_state || "pending", summary: event.summary };
      if (block.approval.state === "pending") {
        block.status = "waiting";
        state.activeTools.add(block);
      } else if (block.status === "waiting") {
        block.status = block.approval.state === "denied" ? "denied" : block.approval.state === "expired" ? "cancelled" : "running";
        if (block.status === "running") state.activeTools.add(block);
        else state.activeTools.delete(block);
      }
      break;
    }
    case "question":
      indexReference(state.questions, event);
      if (event.content && !(last?.kind === "text" && last.content.trim() === event.content.trim()))
        state.blocks.push({ kind: "text", id: String(event.sequence), content: event.content });
      break;
    case "plan":
      indexReference(state.plans, event);
      if (event.content && !(last?.kind === "text" && last.content.trim() === event.content.trim()))
        state.blocks.push({ kind: "text", id: String(event.sequence), content: event.content });
      break;
    case "goal_completed":
      if (event.content && !(last?.kind === "text" && last.content.trim() === event.content.trim()))
        state.blocks.push({ kind: "text", id: String(event.sequence), content: event.content });
      break;
    case "goal_cancelled":
      state.blocks.push({ kind: "text", id: String(event.sequence), content: event.content || "Obiettivo annullato / Goal cancelled" });
      break;
    case "todo":
      state.todos = event.todos || [];
      break;
    case "run":
      state.hasRunEvent = true;
      state.running = event.state === "running" || event.state === "waiting";
      if (event.usage) state.usage = event.usage;
      if (!state.running && state.activeTools.size > 0) {
        for (const block of state.activeTools) {
          if (block.status === "running" || block.status === "waiting") {
            block.status = "cancelled";
            if (block.approval?.state === "pending") block.approval.state = "expired";
          }
        }
        state.activeTools.clear();
      }
      break;
    case "error":
      state.blocks.push({ kind: "error", id: String(event.sequence), message: event.message || "" });
      break;
  }
  return state;
}

/** Produces a view snapshot from an incremental TranscriptState. */
export function toTranscript(state: TranscriptState): Transcript {
  const needsCancelSnapshot = !state.running && state.activeTools.size > 0;
  const blocks = needsCancelSnapshot
    ? state.blocks.map((block) => {
        if (block.kind === "tool" && state.activeTools.has(block) && (block.status === "running" || block.status === "waiting")) {
          return {
            ...block,
            status: "cancelled" as const,
            ...(block.approval?.state === "pending"
              ? { approval: { ...block.approval, state: "expired" } }
              : {}),
          };
        }
        return block;
      })
    : state.blocks.slice();
  return {
    blocks,
    todos: state.todos,
    changes: Array.from(state.changes.values(), (entry) => ({ ...entry })),
    usage: state.usage,
    running: state.running,
    hasRunEvent: state.hasRunEvent,
    questions: state.questions,
    plans: state.plans,
    goals: state.goals,
  };
}

/** Folds ordered transcript events into display blocks; later events win. */
export function fold(events: TranscriptEvent[]): Transcript {
  const state = createTranscriptState();
  for (const event of events) applyEvent(state, event);
  return toTranscript(state);
}

export type DiffLine = { kind: "add" | "del" | "hunk" | "ctx"; text: string };
export function diffLines(patch: string): DiffLine[] {
  return patch.split("\n").map((text) => ({
    kind: text.startsWith("@@") ? "hunk" : text.startsWith("+") ? "add" : text.startsWith("-") ? "del" : "ctx",
    text,
  }));
}
