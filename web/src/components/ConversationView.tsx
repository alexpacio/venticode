import { useMemo, useRef, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { LoaderCircle, Pencil, Trash2 } from "lucide-react";
import { api, ApiError } from "../api/client.ts";
import type { Conversation, Harness, PermissionMode } from "../api/client.ts";
import { useConversation, useHarnesses, useModels, useProjects, useTranscript } from "../queries.ts";
import { fold, type Transcript } from "../transcript.ts";
import { t } from "../i18n/index.ts";
import { Terminal } from "./Terminal.tsx";
import { ToolActions } from "./TranscriptView.tsx";
import { AgentControls } from "./AgentControls.tsx";
import { Composer } from "./Composer.tsx";
import type { ReactNode } from "react";

export const modes = (): Array<[PermissionMode, string]> => [["ask", t("Chiedi prima di modificare")], ["auto_edit", t("Accetta modifiche")], ["plan", t("Solo piano")]];

export function ConversationView({ menu }: { menu: ReactNode }) {
  const { id = "" } = useParams();
  const conversation = useConversation(id);
  const events = useTranscript(id);
  const pending = useRef<{ key: string; id: string } | null>(null);
  const transcript = useMemo(() => fold(events), [events]);
  const harness = useHarnesses().data?.find((h) => h.id === conversation.data?.harness_id);
  const project = useProjects().data?.find((p) => p.id === conversation.data?.project_id);

  if (conversation.isError)
    return <main className="main"><div className="topbar">{menu}</div><div className="empty"><h1>{t("Conversazione non trovata")}</h1></div></main>;
  if (!conversation.data)
    return <main className="main"><div className="empty" role="status"><LoaderCircle className="spin" aria-hidden="true" /></div></main>;
  const c = conversation.data;
  const running = transcript.running || c.state === "running" || c.state === "waiting";
  const send = async (content: string, intent = "message", reference?: string) => {
    const key = JSON.stringify([id, content, intent, reference]);
    const request = pending.current?.key === key ? pending.current : { key, id: crypto.randomUUID() };
    pending.current = request;
    const result = await api(`/conversations/${id}/messages`, "POST", {
      id: request.id, content, ...(intent === "message" ? {} : { intent }), reference_id: reference,
    });
    if (pending.current === request) pending.current = null;
    return result;
  };
  const stop = () => void api(`/conversations/${c.id}/interrupt`, "POST");

  return (
    <>
      <main className="main">
        <Header key={c.id} conversation={c} harness={harness} projectLabel={project ? (project.path === "." ? project.root : `${project.root}/${project.path}`) : ""} menu={menu} running={running} />
        <Terminal key={id} project={project} chat={c} blocks={transcript.blocks} />
        {transcript.blocks.map(block => block.kind === "tool" && <ToolActions key={block.id} block={block} harness={harness?.name || ""} />)}
        <AgentControls key={c.id} c={c} events={events} running={running} send={send} />
        <Composer key={c.id} placeholder={c.question_id ? t("Rispondi alla domanda…") : t("Scrivi un messaggio, /goal obiettivo o /plan richiesta…")} onSend={async (text) => {
          if(c.question_id) await send(text,"answer",c.question_id);
          else if(text.startsWith("/goal ")) await send(text.slice(6).trim(),"goal");
          else if(text.startsWith("/plan ")) await send(text.slice(6).trim(),"plan");
          else await send(text);
        }} onStop={stop} running={running}
          disabled={harness && !harness.online ? t("{0} è offline: avvia ventiharness per continuare.", harness.name) : undefined}
          controls={<ModeSelect conversation={c} />} />
      </main>
      <SessionPanel transcript={transcript} harness={harness} projectLabel={project ? `${project.root}/${project.path === "." ? "" : project.path}` : ""} />
    </>
  );
}

function Header({ conversation, harness, projectLabel, menu, running }: { conversation: Conversation; harness?: Harness; projectLabel: string; menu: ReactNode; running: boolean }) {
  const models = useModels();
  const client = useQueryClient();
  const navigate = useNavigate();
  const update = useUpdate(conversation);
  const [renaming, setRenaming] = useState(false);
  const remove = async () => {
    if (!confirm(t("Eliminare la conversazione “{0}”? Trascrizione e output verranno cancellati.", conversation.title))) return;
    await api(`/conversations/${conversation.id}`, "DELETE");
    await client.invalidateQueries({ queryKey: ["conversations"] });
    navigate("/");
  };
  return (
    <header className="topbar">
      {menu}
      <nav className="crumbs" aria-label={t("Percorso")}>
        <span className="mono">{harness?.name}</span><span aria-hidden="true">/</span>
        <span className="mono">{projectLabel}</span><span aria-hidden="true">/</span>
        <strong>{conversation.title}</strong>
        {running && <span className="pill pill-warn"><span className="dot" aria-hidden="true" />{conversation.state === "waiting" ? t("Attende te") : t("Al lavoro")}</span>}
      </nav>
      <label className="select-inline">{t("Modello")}
        <select className="mono" value={conversation.model} onChange={(e) => void update({ model: e.target.value })}>
          {models.data?.map((m) => <option key={m.id} value={m.id}>{m.name}</option>)}
        </select>
      </label>
      <button className="icon-btn" aria-label={t("Rinomina conversazione")} disabled={running} onClick={() => setRenaming(true)}><Pencil size={16} aria-hidden="true" /></button>
      <button className="icon-btn" aria-label={t("Elimina conversazione")} disabled={running} onClick={() => void remove()}><Trash2 size={16} aria-hidden="true" /></button>
      {renaming && <RenameDialog title={conversation.title} onClose={() => setRenaming(false)} onSave={(title) => update({ title })} />}
    </header>
  );
}

function RenameDialog({ title, onClose, onSave }: { title: string; onClose: () => void; onSave: (title: string) => Promise<boolean> }) {
  const [saving, setSaving] = useState(false);
  const save = async (value: string) => {
    const next = value.trim();
    if (!next) return;
    setSaving(true);
    if (await onSave(next)) onClose();
    else setSaving(false);
  };
  return (
    <dialog ref={(node) => { if (node && !node.open) node.showModal(); }} onClose={onClose} aria-labelledby="rename-conversation">
      <form onSubmit={(event) => {
        event.preventDefault();
        void save(String(new FormData(event.currentTarget).get("title")));
      }}>
        <h2 id="rename-conversation">{t("Rinomina conversazione")}</h2>
        <label className="field"><span>{t("Titolo della conversazione")}</span>
          <input className="input" name="title" defaultValue={title} maxLength={200} required />
        </label>
        <div className="dialog-actions">
          <button className="btn" type="button" onClick={onClose}>{t("Annulla")}</button>
          <button className="btn btn-primary" disabled={saving}>{t("Salva")}</button>
        </div>
      </form>
    </dialog>
  );
}

function ModeSelect({ conversation }: { conversation: Conversation }) {
  const update = useUpdate(conversation);
  return (
    <label className="select-inline">{t("Modalità")}
      <select value={conversation.permission_mode} onChange={(e) => void update({ permission_mode: e.target.value as PermissionMode })}>
        {modes().map(([value, label]) => <option key={value} value={value}>{label}</option>)}
      </select>
    </label>
  );
}

function useUpdate(conversation: Conversation) {
  const client = useQueryClient();
  return async (change: Partial<Pick<Conversation, "model" | "permission_mode" | "title">>) => {
    let saved = false;
    try {
      const next = await api<Conversation>(`/conversations/${conversation.id}`, "PATCH", { revision: conversation.revision, ...change });
      client.setQueryData(["conversation", conversation.id], next);
      saved = true;
    } catch (failure) {
      if (!(failure instanceof ApiError && failure.status === 409)) alert((failure as Error).message);
    }
    void client.invalidateQueries({ queryKey: ["conversation", conversation.id] });
    void client.invalidateQueries({ queryKey: ["conversations"] });
    return saved;
  };
}

function SessionPanel({ transcript, harness, projectLabel }: { transcript: Transcript; harness?: Harness; projectLabel: string }) {
  const ratio = transcript.usage?.context_ratio;
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
            <dt>{t("Cartella")}</dt><dd className="mono">{projectLabel}</dd>
            <dt>{t("Modalità")}</dt><dd>{harness.mode === "yolo" ? t("YOLO · nessuna richiesta") : t("Consenso · chiede per strumento")}</dd>
            <dt>{t("Stato")}</dt><dd>{harness.online ? t("online") : t("offline")}{harness.host_info ? ` · ventiharness ${harness.host_info.version}` : ""}</dd>
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
      {ratio !== undefined && (
        <section>
          <div style={{ display: "flex", justifyContent: "space-between" }}>
            <h2 className="section-title">{t("Contesto")}</h2><span className="muted" style={{ fontSize: 12 }}>{Math.round(ratio * 100)}%</span>
          </div>
          <div className="meter" role="progressbar" aria-label={t("Contesto usato")} aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(ratio * 100)}>
            <div style={{ width: `${Math.round(ratio * 100)}%` }} />
          </div>
        </section>
      )}
      <p className="panel-foot">{t("Trascrizione e output degli strumenti cifrati su S3; indice, approvazioni e consumi su PostgreSQL.")}</p>
    </aside>
  );
}
