import { test } from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
import { randomUUID } from "node:crypto";
import { createMock } from "./mock.ts";

async function start() {
  const server = createServer();
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
  const portal = "http://127.0.0.1:1";
  server.on("request", createMock({ origin: base, portalOrigin: portal, speed: 0 }));
  let cookie = "";
  const call = async (path: string, method = "GET", body?: unknown, origin = base) => {
    const response = await fetch(base + path, {
      method, redirect: "manual",
      headers: { ...(cookie ? { Cookie: cookie } : {}), ...(method !== "GET" ? { Origin: origin } : {}), ...(body !== undefined ? { "Content-Type": "application/json" } : {}) },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    cookie = response.headers.get("set-cookie")?.split(";")[0] || cookie;
    const text = await response.text();
    return { status: response.status, value: text ? JSON.parse(text) : null };
  };
  return { server, call, base, portal };
}
const waitFor = async (check: () => Promise<boolean>) => {
  for (let i = 0; i < 200; i++) { if (await check()) return; await new Promise((r) => setTimeout(r, 10)); }
  throw new Error("Timed out");
};

test("session, origin and inventory guards", async () => {
  const { server, call } = await start();
  try {
    assert.equal((await call("/api/v1/venticode/session")).status, 401);
    assert.equal((await call("/__dev/login")).status, 303);
    const session = await call("/api/v1/venticode/session");
    assert.equal(session.status, 200);
    const harness = (await call("/api/v1/venticode/harnesses")).value.items[0];
    assert.equal((await call("/api/v1/venticode/projects", "POST", { harness_id: harness.id, root: harness.roots[0], path: "src" }, "http://evil.invalid")).status, 403);
    assert.equal((await call("/api/v1/venticode/projects", "POST", { harness_id: harness.id, root: harness.roots[0], path: "../etc" })).status, 400);
    assert.equal((await call("/api/v1/venticode/projects", "POST", { harness_id: harness.id, root: "not-granted" })).status, 400);
    const created = await call("/api/v1/venticode/projects", "POST", { harness_id: harness.id, root: harness.roots[0], path: "src" });
    assert.equal(created.status, 201);
    assert.equal(created.value.name, "src");
    assert.equal((await call("/api/v1/venticode/projects", "POST", { harness_id: harness.id, root: harness.roots[0], path: "src" })).status, 409);
  } finally { server.closeAllConnections(); server.close(); }
});

test("ticket handoff requires the portal origin and is single use", async () => {
  const { server, call, base, portal } = await start();
  try {
    const { ticket } = (await call("/api/v1/venticode/access", "POST", undefined, portal)).value;
    const post = (origin: string) => fetch(base + "/venticode/bootstrap", { method: "POST", redirect: "manual", headers: { Origin: origin, "Content-Type": "application/x-www-form-urlencoded" }, body: "ticket=" + ticket });
    assert.equal((await post(base)).status, 403);
    const accepted = await post(portal);
    assert.equal(accepted.status, 303);
    assert.match(accepted.headers.get("set-cookie") || "", /^venticode_session=[a-f0-9]{64}; Path=\/; HttpOnly; SameSite=Lax/);
    assert.equal((await post(portal)).status, 409);
  } finally { server.closeAllConnections(); server.close(); }
});

test("message idempotency, approvals and run completion", async () => {
  const { server, call } = await start();
  try {
    await call("/__dev/login");
    const project = (await call("/api/v1/venticode/projects")).value.items[0];
    const conversation = (await call("/api/v1/venticode/conversations", "POST", { project_id: project.id, permission_mode: "ask" })).value;
    const id = randomUUID();
    const sent = await call(`/api/v1/venticode/conversations/${conversation.id}/messages`, "POST", { id, content: "Aggiorna il README" });
    assert.equal(sent.status, 202);
    assert.deepEqual((await call(`/api/v1/venticode/conversations/${conversation.id}/messages`, "POST", { id, content: "Aggiorna il README" })).value, sent.value);
    assert.equal((await call(`/api/v1/venticode/conversations/${conversation.id}/messages`, "POST", { id, content: "altro" })).status, 409);
    assert.equal((await call(`/api/v1/venticode/conversations/${conversation.id}/messages`, "POST", { id: randomUUID(), content: "parallel" })).status, 409);
    const events = async () => (await call(`/api/v1/venticode/conversations/${conversation.id}/events`)).value;
    const pending = async () => (await events()).items.filter((e: any) => e.type === "approval" && e.approval_state === "pending")
      .find((e: any) => !(events as any).seen?.has(e.approval_id));
    for (const decision of ["allow_once", "deny"]) {
      let approval: any;
      await waitFor(async () => Boolean(approval = await pending()));
      ((events as any).seen ||= new Set()).add(approval.approval_id);
      assert.equal((await call(`/api/v1/venticode/approvals/${approval.approval_id}`, "POST", { decision })).status, 200);
      assert.equal((await call(`/api/v1/venticode/approvals/${approval.approval_id}`, "POST", { decision })).status, 409);
    }
    await waitFor(async () => (await events()).state === "idle" && (await events()).items.at(-1).type === "run");
    const results = (await events()).items.filter((e: any) => e.type === "tool_result").map((e: any) => e.status);
    assert.deepEqual(results, ["ok", "ok", "ok", "denied"]);
    assert.equal((await call(`/api/v1/venticode/conversations/${conversation.id}`)).value.title, "Aggiorna il README");
  } finally { server.closeAllConnections(); server.close(); }
});

test("bot mentions use @ventic and review once", async () => {
  const { server, call } = await start();
  try {
    await call("/__dev/login");
    const bot = (await call("/api/v1/venticode/bots")).value.items[0];
    assert.equal(bot.handle, "@ventic");
    const held = (await call(`/api/v1/venticode/bots/${bot.id}/mentions`)).value.items.find((m: any) => m.state === "review");
    assert.match(held.text, /^@ventic /);
    assert.equal((await call(`/api/v1/venticode/bots/${bot.id}/mentions/${held.id}/review`, "POST", { decision: "approve" })).value.state, "replied");
    assert.equal((await call(`/api/v1/venticode/bots/${bot.id}/mentions/${held.id}/review`, "POST", { decision: "discard" })).status, 409);
    assert.equal((await call(`/api/v1/venticode/bots/${bot.id}`, "PATCH", { revision: bot.revision + 1, enabled: false })).status, 409);
    assert.equal((await call(`/api/v1/venticode/bots/${bot.id}`, "PATCH", { revision: bot.revision, enabled: false })).value.enabled, false);
  } finally { server.closeAllConnections(); server.close(); }
});

test("a read-only plan and an unanswered goal preserve pending work", async () => {
  const { server, call } = await start();
  try {
    await call("/__dev/login");
    const base = "/api/v1/venticode/conversations";
    const project = (await call("/api/v1/venticode/projects")).value.items[0];
    const chat = (await call(base, "POST", { project_id: project.id })).value;
    const path = base + "/" + chat.id;
    const send = (content: string, intent: string, reference_id?: string) =>
      call(path + "/messages", "POST", { id: randomUUID(), content, intent, reference_id });
    assert.equal((await send("Inspect README", "plan")).status, 202);
    await waitFor(async () => (await call(path)).value.state === "idle");
    const events = (await call(path + "/events")).value.items;
    const plan = (await call(path)).value.plan_id;
    assert.ok(plan);
    assert.ok(events.some((e: any) => e.type === "plan" && e.reference_id === plan));
    const latest = events.filter((e: any) => e.type === "todo").at(-1);
    assert.deepEqual(latest.todos.map((t: any) => t.status), ["completed", "pending", "pending"]);
    assert.equal((await send("Proceed", "implement_plan", randomUUID())).status, 409);
    assert.equal((await send("Ask a question", "goal")).status, 202);
    await waitFor(async () => (await call(path)).value.state === "idle");
    const pending = (await call(path)).value;
    assert.ok(pending.question_id);
    assert.ok(pending.goal_id);
    assert.equal((await send("README", "answer", randomUUID())).status, 409);
    assert.equal((await call(path + "/goal/" + pending.goal_id, "DELETE")).status, 204);
    const cancelled = (await call(path)).value;
    assert.equal(cancelled.goal_id, null);
    assert.equal(cancelled.question_id, null);
  } finally { server.closeAllConnections(); server.close(); }
});
