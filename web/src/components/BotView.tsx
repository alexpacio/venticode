import { useEffect, useState, type ReactNode } from "react";
import { Link, useParams } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { Bot as BotIcon, LoaderCircle } from "lucide-react";
import { api, ApiError } from "../api/client.ts";
import type { Bot, BotUpdate, Mention, ToolPolicy } from "../api/client.ts";
import { useBot, useHarnesses, useMentions, useSession } from "../queries.ts";
import { t } from "../i18n/index.ts";
import { relative } from "./format.ts";

const channelLabels: Record<string, string> = { slack: "Slack", telegram: "Telegram", gitlab: "GitLab" };
const escapeRegex = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

export function BotView({ menu, admin }: { menu: ReactNode; admin: boolean }) {
  const { id = "" } = useParams();
  const bot = useBot(id);
  const mentions = useMentions(id);
  const runtimes = useSession().data?.bot_runtimes ?? [];
  const harness = useHarnesses().data?.find((h) => h.id === bot.data?.harness_id);
  const client = useQueryClient();
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);
  const [savedNotice, setSavedNotice] = useState(false);

  useEffect(() => {
    setError("");
    setSavedNotice(false);
  }, [id]);

  if (bot.isError) {
    const notFound = bot.error instanceof ApiError && bot.error.status === 404;
    return <main className="main"><div className="topbar">{menu}</div><div className="empty" role="alert">
      <h1>{notFound ? t("Bot non trovato") : t("Impossibile caricare il bot")}</h1>
      {!notFound && <p>{bot.error.message}</p>}
      {!notFound && <button className="btn" onClick={() => void bot.refetch()}>{t("Riprova")}</button>}
    </div></main>;
  }
  if (!bot.data) return <main className="main"><div className="topbar">{menu}</div><div className="empty" role="status"><LoaderCircle className="spin" aria-hidden="true" /></div></main>;
  const b = bot.data;
  const update = async (change: Omit<BotUpdate, "revision">): Promise<boolean> => {
    if (saving) return false;
    setSaving(true);
    setError("");
    setSavedNotice(false);
    try {
      await client.cancelQueries({ queryKey: ["bot", b.id] });
      const next = await api<Bot>(`/bots/${b.id}`, "PATCH", { revision: b.revision, ...change });
      client.setQueryData(["bot", b.id], next);
      void client.invalidateQueries({ queryKey: ["bots"] });
      setSavedNotice(true);
      return true;
    } catch (failure) {
      setError((failure as Error).message);
      void client.invalidateQueries({ queryKey: ["bot", b.id] });
      return false;
    } finally {
      setSaving(false);
    }
  };
  const subtitle = b.runtime === "computer"
    ? t("Computer Ubuntu")
    : b.runtime === "kitesurf"
      ? "Kitesurf"
      : harness?.name && b.root
        ? t("{0} · strumenti su {1} ({2})", b.name, harness.name, b.root)
        : b.name;
  return (
    <main className="main">
      <header className="topbar">
        {menu}
        <BotIcon size={20} aria-hidden="true" style={{ color: "var(--accent)" }} />
        <div style={{ display: "flex", flexDirection: "column", flex: 1, minWidth: 180 }}>
          <h1 className="mono" style={{ margin: 0, fontSize: 17, fontWeight: 600 }}>{b.handle}</h1>
          <span className="muted" style={{ fontSize: 12 }}>{subtitle}</span>
        </div>
        <span className={`pill ${b.enabled ? "pill-ok" : "pill-off"}`}><span className="dot" aria-hidden="true" />{b.enabled ? t("Attivo") : t("In pausa")}</span>
        {b.runtime === "computer" && <Link className="btn" to="/computer">{t("Apri il desktop")}</Link>}
        {admin && <button className="btn" disabled={saving} onClick={() => void update({ enabled: !b.enabled })}>{b.enabled ? t("Metti in pausa") : t("Riattiva")}</button>}
      </header>
      <div className="page">
        {error && <p className="error-line" role="alert" style={{ margin: 0 }}>{error}</p>}
        {savedNotice && !error && <p className="muted" role="status" style={{ margin: 0, fontSize: 12 }}>{t("Salvato")}</p>}
        <div className="page-grid">
          <section className="wide" aria-label={t("Menzioni")}>
            <div style={{ display: "flex", justifyContent: "space-between", gap: 12, flexWrap: "wrap", alignItems: "baseline" }}>
              <h2 style={{ margin: 0, fontSize: 15 }}>{t("Menzioni")}</h2>
              <span className="muted" style={{ fontSize: 12 }}>{t("Approva la bozza per pubblicarla nel canale.")}</span>
            </div>
            {mentions.isPending && <p className="muted" role="status">{t("Caricamento…")}</p>}
            {mentions.isError && <p className="error-line" role="alert">{mentions.error.message}</p>}
            {mentions.data?.length === 0 && <p className="muted">{t("Nessuna menzione di {0} finora.", b.handle)}</p>}
            {mentions.data?.map((m) => <MentionCard key={m.id} bot={b} mention={m} />)}
          </section>
          {admin && <aside className="narrow" aria-label={t("Configurazione bot")}>
            {runtimes.length > 1 && <section className="card">
              <label className="field">{t("Ambiente del bot")}
                <select className="select" disabled={saving} value={b.runtime ?? "project"} onChange={e => void update({ runtime: e.currentTarget.value as BotUpdate["runtime"] })}>
                  {runtimes.filter(mode => mode !== "project" || b.harness_id).map(mode =>
                    <option key={mode} value={mode}>{runtimeLabel(mode)}</option>)}
                </select>
              </label>
            </section>}
            <Settings key={b.id} bot={b} saving={saving} onSave={update} />
          </aside>}
        </div>
      </div>
    </main>
  );
}

