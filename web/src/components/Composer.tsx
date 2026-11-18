import { useState, type ReactNode } from "react";
import { ArrowUp, Square } from "lucide-react";
import { t } from "../i18n/index.ts";

/** Enter sends, Shift+Enter adds a line, Esc interrupts a running turn. */
export function Composer({ onSend, onStop, running, disabled, controls, placeholder }: {
  onSend: (text: string) => Promise<void>; onStop?: () => void; running?: boolean; disabled?: string; controls?: ReactNode; placeholder?: string;
}) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const send = async () => {
    const value = text.trim();
    if (!value || busy || running || disabled) return;
    setBusy(true);
    setError("");
    try { await onSend(value); setText(""); } catch (failure) { setError((failure as Error).message); } finally { setBusy(false); }
  };
  return (
    <div className="composer-wrap">
      <form className="composer" onSubmit={(e) => { e.preventDefault(); void send(); }}>
        <label htmlFor="prompt" className="sr-only">{t("Messaggio per Venticode")}</label>
        <textarea id="prompt" rows={2} value={text} onChange={(e) => setText(e.target.value)} disabled={Boolean(disabled)}
          placeholder={disabled || placeholder || t("Chiedi a Venticode di modificare, eseguire o spiegare qualcosa…")}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) { e.preventDefault(); void send(); }
            if (e.key === "Escape" && running) onStop?.();
          }} />
        {error && <p className="error-line" role="alert" style={{ margin: 0 }}>{error}</p>}
        <div className="composer-bar">
          {controls}
          <span className="hint">{t("⏎ invia · ⇧⏎ a capo · Esc interrompe")}</span>
          {running ? (
            <button type="button" className="send stop" aria-label={t("Interrompi")} onClick={onStop}><Square size={14} fill="currentColor" aria-hidden="true" /></button>
          ) : (
            <button className="send" aria-label={t("Invia")} disabled={!text.trim() || busy || Boolean(disabled)}><ArrowUp size={18} aria-hidden="true" /></button>
          )}
        </div>
      </form>
    </div>
  );
}
