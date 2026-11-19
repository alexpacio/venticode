// Synthetic, in-memory Venticode API for frontend development.
// Never calls a model or a harness; every run below is scripted.
// shortcut: state lives in memory and resets on restart; add persistence if
// browser tests ever need durable state across mock reloads.
import { createServer } from "node:http";
import type { IncomingMessage, ServerResponse } from "node:http";
import { randomUUID, randomBytes } from "node:crypto";
import { fileURLToPath } from "node:url";
import type { Duplex } from "node:stream";
import { createBrowserDemo } from "./browser-demo.ts";

type Json = Record<string, any>;
type Event = Json & { sequence: number; type: string; created_at: string };
type Listener = (event: Event) => void;

const now = () => new Date().toISOString();
const sleep = (ms: number, signal?: AbortSignal) =>
  new Promise<void>((resolve, reject) => {
    if (signal?.aborted) return reject(new Error("aborted"));
    const timer = setTimeout(resolve, ms);
    signal?.addEventListener("abort", () => { clearTimeout(timer); reject(new Error("aborted")); }, { once: true });
  });

export interface MockOptions { origin: string; portalOrigin: string; speed?: number }

export function createMock({ origin, portalOrigin, speed = 1 }: MockOptions) {
  const workspace = { id: "0b3a7c5e-1f2d-4e6a-9b8c-7d6e5f4a3b2c", name: "Workspace sintetico" };
  const user = { id: "5c4b3a29-1807-4f6e-8d5c-4b3a29180706", name: "Utente sintetico", email: "user@example.invalid" };
  const sessions = new Set<string>();
  const browser = createBrowserDemo();
  const sessionCookie = (cookie = "") => cookie.match(/(?:^|;\s*)venticode_session=([a-f0-9]{64})/)?.[1] || "";
  const tickets = new Map<string, number>();
  const models = [
    { id: "opus-5.5", name: "Opus 5.5", default: true },
    { id: "sonnet-5.5", name: "Sonnet 5.5" },
    { id: "haiku-5.5", name: "Haiku 5.5" },
  ];
  const harnesses: Json[] = [
    { id: "7f1c2d3e-4a5b-4c6d-8e7f-9a0b1c2d3e4f", name: "dev-workstation", roots: ["ventic", "ventic-frontend", "notes"], online: true, mode: "sandboxed", owned: true, revision: 3,
      host_info: { os: "Linux", arch: "x86_64", version: "0.9.0", hostname: "dev-workstation" },
      mcp_servers: [{ name: "postgres", transport: "stdio", tools: 6, status: "running" }, { name: "gitlab", transport: "http", tools: 14, status: "running" }, { name: "playwright", transport: "stdio", tools: 21, status: "stopped" }] },
    { id: "2a3b4c5d-6e7f-4081-9a2b-3c4d5e6f7081", name: "build-runner-01", roots: ["ventic-common"], online: true, mode: "yolo", owned: false, revision: 1,
      host_info: { os: "Linux", arch: "aarch64", version: "0.9.0" }, mcp_servers: [] },
    { id: "9e8d7c6b-5a49-4382-a716-05f4e3d2c1b0", name: "studio-mac", roots: ["design-tokens"], online: false, mode: "sandboxed", owned: true, revision: 2,
      host_info: { os: "macOS", arch: "arm64", version: "0.8.4" }, mcp_servers: [] },
  ];
  const projects: Json[] = [];
  const conversations: Json[] = [];
  const events = new Map<string, Event[]>();
  const listeners = new Map<string, Set<Listener>>();
  const runs = new Map<string, AbortController>();
  const approvals = new Map<string, Json>();
  const messageIds = new Map<string, { conversation: string; content: string; result: Json }>();
  const ago = (minutes: number) => new Date(Date.now() - minutes * 60000).toISOString();

  const addProject = (harness: Json, root: string, path = ".", name = root) => {
    const project = { id: randomUUID(), harness_id: harness.id, root, path, name, created_at: ago(60 * 24 * 7) };
    projects.push(project);
    return project;
  };
  const addConversation = (project: Json, title: string, minutes: number) => {
    const conversation = { id: randomUUID(), project_id: project.id, harness_id: project.harness_id, title, model: "opus-5.5", permission_mode: "ask", state: "idle", revision: 1, created_at: ago(minutes + 30), updated_at: ago(minutes) };
    conversations.push(conversation);
    events.set(conversation.id, []);
    return conversation;
  };
  const push = (conversationId: string, event: Json) => {
    const list = events.get(conversationId)!;
    const full = { ...event, sequence: list.length + 1, created_at: now() } as Event;
    list.push(full);
    for (const listener of listeners.get(conversationId) || []) listener(full);
    return full;
  };
  const setState = (conversation: Json, state: string, extra: Json = {}) => {
    conversation.state = state;
    conversation.updated_at = now();
    push(conversation.id, { type: "run", state, ...extra });
  };

  // Seeds: one finished transcript so the UI has history to render.
  const [dev, runner, mac] = harnesses;
  const ventic = addProject(dev, "ventic");
  addProject(dev, "ventic-frontend");
  const common = addProject(runner, "ventic-common");
  addProject(mac, "design-tokens");
  const seeded = addConversation(ventic, "Accesso harness per progetto", 90);
  addConversation(ventic, "Revisione ordine lease WAF", 60 * 26);
  addConversation(common, "Rigenera fixture dei contratti", 60 * 5);
  {
    const id = seeded.id, run = randomUUID(), m1 = randomUUID(), m2 = randomUUID();
    const read = randomUUID(), edit = randomUUID(), exec = randomUUID();
    push(id, { type: "user_message", message_id: m1, content: "Aggiungi una migrazione che limiti gli accessi harness per progetto, poi esegui i test di venticode-harness." });
    push(id, { type: "run", run_id: run, state: "running" });
    push(id, { type: "todo", todos: [{ content: "Leggere il modello di accesso", status: "completed" }, { content: "Aggiungere la migrazione", status: "completed" }, { content: "Eseguire i test", status: "completed" }] });
    push(id, { type: "text", message_id: m2, delta: "Controllo come sono modellati oggi gli accessi, poi aggiungo la migrazione." });
    push(id, { type: "tool_call", call_id: read, operation: "read_file", target: "services/control/src/harness.rs", arguments: { path: "services/control/src/harness.rs" } });
    push(id, { type: "tool_result", call_id: read, status: "ok", summary: "412 righe" });
    push(id, { type: "tool_call", call_id: edit, operation: "write_file", target: "services/control/migrations/0055_harness_project_access.sql", arguments: {} });
    push(id, { type: "tool_result", call_id: edit, status: "ok", summary: "+6", diff: { path: "services/control/migrations/0055_harness_project_access.sql", additions: 6, deletions: 0,
      patch: "@@ -0,0 +1,6 @@\n+CREATE TABLE harness_project_access (\n+  harness_id uuid NOT NULL REFERENCES harnesses(id) ON DELETE CASCADE,\n+  project_root text NOT NULL,\n+  granted_at timestamptz NOT NULL DEFAULT now(),\n+  PRIMARY KEY (harness_id, project_root)\n+);" } });
    push(id, { type: "tool_call", call_id: exec, operation: "execute", target: "cargo test -p venticode-harness --release", arguments: { command: "cargo test -p venticode-harness --release" } });
    push(id, { type: "tool_output", call_id: exec, stream: "stdout", data: "running 18 tests\ntest result: ok. 18 passed; 0 failed\n" });
    push(id, { type: "tool_result", call_id: exec, status: "ok", summary: "exit 0 · 41 s" });
    push(id, { type: "text", message_id: m2, delta: "\n\nMigrazione aggiunta e 18 test superati." });
    push(id, { type: "run", run_id: run, state: "idle", usage: { input_tokens: 48210, output_tokens: 2210, context_ratio: 0.24 } });
  }

  const bots: Json[] = [{ id: "c0ffee00-1234-4abc-8def-0123456789ab", name: "Assistente piattaforma", handle: "@ventic", harness_id: runner.id, root: "ventic-common", enabled: true, revision: 1,
    channels: [{ kind: "slack", name: "#eng-help" }, { kind: "slack", name: "#releases" }, { kind: "gitlab", name: "ventic/* merge request" }],
    triggers: { mentions: true, direct_messages: true }, tools: { read: "auto", write: "review", execute: "review", mcp: "auto" },
    instructions: "Rispondi in modo breve, cita file e job, non incollare mai credenziali o dati dei clienti.", rate_limit_per_hour: 30 }];
  const mentions = new Map<string, Json[]>([[bots[0].id, [
    { id: randomUUID(), channel: { kind: "gitlab", name: "ventic/ventic-backend !214" }, author: "Giulia", text: "@ventic puoi aggiungere un test per il rinnovo del lease prima del merge?", state: "review",
      reply: "Ho aggiunto test_renewal_extends_fence_before_expiry, che copre rinnovo e rilascio del lease di configurazione.", created_at: ago(4),
      tool_calls: [{ operation: "read_file", target: "deploy/infrastructure/leases.py", status: "ok" }, { operation: "edit_file", target: "tests/test_leases.py", status: "held" }] },
    { id: randomUUID(), channel: { kind: "slack", name: "#eng-help" }, author: "Marco", text: "@ventic perché la pipeline su main è fallita?", state: "replied",
      reply: "Il job distribution-tests ha rifiutato un checkout frontend non pulito: ripinna da un commit pubblicato e rilancia.", created_at: ago(12), replied_at: ago(12),
      tool_calls: [{ operation: "mcp_call_tool", target: "gitlab.get_job_log", status: "ok" }, { operation: "read_file", target: "scripts/sources.py", status: "ok" }] },
  ]]]);

  const fail = (status: number, message: string) => ({ status, json: { error: { message } } });
  const ok = (json: Json, status = 200) => ({ status, json });
  const isUuid = (v: unknown) => typeof v === "string" && /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(v);

  async function approval(conversation: Json, callId: string, operation: string, summary: string, signal: AbortSignal) {
    const id = randomUUID();
    const entry: Json = { id, conversation: conversation.id, operation, state: "pending" };
    const decided = new Promise<string>((resolve) => { entry.resolve = resolve; });
    approvals.set(id, entry);
    push(conversation.id, { type: "approval", approval_id: id, call_id: callId, operation, summary, approval_state: "pending" });
    setState(conversation, "waiting");
    signal.addEventListener("abort", () => entry.resolve("expired"), { once: true });
    const decision = await decided;
    entry.state = decision === "deny" ? "denied" : decision === "expired" ? "expired" : "allowed";
    if (decision === "allow_project") conversation.allowed = new Set([...(conversation.allowed || []), operation]);
    push(conversation.id, { type: "approval", approval_id: id, call_id: callId, operation, summary, approval_state: entry.state });
    if (entry.state === "expired") throw new Error("aborted");
    setState(conversation, "running");
    return entry.state === "allowed";
  }

  async function run(conversation: Json, runId: string, content: string, signal: AbortSignal) {
    const say = async (messageId: string, text: string) => {
      for (const chunk of text.match(/[\s\S]{1,14}/g) || []) { push(conversation.id, { type: "text", message_id: messageId, delta: chunk }); await sleep(30 * speed, signal); }
    };
    const tool = async (operation: string, target: string, ask: boolean, result: Json, output?: string[]) => {
      const callId = randomUUID();
      push(conversation.id, { type: "tool_call", call_id: callId, operation, target, arguments: { target } });
      if (ask && !conversation.allowed?.has(operation) && !(await approval(conversation, callId, operation, target, signal))) {
        push(conversation.id, { type: "tool_result", call_id: callId, status: "denied", summary: "Negato" });
        return false;
      }
      await sleep(400 * speed, signal);
      for (const line of output || []) { push(conversation.id, { type: "tool_output", call_id: callId, stream: "stdout", data: line + "\n" }); await sleep(250 * speed, signal); }
      push(conversation.id, { type: "tool_result", call_id: callId, ...result });
      return true;
    };
    const mode = conversation.permission_mode, project = projects.find((p) => p.id === conversation.project_id)!;
    const harness = harnesses.find((h) => h.id === conversation.harness_id)!;
    const yolo = harness.mode === "yolo";
    const todos = [{ content: "Esplorare il progetto", status: "in_progress" }, { content: "Applicare la modifica", status: "pending" }, { content: "Verificare con i test", status: "pending" }];
    const progress = (index: number) => push(conversation.id, { type: "todo", todos: todos.map((t, i) => ({ ...t, status: i < index ? "completed" : i === index ? "in_progress" : "pending" })) });
    const answer = randomUUID();
    try {
      setState(conversation, "running", { run_id: runId });
      progress(0);
      await say(answer, `Risposta sintetica (nessun modello reale). Esploro ${project.root}/${project.path === "." ? "" : project.path} su ${harness.name}.\n\n`);
      await tool("list_files", ".", false, { status: "ok", summary: "14 voci" });
      await tool("read_file", "README.md", false, { status: "ok", summary: "120 righe" });
      todos[0].status = "completed";
      if (/\b(domanda|question)\b/i.test(content)) {
        conversation.question_id=randomUUID();
        push(conversation.id,{type:"question",reference_id:conversation.question_id,content:"Quale file devo aggiornare?",options:["README.md","CONTRIBUTING.md"]});
      } else if (mode === "plan") {
        conversation.plan_id=randomUUID();
        push(conversation.id,{type:"plan",reference_id:conversation.plan_id,content:"Aggiornare README.md e verificare con i test."});
      } else {
        progress(1);
        const edited = await tool("edit_file", "README.md", mode === "ask" && !yolo, { status: "ok", summary: "+2 −1", diff: { path: "README.md", additions: 2, deletions: 1,
          patch: `@@ -1,3 +1,4 @@\n # ${project.name}\n-Draft notes.\n+${content.slice(0, 60).replace(/\n/g, " ")}\n+See CONTRIBUTING.md before opening a merge request.\n ` } });
        progress(2);
        const ran = await tool("execute", "npm test", !yolo, { status: "ok", summary: "exit 0 · 3 s" }, ["> test", "> node --test", "✔ 12 tests passed"]);
        todos[1].status = edited ? "completed" : "pending";
        todos[2].status = ran ? "completed" : "pending";
        if (edited && ran && conversation.goal_id) {
          push(conversation.id,{type:"goal_completed",reference_id:conversation.goal_id,content:"README aggiornato e test superati."});
          conversation.goal_id=null;
        }
        await say(answer, edited && ran ? "Fatto: README aggiornato e test superati." : "Mi fermo qui: alcune operazioni sono state negate. Dimmi come procedere.");
      }
      push(conversation.id, { type: "todo", todos });
      setState(conversation, "idle", { run_id: runId, usage: { input_tokens: 18420, output_tokens: 640, context_ratio: 0.12 } });
    } catch {
      setState(conversation, "idle", { run_id: runId });
    } finally {
      runs.delete(conversation.id);
    }
  }

  function route(method: string, path: string, body: Json, headers: Json): Json | undefined {
    const cookie = sessionCookie(headers.cookie);
    const authed = sessions.has(cookie);
    if (method === "POST" && path === "/api/v1/venticode/access") {
      if (headers.origin !== portalOrigin) return fail(403, "Origine non consentita / Origin not allowed");
      // The portal session itself is synthetic in this loopback-only mock.
      const ticket = randomBytes(32).toString("hex");
      tickets.set(ticket, Date.now() + 60000);
      return ok({ url: origin, ticket, expires_in: 60 });
    }
    if (method === "POST" && path === "/venticode/bootstrap") {
      if (headers.origin !== portalOrigin) return fail(403, "Origine non consentita / Origin not allowed");
      const fields = new URLSearchParams(body.raw || "");
      const ticket = fields.get("ticket") || "";
      const expires = tickets.get(ticket);
      tickets.delete(ticket);
      if (!expires || expires < Date.now()) return fail(409, "Ticket scaduto o già usato / Ticket expired or already used");
      return login(fields.get("browser") === "1" ? "/browser" : "/");
    }
    if (method === "GET" && path === "/__dev/login") {
      const id = body.query.get("conversation");
      const result = login(body.query.get("browser") === "1" ? "/browser" : "/");
      if (isUuid(id) && conversations.some(c => c.id === id)) result.headers.Location = "/c/" + id;
      return result;
    }
    if (!path.startsWith("/api/v1/venticode/")) return fail(404, "Not found");
    if (!authed) return fail(401, "Sessione assente / No session");
    if (method !== "GET" && headers.origin !== origin) return fail(403, "Origine non consentita / Origin not allowed");
    const p = path.slice("/api/v1/venticode".length);
    let m: RegExpMatchArray | null;

    if (p === "/session" && method === "GET") return ok({ user, workspace, role: "tenant_admin", portal_url: portalOrigin });
    if (p === "/browser-agent" && method === "GET") return ok({ threads: browser.threads() });
    if (p === "/session" && method === "DELETE") {
      sessions.delete(cookie);
      return { ...ok({ signed_out: true, redirect_url: portalOrigin + "/app" }), headers: { "Set-Cookie": "venticode_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0" } };
    }
    if (p === "/models" && method === "GET") return ok({ items: models });
    if (p === "/harnesses" && method === "GET") return ok({ items: harnesses });
    if (p === "/projects" && method === "GET") {
      const harnessId = body.query.get("harness_id");
      return ok({ items: projects.filter((x) => !harnessId || x.harness_id === harnessId) });
    }
    if (p === "/projects" && method === "POST") {
      const harness = harnesses.find((h) => h.id === body.harness_id);
      if (!harness) return fail(404, "Harness non trovato / Harness not found");
      const path = body.path ?? ".";
      if (!harness.roots.includes(body.root) || typeof path !== "string" || path.length > 1024 || path.startsWith("/") || path.includes("\\") || path.split("/").includes(".."))
        return fail(400, "Cartella non valida / Invalid folder");
      if (body.name !== undefined && (typeof body.name !== "string" || !body.name.trim() || body.name.length > 80)) return fail(400, "Nome non valido / Invalid name");
      if (projects.some((x) => x.harness_id === harness.id && x.root === body.root && x.path === path)) return fail(409, "Progetto già presente / Project already exists");
      const project = addProject(harness, body.root, path, body.name?.trim() || (path === "." ? body.root : path.split("/").pop()));
      project.created_at = now();
      return ok(project, 201);
    }
    if ((m = p.match(/^\/projects\/([^/]+)$/)) && method === "DELETE") {
      const index = projects.findIndex((x) => x.id === m![1]);
      if (index < 0) return fail(404, "Progetto non trovato / Project not found");
      if (conversations.some((c) => c.project_id === m![1] && c.state !== "idle")) return fail(409, "Conversazione in corso / A conversation is running");
      projects.splice(index, 1);
      for (let i = conversations.length - 1; i >= 0; i--) if (conversations[i].project_id === m[1]) { events.delete(conversations[i].id); conversations.splice(i, 1); }
      return ok({ deleted: true });
    }
    if (p === "/conversations" && method === "GET") {
      const projectId = body.query.get("project_id");
      return ok({ items: conversations.filter((c) => !projectId || c.project_id === projectId).sort((a, b) => b.updated_at.localeCompare(a.updated_at)).slice(0, 200) });
    }
    if (p === "/conversations" && method === "POST") {
      const project = projects.find((x) => x.id === body.project_id);
      if (!project) return fail(404, "Progetto non trovato / Project not found");
      if ((body.model !== undefined && !models.some((x) => x.id === body.model)) || (body.permission_mode !== undefined && !["ask", "auto_edit", "plan"].includes(body.permission_mode)) || (body.title !== undefined && (typeof body.title !== "string" || body.title.length > 200)))
        return fail(400, "Richiesta non valida / Invalid request");
      const conversation = addConversation(project, body.title?.trim() || "Nuova conversazione", 0);
      Object.assign(conversation, { created_at: now(), updated_at: now(), model: body.model || "opus-5.5", permission_mode: body.permission_mode || "ask" });
      return ok(conversation, 201);
    }
    if ((m = p.match(/^\/conversations\/([^/]+)\/goal\/([^/]+)$/)) && method==="DELETE") {
      const c=conversations.find(c=>c.id===m![1]);
      if(!c)return fail(404,"Not found");
      if(runs.has(c.id)||c.goal_id!==m[2])return fail(409,"Conflict");
      push(c.id,{type:"goal_cancelled",reference_id:c.goal_id}); c.goal_id=null; c.question_id=null; c.plan_id=null; c.revision++;
      return ok(null,204);
    }
    if ((m = p.match(/^\/conversations\/([^/]+)(\/[a-z]+)?$/))) {
      const conversation = conversations.find((c) => c.id === m![1]);
      if (!conversation) return fail(404, "Conversazione non trovata / Conversation not found");
      const action = m[2] || "";
      if (!action && method === "GET") return ok(conversation);
      if (!action && method === "PATCH") {
        const keys = Object.keys(body.json);
        if (body.runtime !== undefined && body.runtime !== "project" || !Number.isInteger(body.revision) || keys.length < 2 || keys.some((k) => !["revision", "title", "model", "permission_mode"].includes(k)) ||
            (body.title !== undefined && (typeof body.title !== "string" || !body.title.trim() || body.title.length > 200)) ||
            (body.model !== undefined && !models.some((x) => x.id === body.model)) ||
            (body.permission_mode !== undefined && !["ask", "auto_edit", "plan"].includes(body.permission_mode)))
          return fail(400, "Richiesta non valida / Invalid request");
        if (body.revision !== conversation.revision) return fail(409, "Conversazione modificata / Conversation changed");
        for (const k of ["title", "model", "permission_mode"]) if (body[k] !== undefined) conversation[k] = k === "title" ? body[k].trim() : body[k];
        conversation.revision++;
        conversation.updated_at = now();
        return ok(conversation);
      }
      if (!action && method === "DELETE") {
        if (conversation.state !== "idle") return fail(409, "Conversazione in corso / Conversation is running");
        conversations.splice(conversations.indexOf(conversation), 1);
        events.delete(conversation.id);
        return ok({ deleted: true });
      }
      if (action === "/events" && method === "GET") {
        const after = Number(body.query.get("after") || 0);
        if (!Number.isInteger(after) || after < 0) return fail(400, "Cursore non valido / Invalid cursor");
        const list = events.get(conversation.id)!;
        const items = list.slice(after, after + 1000);
        return ok({ items, cursor: items.at(-1)?.sequence ?? after, state: conversation.state });
      }
      if (action === "/stream" && method === "GET") return { stream: conversation.id };
      if (action === "/messages" && method === "POST") {
        if (!isUuid(body.id) || typeof body.content !== "string" || !body.content.trim() || body.content.length > 100000) return fail(400, "Messaggio non valido / Invalid message");
        const intent=body.intent ?? "message";
        if(!["message","goal","answer","plan","implement_plan"].includes(intent) || (["answer","implement_plan"].includes(intent) !== isUuid(body.reference_id)))return fail(400,"Invalid intent");
        const fingerprint=JSON.stringify([body.content,intent,body.reference_id??null]);
        const previous = messageIds.get(body.id);
        if (previous) return previous.conversation === conversation.id && previous.content === fingerprint ? ok(previous.result, 202) : fail(409, "Id messaggio già usato / Message id already used");
        const harness = harnesses.find((h) => h.id === conversation.harness_id);
        if (!harness?.online) return fail(423, "Harness offline");
        if (runs.has(conversation.id)) return fail(409, "Un'esecuzione è già in corso / A run is already active");
        if((conversation.question_id && intent!=="answer") || (intent==="answer" && conversation.question_id!==body.reference_id) ||
           (intent==="implement_plan" && (conversation.plan_id!==body.reference_id || conversation.permission_mode!=="plan")) || (intent==="goal" && conversation.goal_id))return fail(409,"Conflict");
        if(intent==="goal")conversation.goal_id=body.id;
        if(intent==="plan")conversation.permission_mode="plan";
        if(intent==="implement_plan")conversation.permission_mode="ask";
        conversation.plan_id=null;conversation.question_id=null;conversation.revision++;
        const result = { run_id: randomUUID(), message_id: body.id };
        messageIds.set(body.id, { conversation: conversation.id, content: fingerprint, result });
        if (conversation.title === "Nuova conversazione") { conversation.title = body.content.trim().split("\n")[0].slice(0, 60); conversation.revision++; }
        push(conversation.id, { type: "user_message", message_id: body.id, content: body.content, intent, reference_id:body.reference_id });
        const controller = new AbortController();
        runs.set(conversation.id, controller);
        void run(conversation, result.run_id, body.content, controller.signal);
        return ok(result, 202);
      }
      if (action === "/interrupt" && method === "POST") {
        runs.get(conversation.id)?.abort();
        return ok({ state: "idle" });
      }
      return fail(405, "Metodo non consentito / Method not allowed");
    }
    if ((m = p.match(/^\/approvals\/([^/]+)$/)) && method === "POST") {
      const entry = approvals.get(m[1]);
      if (!entry) return fail(404, "Approvazione non trovata / Approval not found");
      if (!["allow_once", "allow_project", "deny"].includes(body.decision)) return fail(400, "Decisione non valida / Invalid decision");
      if (entry.state !== "pending") return fail(409, "Approvazione già decisa o scaduta / Approval already decided or expired");
      entry.state = body.decision === "deny" ? "denied" : "allowed";
      entry.resolve(body.decision);
      return ok({ state: entry.state });
    }
    if (p === "/connections" && method === "GET") return ok({ items: bots.map(b => ({
      id: b.id, kind: "slack", destination: "C_SYNTHETIC", identity: "@ventic", status: "connected",
      project_id: projects[0].id, webhook_url: origin + "/hooks/slack/" + b.id,
    })) });
    if (p.startsWith("/connections")) return fail(501, "Connessione esterna non disponibile nel mock / External connection unavailable in the mock");
    if (p === "/bots" && method === "GET") return ok({ items: bots });
    if ((m = p.match(/^\/bots\/([^/]+)(\/mentions)?$/))) {
      const bot = bots.find((b) => b.id === m![1]);
      if (!bot) return fail(404, "Bot non trovato / Bot not found");
      if (m[2] && method === "GET") return ok({ items: mentions.get(bot.id) || [] });
      if (!m[2] && method === "GET") return ok(bot);
      if (!m[2] && method === "PATCH") {
        const keys = Object.keys(body.json), policy = (v: unknown) => ["auto", "review", "off"].includes(v as string);
        if (!Number.isInteger(body.revision) || keys.length < 2 || keys.some((k) => !["revision", "enabled", "triggers", "tools", "instructions", "rate_limit_per_hour", "runtime"].includes(k)) ||
            (body.enabled !== undefined && typeof body.enabled !== "boolean") ||
            (body.triggers !== undefined && (typeof body.triggers?.mentions !== "boolean" || typeof body.triggers?.direct_messages !== "boolean" || Object.keys(body.triggers).length !== 2)) ||
            (body.tools !== undefined && (Object.keys(body.tools || {}).length !== 4 || !["read", "write", "execute", "mcp"].every((k) => policy(body.tools[k])))) ||
            (body.instructions !== undefined && (typeof body.instructions !== "string" || body.instructions.length > 8000)) ||
            (body.rate_limit_per_hour !== undefined && (!Number.isInteger(body.rate_limit_per_hour) || body.rate_limit_per_hour < 1 || body.rate_limit_per_hour > 1000)))
          return fail(400, "Richiesta non valida / Invalid request");
        if (body.revision !== bot.revision) return fail(409, "Bot modificato / Bot changed");
        for (const k of keys) if (k !== "revision") bot[k] = body[k];
        bot.revision++;
        return ok(bot);
      }
      return fail(405, "Metodo non consentito / Method not allowed");
    }
    if ((m = p.match(/^\/bots\/([^/]+)\/mentions\/([^/]+)\/review$/)) && method === "POST") {
      const mention = mentions.get(m[1])?.find((x) => x.id === m![2]);
      if (!mention) return fail(404, "Menzione non trovata / Mention not found");
      if (!["approve", "discard"].includes(body.decision)) return fail(400, "Decisione non valida / Invalid decision");
      if (mention.state !== "review") return fail(409, "Menzione già gestita / Mention already handled");
      mention.state = body.decision === "approve" ? "replied" : "discarded";
      if (body.decision === "approve") { mention.replied_at = now(); mention.tool_calls = mention.tool_calls.map((t: Json) => ({ ...t, status: t.status === "held" ? "ok" : t.status })); }
      return ok(mention);
    }
    return fail(404, "Not found");

    function login(destination = "/") {
      const token = randomBytes(32).toString("hex");
      sessions.add(token);
      return { status: 303, headers: { Location: destination, "Set-Cookie": `venticode_session=${token}; Path=/; HttpOnly; SameSite=Lax` } };
    }
  }

  function stream(req: IncomingMessage, res: ServerResponse, conversationId: string, after: number) {
    res.writeHead(200, { "Content-Type": "text/event-stream", "Cache-Control": "no-cache, no-transform", "X-Accel-Buffering": "no" });
    res.flushHeaders();
    const write = (event: Event) => res.write(`id: ${event.sequence}\nevent: transcript\ndata: ${JSON.stringify(event)}\n\n`);
    for (const event of (events.get(conversationId) || []).slice(after)) write(event);
    const set = listeners.get(conversationId) || new Set<Listener>();
    listeners.set(conversationId, set);
    set.add(write);
    const heartbeat = setInterval(() => res.write("event: heartbeat\ndata: {}\n\n"), 15000);
    req.once("close", () => { set.delete(write); clearInterval(heartbeat); });
  }

  async function handle(req: IncomingMessage, res: ServerResponse) {
    const url = new URL(req.url || "/", "http://127.0.0.1");
    const chunks: Buffer[] = [];
    let size = 0;
    for await (const chunk of req) { size += chunk.length; if (size > 1 << 20) { res.writeHead(413).end(); return; } chunks.push(chunk); }
    const raw = Buffer.concat(chunks).toString("utf8");
    let json: Json = {};
    if (raw && req.headers["content-type"]?.includes("application/json")) {
      try { json = JSON.parse(raw); } catch { res.writeHead(400, { "Content-Type": "application/json" }).end(JSON.stringify({ error: { message: "JSON non valido / Invalid JSON" } })); return; }
      if (!json || typeof json !== "object" || Array.isArray(json)) json = {};
    }
    const result = route(req.method || "GET", url.pathname, { ...json, json, raw, query: url.searchParams }, req.headers);
    if (result?.stream) {
      const after = Math.max(Number(url.searchParams.get("after") || 0), Number(req.headers["last-event-id"] || 0));
      return stream(req, res, result.stream, Number.isInteger(after) && after > 0 ? after : 0);
    }
    res.writeHead(result!.status, { "Cache-Control": "no-store", ...(result!.json ? { "Content-Type": "application/json" } : {}), ...result!.headers });
    res.end(result!.json ? JSON.stringify(result!.json) : undefined);
  }

  function upgrade(req: IncomingMessage, socket: Duplex, head: Buffer) {
    const path = new URL(req.url || "/", origin).pathname;
    const thread = path.match(/^\/api\/v1\/venticode\/browser-agent\/([0-9a-f-]+)$/)?.[1];
    if (!isUuid(thread) || req.headers.origin !== origin || !sessions.has(sessionCookie(req.headers.cookie))) {
      socket.end("HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n");
      return;
    }
    browser.upgrade(req, socket, head, (client) => browser.connected(client, thread));
  }

  return Object.assign(handle, { upgrade, close: browser.close });
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const port = Number(process.env.VENTICODE_MOCK_PORT || 4189);
  const origin = process.env.VENTICODE_ORIGIN || "http://127.0.0.1:4175";
  const portalOrigin = process.env.VENTICODE_PORTAL_ORIGIN || "http://127.0.0.1:4174";
  const api = createMock({ origin, portalOrigin });
  const server = createServer(api).on("upgrade", api.upgrade);
  server.listen(port, "127.0.0.1", () => console.log(`Venticode synthetic API on http://127.0.0.1:${port} (app origin ${origin}); sign in at ${origin}/__dev/login`));
  for (const signal of ["SIGINT", "SIGTERM"]) process.once(signal, () => {
    api.close();
    server.closeAllConnections();
    server.close(() => process.exit(0));
  });
}
