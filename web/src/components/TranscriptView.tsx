import { useEffect, useRef, useState } from "react";
import { useParams } from "react-router-dom";
import { ShieldAlert } from "lucide-react";
import { api } from "../api/client.ts";
import type { ApprovalDecision } from "../api/client.ts";
import type { ToolBlock } from "../transcript.ts";
import { t } from "../i18n/index.ts";

export function ToolActions({ block, harness }: { block: ToolBlock; harness: string }) {
  if (block.approval?.state === "pending") return <Approval id={block.approval.id} operation={block.operation} target={block.target} summary={block.approval.summary} harness={harness} />;
  if (block.interactive && block.status === "running") return <SessionControls block={block} />;
  return null;
}

function SessionControls({ block }: { block: ToolBlock }) {
  const { id } = useParams();
  const [text, setText] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  async function submit(action: "send" | "stop") {
    setBusy(true);
    setError("");
    try {
      await api(`/conversations/${id}/sessions/${block.id}/input`, "POST", { id: crypto.randomUUID(), action, ...(action === "send" ? { text } : {}) });
      setText("");
    } catch (failure) { setError((failure as Error).message); }
    finally { setBusy(false); }
  }
  return <form className="session-controls" onSubmit={e => { e.preventDefault(); void submit("send"); }}>
    <label className="field">{block.interactive === "terminal" ? t("Riga da inviare al terminale") : t("Messaggio per l’agente")}
      <input className="input mono" maxLength={8192} autoComplete="off" value={text} onChange={e => setText(e.target.value)} /></label>
    <div className="approval-actions"><button className="btn" disabled={busy || !text}>{t("Invia")}</button>
      <button className="btn" type="button" disabled={busy} onClick={() => void submit("stop")}>{t("Chiudi sessione")}</button></div>
    {error && <p role="alert" className="error-line">{error}</p>}
  </form>;
}

function Approval({ id, operation, target, summary, harness }: { id: string; operation: string; target: string; summary?: string; harness: string }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const container = useRef<HTMLElement>(null);
  // Focus the section rather than an action button so typing in the composer cannot accidentally approve.
  useEffect(() => {
    const active = document.activeElement;
    if (!active || active === document.body) container.current?.focus();
  }, []);
  const decide = async (decision: ApprovalDecision) => {
    setBusy(true);
    try { await api(`/approvals/${id}`, "POST", { decision }); } catch (failure) { setError((failure as Error).message); setBusy(false); }
  };
  const details = summary && summary !== target ? summary : target;
  return (
    <section ref={container} tabIndex={-1} className="approval" aria-label={t("Richiesta di permesso")}
      onKeyDown={(e) => { const choice = ({ "1": "allow_once", "2": "allow_project", "3": "deny" } as const)[e.key as "1"]; if (choice && !busy) { e.preventDefault(); void decide(choice); } }}>
      <div className="approval-head">
        <ShieldAlert size={18} aria-hidden="true" style={{ color: "var(--accent)" }} />
        <strong>{t("Consentire {0} su {1}?", operation.replace(/_/g, " "), harness || "harness")}</strong>
      </div>
      <div className="code">{operation === "execute" ? "$ " : ""}{details}</div>
      {error && <p className="error-line" role="alert" style={{ margin: 0 }}>{error}</p>}
      <div className="approval-actions">
        <button className="btn btn-primary btn-lg" disabled={busy} onClick={() => void decide("allow_once")}>{t("Consenti una volta")} <span aria-hidden="true" style={{ opacity: .7 }}>1</span></button>
        <button className="btn btn-lg" disabled={busy} onClick={() => void decide("allow_project")}>{t("Consenti sempre in questo progetto")} <span aria-hidden="true" className="muted">2</span></button>
        <button className="btn btn-lg btn-danger" disabled={busy} onClick={() => void decide("deny")}>{t("Nega")} <span aria-hidden="true" className="muted">3</span></button>
      </div>
    </section>
  );
}
