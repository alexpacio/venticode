import { useState } from "react";
import { api, type Conversation, type TranscriptEvent } from "../api/client.ts";
import { t } from "../i18n/index.ts";

export function AgentControls({ c, events, running, send }: {
  c: Conversation; events: TranscriptEvent[]; running: boolean;
  send: (content: string, intent?: string, reference?: string) => Promise<unknown>;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const latest = (kind: string, id?: string | null) => id ? events.find(e => e.type === kind && (e.reference_id === id || e.message_id === id)) : undefined;
  const question = latest("question", c.question_id);
  const plan = latest("plan", c.plan_id);
  const goal = latest("user_message", c.goal_id);
  const act = async (operation: () => Promise<unknown>) => {
    setBusy(true); setError("");
    try { await operation(); } catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };
  return <div className="agent-controls">
    {goal && <div><strong>{t("Obiettivo attivo")}</strong> {goal.content} <button className="btn" disabled={running || busy}
      onClick={() => void act(() => api(`/conversations/${c.id}/goal/${c.goal_id}`, "DELETE"))}>{t("Annulla obiettivo")}</button></div>}
    {question && <div><strong>{question.content}</strong> {question.options?.map(option => <button className="btn" key={option} disabled={running || busy}
      onClick={() => void act(() => send(option, "answer", c.question_id!))}>{option}</button>)}</div>}
    {plan && <button className="btn" disabled={running || busy} onClick={() => void act(() => send(t("Implementa il piano approvato"), "implement_plan", c.plan_id!))}>{t("Approva ed esegui il piano")}</button>}
    {error && <div role="alert" className="error-line">{error}</div>}
  </div>;
}
