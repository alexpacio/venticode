import type { Diff, Todo, TranscriptEvent } from "./api/client.ts";

export interface ToolBlock {
  kind: "tool";
  id: string;
  operation: string;
  target: string;
  output: string;
  status: "running" | "waiting" | "ok" | "error" | "denied" | "cancelled";
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
}

/** Folds ordered transcript events into display blocks; later events win. */
export function fold(events: TranscriptEvent[]): Transcript {
  const blocks: Block[] = [];
  const tools = new Map<string, ToolBlock>();
  const changes = new Map<string, { path: string; additions: number; deletions: number }>();
  let todos: Todo[] = [];
  let usage: Transcript["usage"];
  let running = false;
  for (const event of events) {
    const last = blocks.at(-1);
    switch (event.type) {
      case "user_message":
        blocks.push({ kind: "user", id: event.message_id || String(event.sequence), content: event.content || "" });
        break;
      case "text":
        // Assistant text resumes as a new block after tool calls, like a terminal transcript.
        if (last?.kind === "text" && last.id.startsWith(event.message_id + ":")) last.content += event.delta || "";
        else blocks.push({ kind: "text", id: `${event.message_id}:${event.sequence}`, content: event.delta || "" });
        break;
      case "tool_call": {
        const block: ToolBlock = { kind: "tool", id: event.call_id!, operation: event.operation || "", target: event.target || "", output: "", status: "running" };
        tools.set(block.id, block);
        blocks.push(block);
        break;
      }
      case "tool_output": {
        const block = tools.get(event.call_id!);
        if (block) block.output += event.data || "";
        break;
      }
      case "tool_result": {
        const block = tools.get(event.call_id!);
        if (!block) break;
        Object.assign(block, { status: event.status || "ok", summary: event.summary, diff: event.diff });
        if (event.diff && event.status === "ok") {
          const entry = changes.get(event.diff.path) || { path: event.diff.path, additions: 0, deletions: 0 };
          entry.additions += event.diff.additions;
          entry.deletions += event.diff.deletions;
          changes.set(entry.path, entry);
        }
        break;
      }
      case "approval": {
        const block = tools.get(event.call_id!);
        if (!block) break;
        block.approval = { id: event.approval_id!, state: event.approval_state || "pending", summary: event.summary };
        if (block.approval.state === "pending") block.status = "waiting";
        else if (block.status === "waiting") block.status = block.approval.state === "denied" ? "denied" : block.approval.state === "expired" ? "cancelled" : "running";
        break;
      }
      case "todo":
        todos = event.todos || [];
        break;
      case "run":
        running = event.state === "running" || event.state === "waiting";
        if (event.usage) usage = event.usage;
        break;
      case "error":
        blocks.push({ kind: "error", id: String(event.sequence), message: event.message || "" });
        break;
    }
  }
  if (!running) for (const block of tools.values()) if (block.status === "running" || block.status === "waiting") block.status = "cancelled";
  return { blocks, todos, changes: [...changes.values()], usage, running };
}

export type DiffLine = { kind: "add" | "del" | "hunk" | "ctx"; text: string };
export function diffLines(patch: string): DiffLine[] {
  return patch.split("\n").map((text) => ({
    kind: text.startsWith("@@") ? "hunk" : text.startsWith("+") ? "add" : text.startsWith("-") ? "del" : "ctx",
    text,
  }));
}