export function runtimeLabel(runtime: NonNullable<BotUpdate["runtime"]>) {
  if (runtime === "computer") return t("Computer Ubuntu");
  if (runtime === "kitesurf") return "Kitesurf";
  return t("Progetto");
}

function MentionCard({ bot, mention }: { bot: Bot; mention: Mention }) {
  const client = useQueryClient();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const review = async (decision: "approve" | "discard") => {
    setBusy(true);
    setError("");
    try {
      await api(`/bots/${bot.id}/mentions/${mention.id}/review`, "POST", { decision });
    } catch (failure) {
      setError((failure as Error).message);
    }
    await client.invalidateQueries({ queryKey: ["mentions", bot.id] });
    setBusy(false);
  };
  const stateMap: Record<string, [string, string]> = {
    review: [t("Da rivedere"), "pill-warn"], replied: [t("Risposto"), "pill-ok"], answering: [t("Sta rispondendo"), "pill-warn"],
    sending: [t("Invio in corso"), "pill-warn"], uncertain: [t("Consegna da verificare nel canale"), "pill-warn"],
    discarded: [t("Scartato"), "pill-off"], failed: [t("Non riuscito"), "pill-bad"],
  };
  const state = stateMap[mention.state] ?? [mention.state, "pill-off"];
  // The handle is rendered as a token wherever it is mentioned.
  const parts = mention.text.split(new RegExp(`(${escapeRegex(bot.handle)})`, "g"));
  return (
    <article className={`mention${mention.state === "review" ? " review" : ""}`}>
      <div className="mention-meta">
        <span className={`pill ${state[1]}`}>{state[0]}</span>
        <span>{channelLabels[mention.channel.kind] ?? mention.channel.kind} · {mention.channel.name}</span>
        <time dateTime={mention.created_at} title={new Date(mention.created_at).toLocaleString()}>{relative(mention.created_at)}</time>
      </div>
      <p style={{ margin: 0 }}><strong>{mention.author}</strong>{"  "}{parts.map((part, i) => part === bot.handle ? <span key={i} className="handle">{part}</span> : part)}</p>
      {mention.conversation_id && <Link to={`/c/${mention.conversation_id}`}>{t("Apri conversazione")}</Link>}
      {mention.tool_calls.length > 0 && (
        <div className="calls">
          {mention.tool_calls.map((call, i) => (
            <span key={i}>
              <span aria-hidden="true" style={{ color: call.status === "ok" ? "var(--ok)" : call.status === "held" ? "var(--accent)" : "var(--bad)" }}>● </span>
              {call.operation}  {call.target}{call.status === "held" ? ` · ${t("in attesa di revisione")}` : ""}
            </span>
          ))}
        </div>
      )}
      {mention.reply && (
        <div style={{ display: "flex", gap: 10 }}>
          <span className="brand-mark" aria-hidden="true" style={{ width: 24, height: 24, fontSize: 10 }}>&gt;_</span>
          <p style={{ margin: 0, color: mention.state === "review" ? "var(--text-2)" : undefined }}>
            {mention.state === "review" && <span className="muted">{t("Bozza: ")}</span>}{mention.reply}
          </p>
        </div>
      )}
      {error && <p className="error-line" role="alert" style={{ margin: 0 }}>{error}</p>}
      {mention.state === "review" && (
        <div className="approval-actions">
          <button className="btn btn-primary btn-lg" disabled={busy} onClick={() => void review("approve")}>{t("Approva e pubblica")}</button>
          <button className="btn btn-lg btn-danger" disabled={busy} onClick={() => void review("discard")}>{t("Scarta")}</button>
        </div>
      )}
    </article>
  );
}

