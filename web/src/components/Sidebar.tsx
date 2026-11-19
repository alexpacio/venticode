import { useRef, useState } from "react";
import { NavLink, useNavigate } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { Bot as BotIcon, ChevronRight, Folder, FolderPlus, Languages, LogOut, Monitor, Search } from "lucide-react";
import { api } from "../api/client.ts";
import type { Harness, Project, Session } from "../api/client.ts";
import { useBots, useConversations, useHarnesses, useProjects } from "../queries.ts";
import { setLanguage, t, useLanguage } from "../i18n/index.ts";
import venticMark from "../design/assets/ventic-icon.svg";
import { relative } from "./format.ts";

export function Sidebar({ session, onNavigate }: { session: Session; onNavigate: () => void }) {
  const harnesses = useHarnesses();
  const projects = useProjects();
  const conversations = useConversations();
  const bots = useBots();
  const language = useLanguage();
  const [query, setQuery] = useState("");
  const [closed, setClosed] = useState<Set<string>>(new Set());
  const [adding, setAdding] = useState<Harness | null>(null);
  const online = harnesses.data?.filter((h) => h.online).length ?? 0;
  const needle = query.trim().toLowerCase();

  const signOut = async () => {
    const result = await api<{ redirect_url: string }>("/session", "DELETE");
    if (window.parent !== window) {
      window.parent.postMessage({ type: "venticode:signed-out" }, new URL(session.portal_url).origin);
      window.location.replace("/");
      return;
    }
    location.assign(result.redirect_url);
  };

  return (
    <nav className="sidebar" aria-label={t("Inventario harness")}>
      <div className="brand">
        <img className="brand-logo" src={venticMark} alt="" />
        <div className="brand-name">
          <strong>Venticode</strong>
          <span>{session.workspace.name}</span>
        </div>
      </div>
      <label className="search">
        <Search size={15} aria-hidden="true" />
        <span className="sr-only">{t("Cerca conversazioni")}</span>
        <input type="search" value={query} onChange={(e) => setQuery(e.target.value)} placeholder={t("Cerca conversazioni…")} />
      </label>
      <div className="sidebar-scroll">
        <div className="nav-head">
          <h2 className="section-title">{t("Harness · {0}/{1} online", online, harnesses.data?.length ?? 0)}</h2>
          <a className="btn" style={{ minHeight: 30, fontSize: 12, border: 0 }} href={`${session.portal_url}/app/harnesses`} target="_blank" rel="noreferrer">{t("Collega")}</a>
        </div>
        {harnesses.data?.length === 0 && <p className="muted" style={{ padding: "0 16px", fontSize: 12 }}>{t("Nessun harness collegato.")}</p>}
        {harnesses.data?.map((harness) => {
          const open = !closed.has(harness.id) || Boolean(needle);
          const own = (projects.data || []).filter((p) => p.harness_id === harness.id);
          return (
            <div key={harness.id}>
              <div className="harness-row">
                <button className="harness-toggle" aria-expanded={open}
                  onClick={() => setClosed((prev) => { const next = new Set(prev); if (next.has(harness.id)) next.delete(harness.id); else next.add(harness.id); return next; })}>
                  <ChevronRight size={13} className={`chev${open ? " open" : ""}`} aria-hidden="true" />
                  <Monitor size={16} aria-hidden="true" style={{ flex: "none", color: "var(--text-2)" }} />
                  <span className="harness-name">
                    <b>{harness.name}</b>
                    <small>{[harness.host_info?.os, harness.mode === "yolo" ? "YOLO" : t("Consenso"), harness.owned ? null : t("condiviso")].filter(Boolean).join(" · ")}</small>
                  </span>
                  <span className={`status ${harness.online ? "status-online" : "status-offline"}`}>
                    <span className="dot" aria-hidden="true" />{harness.online ? t("online") : t("offline")}
                  </span>
                </button>
                <button className="icon-btn" aria-label={t("Aggiungi progetto su {0}", harness.name)} onClick={() => setAdding(harness)}>
                  <FolderPlus size={15} aria-hidden="true" />
                </button>
              </div>
              {open && (
                <div className="tree">
                  {own.map((project) => {
                    const items = (conversations.data || []).filter((c) => c.project_id === project.id && (!needle || c.title.toLowerCase().includes(needle)));
                    if (needle && !items.length) return null;
                    return (
                      <div key={project.id}>
                        <div className="project-row">
                          <NavLink to={`/p/${project.id}`} className="project-link" onClick={onNavigate}>
                            <Folder size={14} aria-hidden="true" style={{ flex: "none" }} />
                            <b>{project.name}</b>
                            {(project.path !== "." || project.name !== project.root) && <small>{project.path === "." ? project.root : `${project.root}/${project.path}`}</small>}
                          </NavLink>
                        </div>
                        {items.map((c) => (
                          <NavLink key={c.id} to={`/c/${c.id}`} className="tree-link" onClick={onNavigate}>
                            <span>{c.title}</span>
                            <small className={c.state === "idle" ? undefined : "running"}>
                              {c.state === "running" ? t("in corso") : c.state === "waiting" ? t("attende") : c.state === "failed" ? t("errore") : relative(c.updated_at)}
                            </small>
                          </NavLink>
                        ))}
                      </div>
                    );
                  })}
                  {!own.length && !needle && (
                    <button className="btn" style={{ border: 0, justifyContent: "flex-start", color: "var(--muted)", fontSize: 12 }} onClick={() => setAdding(harness)}>
                      {t("Apri una cartella su questo harness…")}
                    </button>
                  )}
                </div>
              )}
            </div>
          );
        })}
        <div className="nav-head" style={{ marginTop: 8 }}>
          <h2 className="section-title">{t("Bot")}</h2>
          <NavLink to="/connections" className="btn" onClick={onNavigate}>{t("Connessioni bot")}</NavLink>
        </div>
        {bots.data?.map((bot) => (
          <NavLink key={bot.id} to={`/bots/${bot.id}`} className="bot-link" onClick={onNavigate}>
            <BotIcon size={16} aria-hidden="true" style={{ flex: "none", color: "var(--text-2)" }} />
            <span>
              <b className="mono" style={{ fontWeight: 500 }}>{bot.handle}</b>
              <small>{bot.name} · {bot.channels.map((c) => c.name).join(", ")}</small>
            </span>
            <span className={`status ${bot.enabled ? "status-online" : "status-offline"}`}><span className="dot" aria-hidden="true" />{bot.enabled ? t("attivo") : t("in pausa")}</span>
          </NavLink>
        ))}
      </div>
      <div className="sidebar-foot">
        <span className="avatar" aria-hidden="true">{session.user.name.split(/\s+/).map((part) => part[0]).join("").slice(0, 2).toUpperCase()}</span>
        <span style={{ flex: 1, minWidth: 0, fontSize: 13, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{session.user.name}</span>
        <button className="icon-btn" aria-label={language === "it" ? "Switch to English" : "Passa all'italiano"} onClick={() => setLanguage(language === "it" ? "en" : "it")}>
          <Languages size={16} aria-hidden="true" />
        </button>
        <button className="icon-btn" aria-label={t("Esci")} onClick={signOut}><LogOut size={16} aria-hidden="true" /></button>
      </div>
      {adding && <AddProject harness={adding} onClose={() => setAdding(null)} onNavigate={onNavigate} />}
    </nav>
  );
}

function AddProject({ harness, onClose, onNavigate }: { harness: Harness; onClose: () => void; onNavigate: () => void }) {
  const client = useQueryClient();
  const navigate = useNavigate();
  const ref = useRef<HTMLDialogElement>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const submit = async (form: FormData) => {
    setBusy(true);
    setError("");
    try {
      const name = String(form.get("name") || "").trim();
      const project = await api<Project>("/projects", "POST", { harness_id: harness.id, root: form.get("root"), path: String(form.get("path") || ".").trim() || ".", ...(name ? { name } : {}) });
      await client.invalidateQueries({ queryKey: ["projects"] });
      onClose();
      onNavigate();
      navigate(`/p/${project.id}`);
    } catch (failure) {
      setError((failure as Error).message);
      setBusy(false);
    }
  };
  return (
    <dialog ref={(node) => { ref.current = node; if (node && !node.open) node.showModal(); }} onClose={onClose} aria-labelledby="add-project-title">
      <form onSubmit={(e) => { e.preventDefault(); void submit(new FormData(e.currentTarget)); }}>
        <h2 id="add-project-title">{t("Nuovo progetto su {0}", harness.name)}</h2>
        <label className="field"><span>{t("Cartella autorizzata")}</span>
          <select className="select" name="root" required defaultValue={harness.roots[0]}>
            {harness.roots.map((root) => <option key={root} value={root}>{root}</option>)}
          </select>
        </label>
        <label className="field"><span>{t("Sottocartella (relativa, . per la radice)")}</span>
          <input className="input mono" name="path" defaultValue="." maxLength={1024} pattern="^(\.|[^/\\][^\\]*)$" />
        </label>
        <label className="field"><span>{t("Nome (facoltativo)")}</span>
          <input className="input" name="name" maxLength={80} />
        </label>
        {error && <p className="error-line" role="alert">{error}</p>}
        <div className="dialog-actions">
          <button type="button" className="btn" onClick={() => ref.current?.close()}>{t("Annulla")}</button>
          <button className="btn btn-primary" disabled={busy}>{t("Aggiungi progetto")}</button>
        </div>
      </form>
    </dialog>
  );
}

