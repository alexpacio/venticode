import { useRef, useState, type ReactNode } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { Trash2 } from "lucide-react";
import { api } from "../api/client.ts";
import type { Conversation, PermissionMode } from "../api/client.ts";
import { useHarnesses, useProjects } from "../queries.ts";
import { t } from "../i18n/index.ts";
import { Composer } from "./Composer.tsx";
import { modes } from "./ConversationView.tsx";
import { Terminal } from "./Terminal.tsx";

export function ProjectView({ menu }: { menu: ReactNode }) {
  const { id = "" } = useParams();
  const projects = useProjects();
  const project = projects.data?.find((p) => p.id === id);
  const harness = useHarnesses().data?.find((h) => h.id === project?.harness_id);

  const client = useQueryClient();
  const navigate = useNavigate();
  const [mode, setMode] = useState<PermissionMode>("ask");
  const firstTurn = useRef<{project:string; chat:string; content:string; messageId:string} | null>(null);
  if (projects.isPending) return <main className="main"><Terminal notice={t("Caricamento…")} /></main>;
  if (!project) return <main className="main"><div className="topbar">{menu}</div><div className="empty"><h1>{t("Progetto non trovato")}</h1></div></main>;

  const start = async (content: string) => {
    if (firstTurn.current?.project !== project.id) {
      const chat = await api<Conversation>("/conversations", "POST", { project_id: project.id, permission_mode: mode });
      firstTurn.current = {project:project.id, chat:chat.id, content, messageId:crypto.randomUUID()};
      client.setQueryData(["conversation",chat.id],chat);
      void client.invalidateQueries({queryKey:["conversations"]});
    }
    const turn = firstTurn.current;
    if (turn.content !== content) {
      turn.content = content;
      turn.messageId = crypto.randomUUID();
    }
    await api(`/conversations/${turn.chat}/messages`, "POST", {id:turn.messageId,content});
    firstTurn.current = null;
    navigate(`/c/${turn.chat}`);
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
      <Terminal project={project} />
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