function Settings({ bot, saving, onSave }: { bot: Bot; saving: boolean; onSave: (change: Omit<BotUpdate, "revision">) => Promise<boolean> }) {
  const [instructions, setInstructions] = useState(bot.instructions);
  const [rateText, setRateText] = useState(String(bot.rate_limit_per_hour));
  const [instructionsDirty, setInstructionsDirty] = useState(false);
  const [rateDirty, setRateDirty] = useState(false);

  useEffect(() => {
    if (!instructionsDirty) setInstructions(bot.instructions);
  }, [bot.instructions, instructionsDirty]);

  useEffect(() => {
    if (!rateDirty) setRateText(String(bot.rate_limit_per_hour));
  }, [bot.rate_limit_per_hour, rateDirty]);

  const parsedRate = rateText.trim() === "" ? Number.NaN : Number(rateText);
  const validRate = Number.isInteger(parsedRate) && parsedRate >= 1 && parsedRate <= 1000;
  const unchanged = instructions === bot.instructions && validRate && parsedRate === bot.rate_limit_per_hour;

  const policies: Array<[keyof Bot["tools"], string]> = [["read", t("Lettura file")], ["write", t("Scrittura file")], ["execute", t("Comandi")], ["mcp", t("Strumenti MCP")]];
  return (
    <>
      <section className="card">
        <h2>{t("Attivazione")}</h2>
        <label className="check"><input type="checkbox" disabled={saving} checked={bot.triggers.mentions} onChange={(e) => void onSave({ triggers: { ...bot.triggers, mentions: e.target.checked } })} />{t("Menzioni di {0}", bot.handle)}</label>
        <label className="check"><input type="checkbox" disabled={saving} checked={bot.triggers.direct_messages} onChange={(e) => void onSave({ triggers: { ...bot.triggers, direct_messages: e.target.checked } })} />{t("Messaggi diretti")}</label>
      </section>
      <section className="card">
        <div className="policy-row">
          <h2>{t("Canali")}</h2>
          <Link to="/connections" style={{ fontSize: 12 }}>{t("Connessioni bot")}</Link>
        </div>
        {bot.channels.map((c, i) => (
          <div key={i} className="policy-row"><span className="mono" style={{ fontSize: 13 }}>{c.name}</span><span className="muted" style={{ fontSize: 12 }}>{channelLabels[c.kind] ?? c.kind}</span></div>
        ))}
      </section>
      <section className="card">
        <h2>{t("Strumenti")}</h2>
        {policies.map(([key, label]) => (
          <label key={key} className="policy-row">{label}
            <select className="select" disabled={saving} value={bot.tools[key]} onChange={(e) => void onSave({ tools: { ...bot.tools, [key]: e.target.value as ToolPolicy } })}>
              <option value="auto">{t("Automatico")}</option>
              <option value="review">{t("Con revisione")}</option>
              <option value="off">{t("Disattivato")}</option>
            </select>
          </label>
        ))}
        <span className="muted" style={{ fontSize: 11 }}>{t("Il limite massimo resta quello concesso dall'harness.")}</span>
      </section>
      <form className="card" onSubmit={(e) => {
        e.preventDefault();
        if (!validRate) return;
        void onSave({ instructions, rate_limit_per_hour: parsedRate }).then((ok) => {
          if (ok) {
            setInstructionsDirty(false);
            setRateDirty(false);
          }
        });
      }}>
        <label className="field"><span>{t("Istruzioni")}</span>
          <textarea className="textarea" rows={5} maxLength={8000} value={instructions} onChange={(e) => {
            const next = e.target.value;
            setInstructions(next);
            setInstructionsDirty(next !== bot.instructions);
          }} />
        </label>
        <label className="field"><span>{t("Risposte massime per ora")}</span>
          <input className="input" type="number" min={1} max={1000} required value={rateText} onChange={(e) => {
            const next = e.target.value;
            setRateText(next);
            setRateDirty(next !== String(bot.rate_limit_per_hour));
          }} />
        </label>
        <button className="btn btn-light" disabled={saving || !validRate || unchanged}>{t("Salva")}</button>
      </form>
    </>
  );
}
