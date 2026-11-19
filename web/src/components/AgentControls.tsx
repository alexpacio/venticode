import { useState } from "react";
import { api, type Conversation } from "../api/client.ts";
import type { Transcript } from "../transcript.ts";
import { t } from "../i18n/index.ts";

export function AgentControls({ c, transcript, running, send }: {
  c: Conversation; transcript: Transcript; running: boolean;
  send: (content: string, intent?: string, reference?: string) => Promise<unknown>;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const question = c.question_id ? transcript.questions.get(c.question_id) : undefined;
  const plan = c.plan_id ? transcript.plans.get(c.plan_id) : undefined;
  const goal = c.goal_id ? transcript.goals.get(c.goal_id) : undefined;
  const act = async (operation: () => Promise<unknown>) => {
    setBusy(true); setError("");
    try { await operation(); } catch (e) { setError((e as Error).message); } finally { setBusy(false); }
  };
  return <div className="agent-controls">
    {goal && <div><strong>{t("Obiettivo attivo")}</strong> {goal.content} <button className="btn" disabled={running || busy}
      onClick={() => void act(() => api(`/conversations/${c.id}/goal/${c.goal_id}`, "DELETE"))}>{t("Annulla obiettivo")}</button></div>}
    {question && <div><strong>{question.content}</strong> {question.options?.map(option => <button className="btn" key={option} disabled={running || busy}
      onClick={() => void act(() => send(option, "answer", c.question_id!))}>{option}</button>)}</div>}
    {plan && <button className="btn btn-primary" disabled={running || busy} onClick={() => void act(() => send(t("Implementa il piano approvato"), "implement_plan", c.plan_id!))}>{t("Approva ed esegui il piano")}</button>}
    {error && <div role="alert" className="error-line">{error}</div>}
  </div>;
}
