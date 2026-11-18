import { useEffect, useRef, useState } from "react";
import { ShieldAlert } from "lucide-react";
import { api } from "../api/client.ts";
import type { ApprovalDecision } from "../api/client.ts";
import { diffLines, type Block, type ToolBlock } from "../transcript.ts";
import { t } from "../i18n/index.ts";

const tone: Record<ToolBlock["status"], string> = {
  running: "var(--accent)", waiting: "var(--accent)", ok: "var(--ok)", error: "var(--bad)", denied: "var(--bad)", cancelled: "var(--off)",
};
const statusLabel = (status: ToolBlock["status"]) =>
  ({ running: t("in esecuzione"), waiting: t("attende approvazione"), ok: "", error: t("errore"), denied: t("negato"), cancelled: t("annullato") })[status];

export function TranscriptView({ blocks, harness }: { blocks: Block[]; harness: string }) {
  const end = useRef<HTMLDivElement>(null);
  const count = blocks.length + blocks.reduce((sum, b) => sum + (b.kind === "text" ? b.content.length : b.kind === "tool" ? b.output.length : 0), 0);
  useEffect(() => { end.current?.scrollIntoView({ block: "end" }); }, [count]);
  return (
    <div className="thread" aria-live="polite">
      <div className="thread-inner">
        {blocks.map((block) => {
          if (block.kind === "user") return <div key={block.id} className="msg-user">{block.content}</div>;
          if (block.kind === "text") return <div key={block.id} className="msg-text">{block.content}</div>;
          if (block.kind === "error") return <p key={block.id} className="error-line" role="alert">{block.message}</p>;
          return <Tool key={block.id} block={block} harness={harness} />;
        })}
        <div ref={end} />
      </div>
    </div>
  );
}

function Tool({ block, harness }: { block: ToolBlock; harness: string }) {
  return (
    <div className="tool">
      <div className="tool-head">
        <span className="dot" aria-hidden="true" style={{ color: tone[block.status] }} />
        <b>{block.operation === "mcp_call_tool" ? "mcp" : block.operation}</b>
        <span className="target">{block.target}</span>
        <span className="meta">
          {block.diff ? <><span className="add">+{block.diff.additions}</span> <span className="del">−{block.diff.deletions}</span></> : statusLabel(block.status)}
        </span>
      </div>
      {block.diff && (
        <div className="tool-body" role="group" aria-label={t("Differenze in {0}", block.diff.path)}>
          {diffLines(block.diff.patch).map((line, index) => <div key={index} className={`diff-line diff-${line.kind}`}>{line.text || " "}</div>)}
        </div>
      )}
      {block.output && <div className="tool-body"><pre>{block.output.replace(/\n$/, "")}</pre></div>}
      {block.summary && !block.diff && <div className="tool-result">└ {block.summary}</div>}
      {block.approval?.state === "pending" && <Approval id={block.approval.id} operation={block.operation} target={block.target} harness={harness} />}
    </div>
  );
}

function Approval({ id, operation, target, harness }: { id: string; operation: string; target: string; harness: string }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const first = useRef<HTMLButtonElement>(null);
  useEffect(() => { first.current?.focus(); }, []);
  const decide = async (decision: ApprovalDecision) => {
    setBusy(true);
    try { await api(`/approvals/${id}`, "POST", { decision }); } catch (failure) { setError((failure as Error).message); setBusy(false); }
  };
  return (
    <section className="approval" aria-label={t("Richiesta di permesso")}
      onKeyDown={(e) => { const choice = ({ "1": "allow_once", "2": "allow_project", "3": "deny" } as const)[e.key as "1"]; if (choice && !busy) { e.preventDefault(); void decide(choice); } }}>
      <div className="approval-head">
        <ShieldAlert size={18} aria-hidden="true" style={{ color: "var(--accent)" }} />
        <strong>{t("Consentire {0} su {1}?", operation, harness)}</strong>
      </div>
      <div className="code">{operation === "execute" ? "$ " : ""}{target}</div>
      {error && <p className="error-line" role="alert" style={{ margin: 0 }}>{error}</p>}
      <div className="approval-actions">
        <button ref={first} className="btn btn-primary btn-lg" disabled={busy} onClick={() => decide("allow_once")}>{t("Consenti una volta")} <span aria-hidden="true" style={{ opacity: .7 }}>1</span></button>
        <button className="btn btn-lg" disabled={busy} onClick={() => decide("allow_project")}>{t("Consenti sempre in questo progetto")} <span aria-hidden="true" className="muted">2</span></button>
        <button className="btn btn-lg btn-danger" disabled={busy} onClick={() => decide("deny")}>{t("Nega")} <span aria-hidden="true" className="muted">3</span></button>
      </div>
    </section>
  );
}
