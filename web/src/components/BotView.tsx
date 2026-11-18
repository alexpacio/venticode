import { useState, type ReactNode } from "react";
import { useParams } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { Bot as BotIcon, LoaderCircle } from "lucide-react";
import { api } from "../api/client.ts";
import type { Bot, BotUpdate, Mention, ToolPolicy } from "../api/client.ts";
import { useBot, useHarnesses, useMentions } from "../queries.ts";
import { t } from "../i18n/index.ts";
import { relative } from "./format.ts";

export function BotView({ menu }: { menu: ReactNode }) {
  const { id = "" } = useParams();
  const bot = useBot(id);
  const mentions = useMentions(id);
  const harness = useHarnesses().data?.find((h) => h.id === bot.data?.harness_id);
  const client = useQueryClient();
  const [error, setError] = useState("");
  if (bot.isError) return <main className="main"><div className="topbar">{menu}</div><div className="empty"><h1>{t("Bot non trovato")}</h1></div></main>;
  if (!bot.data) return <main className="main"><div className="empty" role="status"><LoaderCircle className="spin" aria-hidden="true" /></div></main>;
  const b = bot.data;
  const update = async (change: Omit<BotUpdate, "revision">) => {
    setError("");
    try {
      const next = await api<Bot>(`/bots/${b.id}`, "PATCH", { revision: b.revision, ...change });
      client.setQueryData(["bot", b.id], next);
      void client.invalidateQueries({ queryKey: ["bots"] });
    } catch (failure) {
      setError((failure as Error).message);
      void client.invalidateQueries({ queryKey: ["bot", b.id] });
    }
  };
  return (
    <main className="main">
      <header className="topbar">
        {menu}
        <BotIcon size={20} aria-hidden="true" style={{ color: "var(--accent)" }} />
        <div style={{ display: "flex", flexDirection: "column", flex: 1, minWidth: 180 }}>
          <h1 className="mono" style={{ margin: 0, fontSize: 17, fontWeight: 600 }}>{b.handle}</h1>
          <span className="muted" style={{ fontSize: 12 }}>{t("{0} · strumenti su {1} ({2})", b.name, harness?.name || "", b.root)}</span>
        </div>
        <span className={`pill ${b.enabled ? "pill-ok" : "pill-off"}`}><span className="dot" aria-hidden="true" />{b.enabled ? t("Attivo") : t("In pausa")}</span>
        <button className="btn" onClick={() => void update({ enabled: !b.enabled })}>{b.enabled ? t("Metti in pausa") : t("Riattiva")}</button>
      </header>
      <div className="page">
        {error && <p className="error-line" role="alert">{error}</p>}
        <div className="page-grid">
          <section className="wide" aria-label={t("Menzioni")}>
            <div style={{ display: "flex", justifyContent: "space-between", gap: 12, flexWrap: "wrap", alignItems: "baseline" }}>
              <h2 style={{ margin: 0, fontSize: 15 }}>{t("Menzioni")}</h2>
              <span className="muted" style={{ fontSize: 12 }}>{t("Le risposte in sola lettura partono subito · le scritture attendono revisione")}</span>
            </div>
            {mentions.data?.length === 0 && <p className="muted">{t("Nessuna menzione di {0} finora.", b.handle)}</p>}
            {mentions.data?.map((m) => <MentionCard key={m.id} bot={b} mention={m} />)}
          </section>
          <aside className="narrow" aria-label={t("Configurazione bot")}>
            <Settings bot={b} onSave={update} />
          </aside>
        </div>
      </div>
    </main>
  );
}

