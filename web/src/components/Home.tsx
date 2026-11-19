import type { ReactNode } from "react";
import { Navigate, useNavigate } from "react-router-dom";
import type { Session } from "../api/client.ts";
import { useHarnesses, useProjects } from "../queries.ts";
import { t } from "../i18n/index.ts";
import { Terminal } from "./Terminal.tsx";
export function Home({ session, menu }: { session: Session; menu: ReactNode }) {
  const projects = useProjects();
  const harnesses = useHarnesses();
  const navigate = useNavigate();
  if (projects.data?.length === 1)
    return <Navigate replace to={"/p/" + projects.data[0].id} />;
  const options = projects.data || [];
  const notice = projects.isPending
    ? t("Caricamento…")
    : projects.error?.message ||
      (session.standalone && !harnesses.data?.some((h) => h.online)
        ? t("Avvia il runner Venticode per collegare una cartella di progetto.")
        : options.length
        ? t("Scegli un progetto per iniziare.")
        : harnesses.data?.length
          ? t("Seleziona Aggiungi progetto accanto a un harness.")
          : t("Collega il primo harness"));
  return (
    <main className="main">
      <header className="topbar">
        {menu}
        <strong>{session.workspace.name}</strong>
        {options.length > 0 && (
          <label className="select-inline">
            {t("Progetto")}
            <select value="" onChange={(e) => navigate("/p/" + e.target.value)}>
              <option value="" disabled>
                {t("Scegli un progetto")}
              </option>
              {options.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name} · {p.path === "." ? p.root : `${p.root}/${p.path}`}
                </option>
              ))}
            </select>
          </label>
        )}
        {!session.standalone && !harnesses.isPending && !harnesses.data?.length && (
          <a className="btn" href={session.portal_url.replace(/\/+$/, "") + "/app/harnesses"} target={window.parent === window ? "_self" : "_blank"} rel="noreferrer">
            {t("Apri gli harness nel portale")}
          </a>
        )}
      </header>
      <Terminal notice={notice} />
    </main>
  );
}
