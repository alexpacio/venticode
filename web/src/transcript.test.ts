import { test } from "node:test";
import assert from "node:assert/strict";
import { fold, diffLines } from "./transcript.ts";

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
  assert.equal(view.usage?.context_ratio, 0.5);
});

test("unfinished tool calls are cancelled once the run stops", () => {
  const view = fold([e({ type: "run", state: "running" }), e({ type: "tool_call", call_id: "c9", operation: "execute", target: "make" }), e({ type: "run", state: "idle" })]);
  assert.equal((view.blocks[0] as any).status, "cancelled");
});

test("diffLines classifies unified diff lines", () => {
  assert.deepEqual(diffLines("@@ -1 +1 @@\n-a\n+b\n c").map((line) => line.kind), ["hunk", "del", "add", "ctx"]);
});
