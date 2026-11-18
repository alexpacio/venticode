import { useState, type ReactNode } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { MessageSquare, Trash2 } from "lucide-react";
import { api } from "../api/client.ts";
import type { Conversation, PermissionMode } from "../api/client.ts";
import { useConversations, useHarnesses, useProjects } from "../queries.ts";
import { t } from "../i18n/index.ts";
import { Composer } from "./Composer.tsx";
import { modes } from "./ConversationView.tsx";
import { relative } from "./format.ts";

export function ProjectView({ menu }: { menu: ReactNode }) {
  const { id = "" } = useParams();
  const project = useProjects().data?.find((p) => p.id === id);
  const harness = useHarnesses().data?.find((h) => h.id === project?.harness_id);
  const conversations = (useConversations().data || []).filter((c) => c.project_id === id);
  const client = useQueryClient();
  const navigate = useNavigate();
  const [mode, setMode] = useState<PermissionMode>("ask");
  if (!project) return <main className="main"><div className="topbar">{menu}</div><div className="empty"><h1>{t("Progetto non trovato")}</h1></div></main>;

  const start = async (content: string) => {
    const conversation = await api<Conversation>("/conversations", "POST", { project_id: project.id, permission_mode: mode });
    // The conversation exists even if the first message fails; open it so the user can retry.
    navigate(`/c/${conversation.id}`);
    await client.invalidateQueries({ queryKey: ["conversations"] });
    await api(`/conversations/${conversation.id}/messages`, "POST", { id: crypto.randomUUID(), content });
  };
  const remove = async () => {
    if (!confirm(t("Eliminare il progetto “{0}” e tutte le sue conversazioni? I file sull'harness non vengono toccati.", project.name))) return;
    try {
      await api(`/projects/${project.id}`, "DELETE");
      await Promise.all([client.invalidateQueries({ queryKey: ["projects"] }), client.invalidateQueries({ queryKey: ["conversations"] })]);
      navigate("/");
    } catch (failure) { alert((failure as Error).message); }
  };
  const folder = project.path === "." ? project.root : `${project.root}/${project.path}`;
  return (
    <main className="main">
      <header className="topbar">
        {menu}
        <nav className="crumbs" aria-label={t("Percorso")}>
          <span className="mono">{harness?.name}</span><span aria-hidden="true">/</span><strong className="mono">{folder}</strong>
        </nav>
        <button className="icon-btn" aria-label={t("Elimina progetto")} onClick={() => void remove()}><Trash2 size={16} aria-hidden="true" /></button>
      </header>
      <div className="page">
        <div style={{ maxWidth: 820, margin: "0 auto", display: "flex", flexDirection: "column", gap: 16 }}>
          <h1 style={{ margin: "24px 0 0", fontSize: 24 }}>{project.name}</h1>
          <p className="muted" style={{ margin: 0 }}>{t("Nuova conversazione in {0} su {1}.", folder, harness?.name || "")}</p>
          {conversations.length > 0 && (
            <>
              <h2 className="section-title" style={{ marginTop: 16 }}>{t("Conversazioni")}</h2>
              <div className="list">
                {conversations.map((c) => (
                  <Link key={c.id} to={`/c/${c.id}`}><MessageSquare size={15} aria-hidden="true" className="muted" />{c.title}<small>{relative(c.updated_at)}</small></Link>
                ))}
              </div>
            </>
          )}
        </div>
      </div>
      <Composer onSend={start} placeholder={t("Descrivi il lavoro da fare in questo progetto…")}
        disabled={harness && !harness.online ? t("{0} è offline: avvia ventiharness per continuare.", harness.name) : undefined}
        controls={
          <label className="select-inline">{t("Modalità")}
            <select value={mode} onChange={(e) => setMode(e.target.value as PermissionMode)}>
              {modes().map(([value, label]) => <option key={value} value={value}>{label}</option>)}
            </select>
          </label>
        } />
    </main>
  );
}
