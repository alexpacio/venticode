import { useEffect, useRef, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { LoaderCircle, Pencil, Trash2 } from "lucide-react";
import { api, ApiError } from "../api/client.ts";
import type { Conversation, Harness, PermissionMode } from "../api/client.ts";
import { useConversation, useHarnesses, useModels, useProjects, useTranscript } from "../queries.ts";
import type { Transcript } from "../transcript.ts";
import { t } from "../i18n/index.ts";
import { ErrorBoundary } from "./ErrorBoundary.tsx";
import { Terminal } from "./Terminal.tsx";
import { ToolActions } from "./TranscriptView.tsx";
import { AgentControls } from "./AgentControls.tsx";
import { Composer } from "./Composer.tsx";
import type { ReactNode } from "react";

export const modes = (): Array<[PermissionMode, string]> => [["ask", t("Chiedi prima di modificare")], ["auto_edit", t("Accetta modifiche")], ["plan", t("Solo piano")]];

type UpdateResult = { ok: boolean; error?: string };
type UpdateFn = (
  change: Partial<Pick<Conversation, "model" | "permission_mode" | "title">>,
  options?: { reportError?: boolean },
) => Promise<UpdateResult>;

export function ConversationView({ menu, panelToggle }: { menu: ReactNode; panelToggle?: ReactNode }) {
  const { id = "" } = useParams();
  const conversation = useConversation(id);
  const { transcript, disconnected } = useTranscript(id, Boolean(conversation.data));
  const pending = useRef<{ key: string; id: string } | null>(null);
  const harness = useHarnesses().data?.find((h) => h.id === conversation.data?.harness_id);
  const project = useProjects().data?.find((p) => p.id === conversation.data?.project_id);
  const [actionError, setActionError] = useState("");
  const update = useUpdate(id, setActionError);

  useEffect(() => {
    setActionError("");
    pending.current = null;
  }, [id]);

  if (conversation.isError) {
    const notFound = conversation.error instanceof ApiError && conversation.error.status === 404;
    return <main className="main"><div className="topbar">{menu}</div><div className="empty" role="alert">
      <h1>{notFound ? t("Conversazione non trovata") : t("Impossibile caricare la conversazione")}</h1>
      {!notFound && <p>{conversation.error.message}</p>}
      {!notFound && <button className="btn" onClick={() => void conversation.refetch()}>{t("Riprova")}</button>}
    </div></main>;
  }
  if (!conversation.data)
    return <main className="main"><div className="topbar">{menu}</div><div className="empty" role="status"><LoaderCircle className="spin" aria-hidden="true" /></div></main>;
  const c = conversation.data;
  const running = transcript.hasRunEvent ? transcript.running : (c.state === "running" || c.state === "waiting");
  const send = async (content: string, intent = "message", reference?: string) => {
    setActionError("");
    const key = JSON.stringify([id, content, intent, reference]);
    const request = pending.current?.key === key ? pending.current : { key, id: crypto.randomUUID() };
    pending.current = request;
    const result = await api(`/conversations/${id}/messages`, "POST", {
      id: request.id, content, ...(intent === "message" ? {} : { intent }), reference_id: reference,
    });
    if (pending.current === request) pending.current = null;
    return result;
  };
  const stop = async () => {
    setActionError("");
    try {
      await api(`/conversations/${c.id}/interrupt`, "POST");
    } catch (failure) {
      setActionError((failure as Error).message);
    }
  };
  const projectLabel = project ? (project.path === "." ? project.root : `${project.root}/${project.path}`) : "";

  return (
    <>
      <main className="main">
        <Header key={`header-${c.id}`} conversation={c} harness={harness} projectLabel={projectLabel} menu={menu} panelToggle={panelToggle} running={running} update={update} onError={setActionError} />
        {disconnected && <div className="agent-controls" role="status"><span className="muted" style={{ fontSize: 12 }}>{t("Connessione persa, riconnessione in corso…")}</span></div>}
        {actionError && <div className="agent-controls" role="alert"><span className="error-line">{actionError}</span></div>}
        <ErrorBoundary resetKey={id} inline>
          <Terminal key={`terminal-${id}`} project={project} chat={c} blocks={transcript.blocks} />
        </ErrorBoundary>
        {transcript.blocks.map(block => block.kind === "tool" && <ToolActions key={block.id} block={block} harness={harness?.name || ""} disconnected={disconnected} />)}
        <AgentControls key={`controls-${c.id}`} c={c} transcript={transcript} running={running} send={send} />
        <Composer key={`composer-${c.id}`} placeholder={c.question_id ? t("Rispondi alla domanda…") : t("Scrivi un messaggio, /goal obiettivo o /plan richiesta…")} onSend={async (text) => {
          if (c.question_id) await send(text, "answer", c.question_id);
          else if (text.startsWith("/goal ")) await send(text.slice(6).trim(), "goal");
          else if (text.startsWith("/plan ")) await send(text.slice(6).trim(), "plan");
          else await send(text);
        }} onStop={() => void stop()} running={running}
          disabled={harness && !harness.online && !harness.managed ? t("{0} è offline: avvia venticode-harness per continuare.", harness.name) : undefined}
          controls={<ModeSelect conversation={c} update={update} />} />
      </main>
      <SessionPanel transcript={transcript} harness={harness} projectLabel={projectLabel} />
    </>
  );
}

function Header({ conversation, harness, projectLabel, menu, panelToggle, running, update, onError }: {
  conversation: Conversation; harness?: Harness; projectLabel: string; menu: ReactNode; panelToggle?: ReactNode; running: boolean; update: UpdateFn; onError: (msg: string) => void;
}) {
  const models = useModels();
  const client = useQueryClient();
  const navigate = useNavigate();
  const [renaming, setRenaming] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const remove = async () => {
    if (deleting || !confirm(t("Eliminare la conversazione “{0}”? Trascrizione e output verranno cancellati.", conversation.title))) return;
    setDeleting(true);
    try {
      await api(`/conversations/${conversation.id}`, "DELETE");
      await client.invalidateQueries({ queryKey: ["conversations"] });
      navigate(conversation.project_id ? `/p/${conversation.project_id}` : "/");
    } catch (failure) {
      onError((failure as Error).message);
      setDeleting(false);
    }
  };
  const modelList = models.data ?? [];
  const hasCurrentModel = modelList.some((m) => m.id === conversation.model);
  return (
    <header className="topbar">
      {menu}
      <nav className="crumbs" aria-label={t("Percorso")}>
        {harness?.name && <><span className="mono">{harness.name}</span><span aria-hidden="true">/</span></>}
        {projectLabel && <><span className="mono">{projectLabel}</span><span aria-hidden="true">/</span></>}
        <strong>{conversation.title}</strong>
        {running && <span className="pill pill-warn"><span className="dot" aria-hidden="true" />{conversation.state === "waiting" ? t("Attende te") : t("Al lavoro")}</span>}
        {!running && conversation.state === "failed" && <span className="pill pill-bad"><span className="dot" aria-hidden="true" />{t("Non riuscito")}</span>}
      </nav>
      {conversation.runtime === "computer" && <Link className="btn" to="/computer">{t("Apri il desktop")}</Link>}
      <label className="select-inline">{t("Modello")}
        <select className="mono" value={conversation.model} disabled={running} onChange={(e) => void update({ model: e.target.value })}>
          {!hasCurrentModel && conversation.model && <option value={conversation.model}>{conversation.model}</option>}
          {modelList.map((m) => <option key={m.id} value={m.id}>{m.name}</option>)}
        </select>
      </label>
      <button className="icon-btn" title={t("Rinomina conversazione")} aria-label={t("Rinomina conversazione")} disabled={running} onClick={() => setRenaming(true)}><Pencil size={16} aria-hidden="true" /></button>
      <button className="icon-btn" title={t("Elimina conversazione")} aria-label={t("Elimina conversazione")} disabled={running || deleting} onClick={() => void remove()}><Trash2 size={16} aria-hidden="true" /></button>
      {panelToggle}
      {renaming && <RenameDialog title={conversation.title} onClose={() => setRenaming(false)} onSave={(title) => update({ title }, { reportError: false })} />}
    </header>
  );
}

function RenameDialog({ title, onClose, onSave }: { title: string; onClose: () => void; onSave: (title: string) => Promise<UpdateResult> }) {
  const ref = useRef<HTMLDialogElement>(null);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const save = async (value: string) => {
    const next = value.trim();
    if (!next) return;
    if (next === title.trim()) { onClose(); return; }
    setSaving(true);
    setError("");
    const result = await onSave(next);
    if (result.ok) onClose();
    else {
      setError(result.error || t("Si è verificato un errore imprevisto"));
      setSaving(false);
    }
  };
  return (
    <dialog ref={(node) => { ref.current = node; if (node && !node.open) node.showModal(); }} onClose={onClose} onClick={(e) => { if (e.target === ref.current) ref.current.close(); }} aria-labelledby="rename-conversation">
      <form onSubmit={(event) => {
        event.preventDefault();
        void save(String(new FormData(event.currentTarget).get("title")));
      }}>
        <h2 id="rename-conversation">{t("Rinomina conversazione")}</h2>
        <label className="field"><span>{t("Titolo della conversazione")}</span>
          <input autoFocus className="input" name="title" defaultValue={title} maxLength={200} required />
        </label>
        {error && <p className="error-line" role="alert" style={{ margin: 0 }}>{error}</p>}
        <div className="dialog-actions">
          <button className="btn" type="button" onClick={() => ref.current?.close()}>{t("Annulla")}</button>
          <button className="btn btn-primary" disabled={saving}>{t("Salva")}</button>
        </div>
      </form>
    </dialog>
  );
}

function ModeSelect({ conversation, update }: { conversation: Conversation; update: UpdateFn }) {
  return (
    <label className="select-inline">{t("Modalità")}
      <select value={conversation.permission_mode} onChange={(e) => void update({ permission_mode: e.target.value as PermissionMode })}>
        {modes().map(([value, label]) => <option key={value} value={value}>{label}</option>)}
      </select>
    </label>
  );
}

function useUpdate(conversationId: string, onError: (msg: string) => void): UpdateFn {
  const client = useQueryClient();
  const queueRef = useRef<Promise<unknown>>(Promise.resolve());
  useEffect(() => {
    queueRef.current = Promise.resolve();
  }, [conversationId]);
  return (change, options) => {
    const reportError = options?.reportError ?? true;
    const task = queueRef.current
      .catch(() => {})
      .then(async (): Promise<UpdateResult> => {
        if (reportError) onError("");
        await client.cancelQueries({ queryKey: ["conversation", conversationId] });
        const current = client.getQueryData<Conversation>(["conversation", conversationId]);
        if (!current) {
          const msg = t("Impossibile caricare la conversazione");
          if (reportError) onError(msg);
          return { ok: false, error: msg };
        }
        try {
          const next = await api<Conversation>(`/conversations/${conversationId}`, "PATCH", {
            revision: current.revision,
            ...change,
          });
          await client.cancelQueries({ queryKey: ["conversation", conversationId] });
          client.setQueryData(["conversation", conversationId], next);
          void client.invalidateQueries({ queryKey: ["conversations"] });
          return { ok: true };
        } catch (failure) {
          const msg = (failure as Error).message;
          if (reportError) onError(msg);
          void client.invalidateQueries({ queryKey: ["conversation", conversationId] });
          void client.invalidateQueries({ queryKey: ["conversations"] });
          return { ok: false, error: msg };
        }
      });
    queueRef.current = task;
    return task;
  };
}

function SessionPanel({ transcript, harness, projectLabel }: { transcript: Transcript; harness?: Harness; projectLabel: string }) {
  const ratio = transcript.usage?.context_ratio;
  const pct = ratio !== undefined ? Math.max(0, Math.min(100, Math.round(ratio * 100))) : undefined;
  const usageTitle = transcript.usage
    ? [
        transcript.usage.input_tokens !== undefined ? `in: ${transcript.usage.input_tokens.toLocaleString()}` : null,
        transcript.usage.output_tokens !== undefined ? `out: ${transcript.usage.output_tokens.toLocaleString()}` : null,
      ].filter(Boolean).join(" · ")
    : undefined;
  return (
    <aside className="panel" aria-label={t("Dettagli sessione")}>
      <section>
        <h2 className="section-title">{t("Attività")}</h2>
        {transcript.todos.length === 0 && <p className="muted" style={{ margin: 0 }}>{t("Nessuna attività pianificata.")}</p>}
        {transcript.todos.map((todo, index) => (
          <div key={index} className={`todo ${todo.status}`}>
            <span className="box" aria-hidden="true">{todo.status === "completed" ? "✓" : ""}</span>
            <span><span className="sr-only">{todo.status === "completed" ? t("Completata:") : todo.status === "in_progress" ? t("In corso:") : t("Da fare:")} </span>{todo.content}</span>
          </div>
        ))}
      </section>
      <section>
        <h2 className="section-title">{t("Modifiche")}</h2>
        {transcript.changes.length === 0 ? <p className="muted" style={{ margin: 0 }}>{t("Nessun file modificato.")}</p> : (
          <div className="files">
            {transcript.changes.map((file) => <div key={file.path}><span title={file.path}>{file.path}</span><span><span className="add">+{file.additions}</span> <span className="del">−{file.deletions}</span></span></div>)}
          </div>
        )}
      </section>
      {harness && (
        <section>
          <h2 className="section-title">{t("Harness")}</h2>
          <dl className="kv">
            <dt>{t("Host")}</dt><dd className="mono">{harness.name}</dd>
            {projectLabel && <><dt>{t("Cartella")}</dt><dd className="mono">{projectLabel}</dd></>}
            <dt>{t("Modalità")}</dt><dd>{harness.mode === "yolo" ? t("YOLO · nessuna richiesta") : t("Consenso · chiede per strumento")}</dd>
            <dt>{t("Stato")}</dt><dd>{harness.online ? t("online") : t("offline")}{harness.host_info ? ` · venticode-harness ${harness.host_info.version}` : ""}</dd>
          </dl>
        </section>
      )}
      {!!harness?.mcp_servers?.length && (
        <section>
          <h2 className="section-title">{t("Server MCP")}</h2>
          {harness.mcp_servers.map((server) => (
            <div key={server.name} style={{ display: "flex", alignItems: "center", gap: 10, fontSize: 12 }}>
              <span className="dot" aria-hidden="true" style={{ color: server.status === "running" ? "var(--ok)" : server.status === "auth_required" ? "var(--bad)" : "var(--off)" }} />
              <span className="mono" style={{ flex: 1 }}>{server.name}</span>
              <span className="muted">{server.transport} · {server.status === "running" ? t("{0} strumenti", server.tools) : server.status === "auth_required" ? t("accesso richiesto") : t("fermo")}</span>
            </div>
          ))}
        </section>
      )}
      {pct !== undefined && (
        <section title={usageTitle}>
          <div style={{ display: "flex", justifyContent: "space-between" }}>
            <h2 className="section-title">{t("Contesto")}</h2><span className="muted" style={{ fontSize: 12 }}>{pct}%</span>
          </div>
          <div className="meter" role="progressbar" aria-label={t("Contesto usato")} aria-valuemin={0} aria-valuemax={100} aria-valuenow={pct}>
            <div className={pct >= 90 ? "danger" : pct >= 75 ? "warn" : undefined} style={{ width: `${pct}%` }} />
          </div>
        </section>
      )}
      <p className="panel-foot">{t("Trascrizione e output degli strumenti cifrati su S3; indice, approvazioni e consumi su PostgreSQL.")}</p>
    </aside>
  );
}
