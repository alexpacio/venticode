import { useState, type FormEvent, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import { api, list } from "../api/client.ts";
import type { components } from "../api/schema.ts";
import { useProjects, useSession } from "../queries.ts";
import { t } from "../i18n/index.ts";

import { runtimeLabel } from "./BotView.tsx";

type Connection = components["schemas"]["Connection"];
const providers = { slack: "Slack", telegram: "Telegram", gitlab: "GitLab" };

export function Connections({ menu, admin }: { menu: ReactNode; admin: boolean }) {
  const client = useQueryClient();
  const connections = useQuery({ queryKey: ["connections"], refetchInterval: 15000, queryFn: () => list<Connection>("/connections") });
  const [editing, setEditing] = useState<Connection | "new" | null>(null);
  const refresh = async () => {
    await Promise.all(["connections", "bots"].map(key => client.invalidateQueries({ queryKey: [key] })));
    setEditing(null);
  };
  return <main className="main">
    <header className="topbar">{menu}<h1>{t("Connessioni bot")}</h1></header>
    <div className="page">
      {connections.isError && <p role="alert">{connections.error.message}</p>}
      {connections.isPending && <p role="status">{t("Caricamento…")}</p>}
      {connections.data?.length === 0 && <p>{t("Nessuna connessione configurata.")}</p>}
      {connections.data?.map(c => <ConnectionCard key={c.id} connection={c} admin={admin} refresh={refresh} edit={() => setEditing(c)} />)}
      {admin && !editing && <button className="btn" onClick={() => setEditing("new")}>{t("Aggiungi connessione")}</button>}
      {admin && editing && <SetupForm key={editing === "new" ? editing : editing.id} connection={editing === "new" ? null : editing} done={refresh} cancel={() => setEditing(null)} />}
    </div>
  </main>;
}

function ConnectionCard({ connection: c, admin, refresh, edit }: { connection: Connection; admin: boolean; refresh: () => Promise<void>; edit: () => void }) {
  const [command, setCommand] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const run = async (action: () => Promise<void>) => {
    setBusy(true); setError(""); setCommand("");
    try { await action(); } catch (failure) { setError((failure as Error).message); } finally { setBusy(false); }
  };
  const link = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const external_user_id = new FormData(event.currentTarget).get("identity");
    void run(async () => {
      const proof = await api<{ command: string }>(`/connections/${c.id}/link`, "POST", { external_user_id });
      setCommand(proof.command);
    });
  };
  return <section className="card"><h2>{providers[c.kind]} · {c.destination}</h2>
    <p>{c.status === "connected" ? t("Connesso") : c.status === "pending" ? t("In attesa di autorizzazione") : t("Disconnesso")}</p>
    {admin && c.kind === "slack" && <p>{t("URL per gli eventi Slack")} <code>{c.webhook_url}</code></p>}
    {c.status === "connected" && <>
      <Link className="btn" to={`/bots/${c.id}`}>{t("Apri menzioni")}</Link>
      {c.linked_user_id ? <p>{t("Account collegato: {0}", c.linked_user_id)} <button className="btn" disabled={busy} onClick={() => void run(async () => {
        await api(`/connections/${c.id}/link`, "DELETE"); await refresh();
      })}>{t("Scollega il mio account")}</button></p> : <form onSubmit={link}><label className="field">{t("Il tuo ID utente su {0}", providers[c.kind])}
        <input name="identity" required className="input" maxLength={64} autoComplete="off" /></label>
        <button className="btn" disabled={busy}>{t("Collega il mio account")}</button>
      </form>}
      {command && <p role="status">{t("Invia questo comando dal tuo account entro cinque minuti.")} <code>{command}</code></p>}
    </>}
    {admin && <div className="dialog-actions">{c.status !== "disconnected" && (c.project_id || c.runtime === "kitesurf" || c.runtime === "computer") && <button className="btn" disabled={busy} onClick={edit}>{t("Sostituisci credenziali")}</button>}
      {c.status !== "disconnected" && <button className="btn btn-danger" disabled={busy} onClick={() => {
        if (window.confirm(t("Disconnettere il bot e revocare i collegamenti degli utenti?"))) void run(async () => {
          await api(`/connections/${c.id}`, "DELETE"); await refresh();
        });
      }}>{t("Disconnetti")}</button>}</div>}
    {error && <p role="alert" className="error-line">{error}</p>}
  </section>;
}

