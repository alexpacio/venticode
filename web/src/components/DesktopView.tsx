import { type ReactNode, useEffect, useRef, useState } from "react";
import { base } from "../api/client.ts";
import { t } from "../i18n/index.ts";
import { BrowserScreen, type BrowserFrame } from "./BrowserScreen.tsx";
import { decodeFrame } from "./BrowserView.tsx";

export function DesktopView({ menu }: { menu: ReactNode }) {
  const socket = useRef<WebSocket | null>(null);
  const lastMove = useRef(0);
  const control = useRef<HTMLButtonElement>(null);
  const [connected, setConnected] = useState(false);
  const [frame, setFrame] = useState<BrowserFrame | null>(null);
  const [human, setHuman] = useState(false);
  const [error, setError] = useState(false);

  useEffect(() => () => {
    const ws = socket.current;
    socket.current = null;
    ws?.close();
  }, []);

  function close(failed = false) {
    const ws = socket.current;
    socket.current = null;
    ws?.close();
    setConnected(false);
    setHuman(false);
    setFrame(null);
    setError(failed);
  }

  function start() {
    setError(false);
    setConnected(true);
    const url = new URL(`${base}/computer/screen`, location.origin);
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    const ws = new WebSocket(url);
    socket.current = ws;
    ws.binaryType = "arraybuffer";
    ws.onerror = ws.onclose = () => { if (socket.current === ws) close(true); };
    ws.onmessage = event => {
      if (socket.current !== ws) return;
      try {
        if (event.data instanceof ArrayBuffer) {
          const next = decodeFrame(event.data);
          if (next?.tab === "desktop") setFrame(next);
        } else {
          const packet = JSON.parse(event.data);
          if (packet.type === "control") setHuman(packet.human === true);
          if (packet.type === "ended") close(true);
        }
      } catch {
        close(true);
      }
    };
  }

  function send(value: object) {
    const ws = socket.current;
    if (ws?.readyState === WebSocket.OPEN && ws.bufferedAmount < 64 * 1024) ws.send(JSON.stringify(value));
  }

  return <main className="main">
    <header className="topbar">
      {menu}<h1>{t("Desktop remoto")}</h1>
      {connected && <>
        <button className="btn" ref={control} disabled={!frame} onClick={() => send({ type: human ? "release" : "take" })}>
          {human ? t("Restituisci il controllo") : t("Prendi il controllo")}
        </button>
        <button className="btn" onClick={() => close()}>{t("Chiudi il desktop")}</button>
      </>}
    </header>
    {connected ? <div className="desktop-surface">
      {frame ? <BrowserScreen frame={frame} human={human} desktop exit={control}
        onFailure={() => close(true)} onInput={(tab, input) => {
          if ("kind" in input && input.kind === "move") {
            const now = performance.now();
            if (now - lastMove.current < 30) return;
            lastMove.current = now;
          }
          send({ type: "input", tab, input });
        }} /> :
        <p className="empty" role="status">{t("Avvio del desktop…")}</p>}
    </div> : <div className="empty">
      {error && <p role="alert">{t("Desktop non disponibile.")}</p>}
      <button className="btn btn-primary" onClick={start}>{t("Avvia il desktop")}</button>
    </div>}
  </main>;
}
