import { useEffect, useRef, useState, type ReactNode } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { Trash2 } from "lucide-react";
import { api } from "../api/client.ts";
import type { Conversation, PermissionMode } from "../api/client.ts";
import { useHarnesses, useModels, useProjects } from "../queries.ts";
import { t } from "../i18n/index.ts";
import { Composer } from "./Composer.tsx";
import { modes } from "./ConversationView.tsx";
import { Terminal } from "./Terminal.tsx";

export function ProjectView({ menu }: { menu: ReactNode }) {
  const { id = "" } = useParams();
  const projects = useProjects();
  const models = useModels();
  const project = projects.data?.find((p) => p.id === id);
  const harness = useHarnesses().data?.find((h) => h.id === project?.harness_id);

  const client = useQueryClient();
  const navigate = useNavigate();
  const [mode, setMode] = useState<PermissionMode>("ask");
  const [model, setModel] = useState("");
  const firstTurn = useRef<{ project: string; chat: string; key: string; messageId: string } | null>(null);

  useEffect(() => {
    setMode("ask");
    firstTurn.current = null;
  }, [id]);

  if (projects.isPending) return <main className="main"><div className="topbar">{menu}</div><Terminal notice={t("Caricamento…")} /></main>;
  if (projects.isError) return <main className="main"><div className="topbar">{menu}</div><div className="empty" role="alert">
    <h1>{t("Impossibile caricare il progetto")}</h1>
    <p>{projects.error.message}</p>
    <button className="btn" onClick={() => void projects.refetch()}>{t("Riprova")}</button>
  </div></main>;
  if (!project) return <main className="main"><div className="topbar">{menu}</div><div className="empty"><h1>{t("Progetto non trovato")}</h1></div></main>;

  const start = async (raw: string) => {
    let content = raw;
    let intent: "message" | "goal" | "plan" = "message";
    if (raw.startsWith("/goal ")) { content = raw.slice(6).trim(); intent = "goal"; }
    else if (raw.startsWith("/plan ")) { content = raw.slice(6).trim(); intent = "plan"; }
    const turnKey = JSON.stringify([content, intent]);
    if (firstTurn.current?.project !== project.id) {
      const chat = await api<Conversation>("/conversations", "POST", {
        project_id: project.id,
        permission_mode: intent === "plan" ? "plan" : mode,
        ...(model ? { model } : {}),
      });
      firstTurn.current = { project: project.id, chat: chat.id, key: turnKey, messageId: crypto.randomUUID() };
      client.setQueryData(["conversation", chat.id], chat);
      void client.invalidateQueries({ queryKey: ["conversations"] });
    }
    const turn = firstTurn.current;
    if (turn.key !== turnKey) {
      turn.key = turnKey;
      turn.messageId = crypto.randomUUID();
    }
    await api(`/conversations/${turn.chat}/messages`, "POST", {
      id: turn.messageId,
      content,
      ...(intent === "message" ? {} : { intent }),
    });
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
  const defaultModel = models.data?.find((m) => m.default)?.id || models.data?.[0]?.id || "";
  return (
    <main className="main">
      <header className="topbar">
        {menu}
        <nav className="crumbs" aria-label={t("Percorso")}>
          {harness?.name && <><span className="mono">{harness.name}</span><span aria-hidden="true">/</span></>}
          <strong className="mono">{folder}</strong>
        </nav>
        {!!models.data?.length && (
          <label className="select-inline">{t("Modello")}
            <select className="mono" value={model || defaultModel} onChange={(e) => setModel(e.target.value)}>
              {models.data.map((m) => <option key={m.id} value={m.id}>{m.name}</option>)}
            </select>
          </label>
        )}
        <button className="icon-btn" title={t("Elimina progetto")} aria-label={t("Elimina progetto")} onClick={() => void remove()}><Trash2 size={16} aria-hidden="true" /></button>
      </header>
      <Terminal project={project} />
      <Composer key={project.id} onSend={start} placeholder={t("Descrivi il lavoro da fare in questo progetto…")}
        disabled={harness && !harness.online && !harness.managed ? t("{0} è offline: avvia venticode-harness per continuare.", harness.name) : undefined}
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