function SetupForm({ connection, done, cancel }: { connection: Connection | null; done: () => Promise<void>; cancel: () => void }) {
  const [kind, setKind] = useState<Connection["kind"]>(connection?.kind ?? "slack");
  const projects = useProjects();
  const runtimes = useSession().data?.bot_runtimes ?? ["project"];
  const [runtime, setRuntime] = useState<NonNullable<Connection["runtime"]>>(connection?.runtime ?? "project");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const form = event.currentTarget;
    const data = new FormData(form);
    const credentials = Object.fromEntries((kind === "slack" ? ["client_id", "client_secret", "signing_secret"] : ["token"]).map(key => [key, data.get(key)]));
    setBusy(true); setError("");
    try {
      const result = await api<{ authorize_url?: string }>(connection ? `/connections/${connection.id}` : "/connections", connection ? "PUT" : "POST", {
        kind, destination: connection?.destination ?? data.get("destination"), runtime,
        project_id: connection ? connection.project_id : runtime === "project" ? data.get("project") : null, credentials,
      });
      form.reset();
      if (result.authorize_url) {
        const url = new URL(result.authorize_url);
        if (url.origin !== "https://slack.com" || url.pathname !== "/oauth/v2/authorize") throw new Error(t("URL di autorizzazione non valido."));
        location.assign(url.href);
      } else await done();
    } catch (failure) { setError((failure as Error).message); } finally { setBusy(false); }
  };
  return <form className="card" onSubmit={submit}><h2>{connection ? t("Sostituisci credenziali") : t("Aggiungi connessione")}</h2>
    <label className="field">{t("Servizio")}<select className="select" value={kind} disabled={!!connection} onChange={e => setKind(e.target.value as Connection["kind"])}>
      {Object.entries(providers).map(([key, name]) => <option key={key} value={key}>{name}</option>)}</select></label>
    <label className="field">{kind === "slack" ? t("ID del canale Slack") : kind === "telegram" ? t("ID della chat Telegram") : t("ID numerico del progetto GitLab")}
      <input className="input" name="destination" required disabled={!!connection} defaultValue={connection?.destination} /></label>
    {runtimes.length > 1 && <label className="field">{t("Ambiente del bot")}
      <select className="select" value={runtime} disabled={!!connection} onChange={e => setRuntime(e.target.value as NonNullable<Connection["runtime"]>)}>
        {runtimes.map(mode => <option value={mode} key={mode}>{runtimeLabel(mode)}</option>)}
      </select></label>}
    {runtime === "project" && <label className="field">{t("Progetto")}<select className="select" name="project" required defaultValue={connection?.project_id ?? ""} disabled={!!connection}>
      {projects.data?.map(p => <option key={p.id} value={p.id}>{p.name} · {p.root}/{p.path}</option>)}</select></label>}
    {kind === "slack" ? <>
      <p>{t("URL di reindirizzamento OAuth")} <code>{location.origin}/oauth/slack</code></p>
      <label className="field">{t("ID client")}<input className="input" name="client_id" required autoComplete="off" /></label>
      <label className="field">{t("Segreto client")}<input className="input" name="client_secret" type="password" required autoComplete="new-password" /></label>
      <label className="field">{t("Segreto di firma")}<input className="input" name="signing_secret" type="password" required autoComplete="new-password" /></label>
    </> : <label className="field">{t("Token del bot")}<input className="input" name="token" type="password" required autoComplete="new-password" /></label>}
    {error && <p role="alert" className="error-line">{error}</p>}
    <div className="dialog-actions"><button className="btn" type="button" onClick={cancel}>{t("Annulla")}</button>
      <button className="btn btn-primary" disabled={busy || (!connection && runtime === "project" && !projects.data?.length)}>{t("Collega")}</button></div>
  </form>;
}
