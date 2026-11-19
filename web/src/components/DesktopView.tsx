import { type ReactNode, useEffect, useRef, useState } from "react";
import { LoaderCircle } from "lucide-react";
import { base } from "../api/client.ts";
import { useComputerStatus } from "../queries.ts";
import { t } from "../i18n/index.ts";
import { BrowserScreen, type BrowserFrame } from "./BrowserScreen.tsx";
import { decodeFrame } from "./BrowserView.tsx";
import { ErrorBoundary } from "./ErrorBoundary.tsx";

export function DesktopView({ menu }: { menu: ReactNode }) {
  const status = useComputerStatus(true);
  const socket = useRef<WebSocket | null>(null);
  const startTimeout = useRef<number | undefined>(undefined);
  const lastMove = useRef(0);
  const control = useRef<HTMLButtonElement>(null);
  const [connected, setConnected] = useState(false);
  const [frame, setFrame] = useState<BrowserFrame | null>(null);
  const [human, setHuman] = useState(false);
  const [changingControl, setChangingControl] = useState(false);
  const [error, setError] = useState("");

  const clearStartTimeout = () => {
    if (startTimeout.current !== undefined) {
      window.clearTimeout(startTimeout.current);
      startTimeout.current = undefined;
    }
  };

  useEffect(() => () => {
    clearStartTimeout();
    const ws = socket.current;
    socket.current = null;
    ws?.close();
  }, []);

  function close(failed = false, message = "") {
    clearStartTimeout();
    const ws = socket.current;
    socket.current = null;
    if (ws?.readyState === WebSocket.OPEN && !failed) {
      try { ws.send(JSON.stringify({ type: "end" })); } catch { /* closing */ }
    }
    ws?.close();
    setConnected(false);
    setHuman(false);
    setChangingControl(false);
    setFrame(null);
    setError(failed ? (message || t("Desktop non disponibile.")) : "");
  }

  function start() {
    if (socket.current) return;
    setError("");
    setConnected(true);
    const url = new URL(`${base}/computer/screen`, location.origin);
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    const ws = new WebSocket(url);
    socket.current = ws;
    ws.binaryType = "arraybuffer";
    let ready = false;
    clearStartTimeout();
    startTimeout.current = window.setTimeout(() => {
      startTimeout.current = undefined;
      if (socket.current === ws && !ready) close(true);
    }, 90_000);
    ws.onerror = ws.onclose = () => {
      if (socket.current === ws) close(true);
    };
    ws.onmessage = event => {
      if (socket.current !== ws) return;
      try {
        if (event.data instanceof ArrayBuffer) {
          const next = decodeFrame(event.data);
          if (next?.tab === "desktop") {
            ready = true;
            clearStartTimeout();
            setFrame(next);
          }
        } else {
          const packet = JSON.parse(event.data);
          if (packet.type === "ready") { ready = true; clearStartTimeout(); }
          if (packet.type === "control") { setHuman(packet.human === true); setChangingControl(false); }
          if (packet.type === "error") { setChangingControl(false); setError(t("Impossibile completare l’azione.")); }
          if (packet.type === "ended") {
            const normal = packet.reason === "closed";
            close(!normal, packet.reason === "busy" ? t("Questa sessione è già aperta.") : "");
          }
        }
      } catch {
        close(true);
      }
    };
  }

  function send(value: object) {
    const ws = socket.current;
    if (ws?.readyState === WebSocket.OPEN && ws.bufferedAmount < 64 * 1024) {
      ws.send(JSON.stringify(value));
      return true;
    }
    return false;
  }

  const statePill = status.data?.state === "ready"
    ? <span className="pill pill-ok"><span className="dot" aria-hidden="true" />{t("online")}</span>
    : status.data?.state === "starting"
      ? <span className="pill pill-warn"><span className="dot" aria-hidden="true" />{t("Avvio del desktop…")}</span>
      : status.data?.state === "stopped"
        ? <span className="pill pill-off"><span className="dot" aria-hidden="true" />{t("fermo")}</span>
        : null;

  return <main className="main">
    <header className="topbar">
      {menu}<h1>{t("Desktop remoto")}</h1>
      {statePill}
      {connected && <>
        <button className="btn" ref={control} disabled={!frame || changingControl} onClick={() => {
          if (send({ type: human ? "release" : "take" })) setChangingControl(true);
        }}>
          {human ? t("Restituisci il controllo") : t("Prendi il controllo")}
        </button>
        <button className="btn" onClick={() => close()}>{t("Chiudi il desktop")}</button>
      </>}
    </header>
    {connected ? <>
      <div className="desktop-surface">
        <ErrorBoundary inline>
          <BrowserScreen frame={frame} human={human} desktop exit={control}
            onFailure={() => close(true)} onInput={(tab, input) => {
              if ("kind" in input && input.kind === "move") {
                const now = performance.now();
                if (now - lastMove.current < 30) return;
                lastMove.current = now;
              }
              send({ type: "input", tab, input });
            }} />
        </ErrorBoundary>
        {!frame && <div className="browser-placeholder" style={{ position: "absolute", inset: 0, display: "flex", alignItems: "center", justifyContent: "center" }} role="status">
          <LoaderCircle className="spin" aria-hidden="true" />{t("Avvio del desktop…")}
        </div>}
      </div>
      {human && <p className="browser-hint">{t("Fai clic sullo schermo per digitare. Maiusc+Tab torna ai comandi.")}</p>}
    </> : <div className="empty">
      {error && <p role="alert" className="error-line">{error}</p>}
      <button className="btn btn-primary" onClick={start}>{t("Avvia il desktop")}</button>
    </div>}
  </main>;
}
