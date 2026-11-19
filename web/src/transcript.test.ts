import { test } from "node:test";
import assert from "node:assert/strict";
import {
  applyEvent,
  createTranscriptState,
  diffLines,
  fold,
  MAX_TOOL_OUTPUT_CHARS,
  TOOL_OUTPUT_TRUNCATED_MARKER,
  toTranscript,
} from "./transcript.ts";

const at = "2026-01-01T00:00:00Z";
let sequence = 0;
const e = (event: Record<string, unknown>) => ({ sequence: ++sequence, created_at: at, ...event }) as any;

test("fold merges text, tracks tools, approvals and changes", () => {
  const events = [
    e({ type: "user_message", message_id: "u1", content: "ciao" }),
    e({ type: "run", state: "running" }),
    e({ type: "text", message_id: "a1", delta: "Leggo " }),
    e({ type: "text", message_id: "a1", delta: "il file." }),
    e({ type: "tool_call", call_id: "c1", operation: "edit_file", target: "a.ts" }),
    e({ type: "approval", call_id: "c1", approval_id: "p1", approval_state: "pending" }),
  ];
  let view = fold(events);
  assert.equal(view.blocks.length, 3);
  assert.deepEqual(view.blocks[1], { kind: "text", id: "a1:3", content: "Leggo il file." });
  assert.equal((view.blocks[2] as any).status, "waiting");
  assert.equal(view.running, true);
  assert.equal(view.hasRunEvent, true);

  events.push(
    e({ type: "approval", call_id: "c1", approval_id: "p1", approval_state: "allowed" }),
    e({ type: "tool_result", call_id: "c1", status: "ok", diff: { path: "a.ts", additions: 2, deletions: 1, patch: "" } }),
    e({ type: "text", message_id: "a1", delta: "Fatto." }),
    e({ type: "run", state: "idle", usage: { context_ratio: 0.5 } }),
  );
  view = fold(events);
  assert.equal((view.blocks[2] as any).status, "ok");
  assert.equal(view.blocks.at(-1)!.kind, "text", "text after a tool starts a new block");
  assert.deepEqual(view.changes, [{ path: "a.ts", additions: 2, deletions: 1 }]);
  assert.equal(view.running, false);
  assert.equal(view.hasRunEvent, true);
  assert.equal(view.usage?.context_ratio, 0.5);
});

test("unfinished tool calls are cancelled once the run stops", () => {
  const view = fold([
    e({ type: "run", state: "running" }),
    e({ type: "tool_call", call_id: "c9", operation: "execute", target: "make" }),
    e({ type: "run", state: "idle" }),
  ]);
  assert.equal((view.blocks[0] as any).status, "cancelled");
});

test("incremental applyEvent matches fold at every step and tracks hasRunEvent", () => {
  const events = [
    e({ type: "user_message", message_id: "u10", content: "avvia" }),
    e({ type: "tool_call", call_id: "c10", operation: "execute", target: "ls" }),
    e({ type: "run", state: "running" }),
    e({ type: "tool_output", call_id: "c10", data: "a\n" }),
    e({ type: "tool_output", call_id: "c10", data: "b\n", interactive: "terminal" }),
    e({ type: "approval", call_id: "c10", approval_id: "ap10", approval_state: "pending", summary: "run ls" }),
    e({ type: "approval", call_id: "c10", approval_id: "ap10", approval_state: "allowed" }),
    e({ type: "tool_result", call_id: "c10", status: "ok", summary: "done", diff: { path: "f.ts", additions: 3, deletions: 1, patch: "" } }),
    e({ type: "text", message_id: "m10", delta: "Prima " }),
    e({ type: "text", message_id: "m10", delta: "risposta" }),
    e({ type: "question", reference_id: "q10", content: "Confermi?", options: ["Sì", "No"] }),
    e({ type: "plan", reference_id: "pl10", content: "Piano A" }),
    e({ type: "todo", todos: [{ content: "step 1", status: "completed" }] }),
    e({ type: "error", message: "warn" }),
    e({ type: "run", state: "idle", usage: { context_ratio: 0.25 } }),
  ];

  const state = createTranscriptState();
  assert.equal(toTranscript(state).hasRunEvent, false);

  for (let i = 0; i < events.length; i++) {
    applyEvent(state, events[i]);
    const incremental = toTranscript(state);
    const full = fold(events.slice(0, i + 1));
    assert.deepEqual(incremental.blocks, full.blocks);
    assert.deepEqual(incremental.todos, full.todos);
    assert.deepEqual(incremental.changes, full.changes);
    assert.deepEqual(incremental.usage, full.usage);
    assert.equal(incremental.running, full.running);
    assert.equal(incremental.hasRunEvent, full.hasRunEvent);
    assert.equal(incremental.questions.get("q10")?.content, full.questions.get("q10")?.content);
    assert.equal(incremental.plans.get("pl10")?.content, full.plans.get("pl10")?.content);
    assert.equal(incremental.goals.get("u10")?.content, full.goals.get("u10")?.content);
  }

  // Duplicate sequence is ignored cleanly.
  const before = toTranscript(state).blocks.length;
  applyEvent(state, events[0]);
  assert.equal(toTranscript(state).blocks.length, before);
});

