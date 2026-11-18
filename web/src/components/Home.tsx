import type { ReactNode } from "react";
import { Link } from "react-router-dom";
import { ExternalLink, MessageSquare } from "lucide-react";
import type { Session } from "../api/client.ts";
import { useConversations, useHarnesses, useProjects } from "../queries.ts";
import { t } from "../i18n/index.ts";
import { relative } from "./format.ts";

export function Home({ session, menu }: { session: Session; menu: ReactNode }) {
  const harnesses = useHarnesses();
  const projects = useProjects().data || [];
  const recent = (useConversations().data || []).slice(0, 8);
  const label = (projectId: string) => {
    const p = projects.find((x) => x.id === projectId);
    return p ? p.name : "";
  };
  return (
    <main className="main">
      <header className="topbar">{menu}<span className="crumbs"><strong>{session.workspace.name}</strong></span></header>
      <div className="page">
        {harnesses.data?.length === 0 ? (
          <div className="empty">
            <h1>{t("Collega il primo harness")}</h1>
            <p>{t("Venticode esegue file, comandi, terminali e server MCP sulle tue macchine tramite ventiharness. Abbina una macchina dal portale, poi avvia il demone:")}</p>
            <div className="code">ventiharness up</div>
            <a className="btn btn-primary btn-lg" href={`${session.portal_url}/app/harnesses`} target="_blank" rel="noreferrer">{t("Apri gli harness nel portale")}<ExternalLink size={14} aria-hidden="true" /></a>
          </div>
        ) : (
          <div style={{ maxWidth: 820, margin: "0 auto", display: "flex", flexDirection: "column", gap: 16 }}>
            <h1 style={{ margin: "24px 0 0", fontSize: 24 }}>{t("Bentornato, {0}", session.user.name.split(" ")[0])}</h1>
            <p className="muted" style={{ margin: 0 }}>{t("Scegli un progetto a sinistra per iniziare una conversazione, oppure riprendi da qui.")}</p>
            {recent.length > 0 && (
              <>
                <h2 className="section-title" style={{ marginTop: 16 }}>{t("Recenti")}</h2>
                <div className="list">
                  {recent.map((c) => (
                    <Link key={c.id} to={`/c/${c.id}`}>
                      <MessageSquare size={15} aria-hidden="true" className="muted" />
                      <span>{c.title} <span className="muted">· {label(c.project_id)}</span></span>
                      <small>{relative(c.updated_at)}</small>
                    </Link>
                  ))}
                </div>
              </>
            )}
          </div>
        )}
      </div>
    </main>
  );
}
