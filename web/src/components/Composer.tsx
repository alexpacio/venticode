import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { ArrowUp, LoaderCircle, Square } from "lucide-react";
import { t } from "../i18n/index.ts";

/** Enter sends, Shift+Enter adds a line, Esc interrupts a running turn. */
export function Composer({ onSend, onStop, running, disabled, controls, placeholder }: {
  onSend: (text: string) => Promise<void>; onStop?: () => void; running?: boolean; disabled?: string; controls?: ReactNode; placeholder?: string;
}) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const textarea = useRef<HTMLTextAreaElement>(null);

  useLayoutEffect(() => {
    const el = textarea.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(240, Math.max(48, el.scrollHeight))}px`;
  }, [text]);

  useEffect(() => {
    if (!running || !onStop) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !document.querySelector("dialog[open]")) {
        onStop();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [running, onStop]);

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
        <textarea ref={textarea} id="prompt" rows={2} maxLength={100000} value={text} onChange={(e) => setText(e.target.value)} disabled={Boolean(disabled)}
          placeholder={disabled || placeholder || t("Chiedi a Venticode di modificare, eseguire o spiegare qualcosa…")}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) { e.preventDefault(); void send(); }
          }} />
        {error && <p className="error-line" role="alert" style={{ margin: 0 }}>{error}</p>}
        <div className="composer-bar">
          {controls}
          <span className="hint">{t("⏎ invia · ⇧⏎ a capo · Esc interrompe")}</span>
          {running ? (
            <button type="button" className="send stop" title={t("Interrompi")} aria-label={t("Interrompi")} onClick={onStop}><Square size={14} fill="currentColor" aria-hidden="true" /></button>
          ) : (
            <button className="send" title={t("Invia")} aria-label={t("Invia")} disabled={!text.trim() || busy || Boolean(disabled)}>
              {busy ? <LoaderCircle size={16} className="spin" aria-hidden="true" /> : <ArrowUp size={18} aria-hidden="true" />}
            </button>
          )}
        </div>
      </form>
    </div>
  );
}