test("missing call_id and approval_id do not collide or throw", () => {
  const events = [
    e({ type: "run", state: "running" }),
    e({ type: "tool_call", operation: "execute", target: "cmd1" }),
    e({ type: "tool_call", operation: "execute", target: "cmd2" }),
    e({ type: "tool_output", data: "orphan output" }),
    e({ type: "tool_result", status: "ok" }),
    e({ type: "approval", approval_id: "ap-no-call", approval_state: "pending" }),
    e({ type: "tool_call", call_id: "c-valid", operation: "write_file", target: "x.ts" }),
    e({ type: "approval", call_id: "c-valid", approval_state: "pending" }),
    e({ type: "run", state: "idle" }),
  ];
  const view = fold(events);
  assert.equal(view.blocks.length, 3);
  assert.notEqual(view.blocks[0].id, view.blocks[1].id, "fallback tool ids are distinct");
  assert.equal((view.blocks[0] as any).output, "", "orphan tool_output does not attach to missing-id tools");
  assert.equal((view.blocks[0] as any).status, "cancelled");
  assert.equal((view.blocks[1] as any).status, "cancelled");
  assert.equal((view.blocks[2] as any).approval, undefined, "approval without approval_id is ignored");
  assert.equal((view.blocks[2] as any).status, "cancelled");
});

test("tool output is capped at MAX_TOOL_OUTPUT_CHARS with truncation marker", () => {
  const chunk = "x".repeat(100_000);
  const events = [
    e({ type: "run", state: "running" }),
    e({ type: "tool_call", call_id: "big", operation: "execute", target: "cat huge.log" }),
    e({ type: "tool_output", call_id: "big", data: chunk }),
    e({ type: "tool_output", call_id: "big", data: chunk }),
    e({ type: "tool_output", call_id: "big", data: chunk }),
    e({ type: "tool_output", call_id: "big", data: "ignored trailing chunk", interactive: "terminal" }),
    e({ type: "tool_result", call_id: "big", status: "ok" }),
    e({ type: "run", state: "idle" }),
  ];
  const view = fold(events);
  const tool = view.blocks[0] as any;
  assert.equal(tool.output.length, MAX_TOOL_OUTPUT_CHARS + TOOL_OUTPUT_TRUNCATED_MARKER.length);
  assert.ok(tool.output.endsWith(TOOL_OUTPUT_TRUNCATED_MARKER));
  assert.equal(tool.interactive, "terminal", "interactive flag still updates after output cap");

  const state = createTranscriptState();
  for (const ev of events) applyEvent(state, ev);
  assert.deepEqual(toTranscript(state).blocks, view.blocks);
});

test("diffLines classifies unified diff lines", () => {
  assert.deepEqual(diffLines("@@ -1 +1 @@\n-a\n+b\n c").map((line) => line.kind), ["hunk", "del", "add", "ctx"]);
});