function MentionCard({ bot, mention }: { bot: Bot; mention: Mention }) {
  const client = useQueryClient();
  const [busy, setBusy] = useState(false);
  const review = async (decision: "approve" | "discard") => {
    setBusy(true);
    try { await api(`/bots/${bot.id}/mentions/${mention.id}/review`, "POST", { decision }); } catch (failure) { alert((failure as Error).message); }
    await client.invalidateQueries({ queryKey: ["mentions", bot.id] });
    setBusy(false);
  };
  const state = {
    review: [t("Da rivedere"), "pill-warn"], replied: [t("Risposto"), "pill-ok"], answering: [t("Sta rispondendo"), "pill-warn"],
    discarded: [t("Scartato"), "pill-off"], failed: [t("Non riuscito"), "pill-bad"],
  }[mention.state];
  // The handle is rendered as a token wherever it is mentioned.
  const parts = mention.text.split(new RegExp(`(${bot.handle})`, "g"));
  return (
    <article className={`mention${mention.state === "review" ? " review" : ""}`}>
      <div className="mention-meta">
        <span className={`pill ${state[1]}`}>{state[0]}</span>
        <span>{mention.channel.kind === "gitlab" ? "GitLab" : mention.channel.kind === "slack" ? "Slack" : "Telegram"} · {mention.channel.name}</span>
        <time dateTime={mention.created_at}>{relative(mention.created_at)}</time>
      </div>
      <p style={{ margin: 0 }}><strong>{mention.author}</strong>{"  "}{parts.map((part, i) => part === bot.handle ? <span key={i} className="handle">{part}</span> : part)}</p>
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
          <span className="brand-mark" aria-hidden="true" style={{ width: 24, height: 24, fontSize: 10, borderRadius: 6 }}>&gt;_</span>
          <p style={{ margin: 0, color: mention.state === "review" ? "var(--text-2)" : undefined }}>
            {mention.state === "review" && <span className="muted">{t("Bozza: ")}</span>}{mention.reply}
          </p>
        </div>
      )}
      {mention.state === "review" && (
        <div className="approval-actions">
          <button className="btn btn-primary btn-lg" disabled={busy} onClick={() => void review("approve")}>{t("Approva e pubblica")}</button>
          <button className="btn btn-lg btn-danger" disabled={busy} onClick={() => void review("discard")}>{t("Scarta")}</button>
        </div>
      )}
    </article>
  );
}

function Settings({ bot, onSave }: { bot: Bot; onSave: (change: Omit<BotUpdate, "revision">) => Promise<void> }) {
  const [instructions, setInstructions] = useState(bot.instructions);
  const [rate, setRate] = useState(bot.rate_limit_per_hour);
  const policies: Array<[keyof Bot["tools"], string]> = [["read", t("Lettura file")], ["write", t("Scrittura file")], ["execute", t("Comandi")], ["mcp", t("Strumenti MCP")]];
  return (
    <>
      <section className="card">
        <h2>{t("Attivazione")}</h2>
        <label className="check"><input type="checkbox" checked={bot.triggers.mentions} onChange={(e) => void onSave({ triggers: { ...bot.triggers, mentions: e.target.checked } })} />{t("Menzioni di {0}", bot.handle)}</label>
        <label className="check"><input type="checkbox" checked={bot.triggers.direct_messages} onChange={(e) => void onSave({ triggers: { ...bot.triggers, direct_messages: e.target.checked } })} />{t("Messaggi diretti")}</label>
      </section>
      <section className="card">
        <h2>{t("Canali")}</h2>
        {bot.channels.map((c, i) => (
          <div key={i} className="policy-row"><span className="mono" style={{ fontSize: 13 }}>{c.name}</span><span className="muted" style={{ fontSize: 12 }}>{c.kind}</span></div>
        ))}
      </section>
      <section className="card">
        <h2>{t("Strumenti")}</h2>
        {policies.map(([key, label]) => (
          <label key={key} className="policy-row">{label}
            <select className="select" value={bot.tools[key]} onChange={(e) => void onSave({ tools: { ...bot.tools, [key]: e.target.value as ToolPolicy } })}>
              <option value="auto">{t("Automatico")}</option>
              <option value="review">{t("Con revisione")}</option>
              <option value="off">{t("Disattivato")}</option>
            </select>
          </label>
        ))}
        <span className="muted" style={{ fontSize: 11 }}>{t("Il limite massimo resta quello concesso dall'harness.")}</span>
      </section>
      <form className="card" onSubmit={(e) => { e.preventDefault(); void onSave({ instructions, rate_limit_per_hour: rate }); }}>
        <label className="field"><span>{t("Istruzioni")}</span>
          <textarea className="textarea" rows={5} maxLength={8000} value={instructions} onChange={(e) => setInstructions(e.target.value)} />
        </label>
        <label className="field"><span>{t("Risposte massime per ora")}</span>
          <input className="input" type="number" min={1} max={1000} required value={rate} onChange={(e) => setRate(Number(e.target.value))} />
        </label>
        <button className="btn btn-light" disabled={instructions === bot.instructions && rate === bot.rate_limit_per_hour}>{t("Salva")}</button>
      </form>
    </>
  );
}
