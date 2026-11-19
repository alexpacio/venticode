import { useEffect, useRef, useState } from "react";
import type { FormEvent, ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { AppWindow, LoaderCircle, Plus } from "lucide-react";
import { ApiError, base } from "../api/client.ts";
import { t, useLanguage } from "../i18n/index.ts";
import { useBrowserThreads } from "../queries.ts";
import { BrowserScreen } from "./BrowserScreen.tsx";
import type { BrowserFrame } from "./BrowserScreen.tsx";
import { ErrorBoundary } from "./ErrorBoundary.tsx";

type Tab = { targetId: string; title: string; url: string };
type Message = { role: string; content: string };
type BrowserEvent =
  | { type: "tabs"; active: string | null; tabs: Tab[] }
  | { type: "history"; messages: Message[] }
  | { type: "control"; human: boolean }
  | { type: "ready" | "working" | "idle" | "heartbeat" }
  | { type: "ended"; reason: string }
  | { type: "error"; status: number };

const utf8 = new TextDecoder();

export function decodeFrame(packet: ArrayBuffer): BrowserFrame | null {
  const bytes = new Uint8Array(packet);
  const end = bytes.subarray(0, 1024).indexOf(0x0a);
  if (end < 0 || end + 1 === bytes.length) return null;
  let header: any;
  try {
    header = JSON.parse(utf8.decode(bytes.subarray(0, end)));
  } catch {
    return null;
  }
  if (header?.type !== "frame" || typeof header.tab !== "string" ||
      typeof header.width !== "number" || typeof header.height !== "number" ||
      !(header.width > 0 && header.width <= 4096 && header.height > 0 && header.height <= 4096)) return null;
  return {
    tab: header.tab,
    width: header.width,
    height: header.height,
    jpeg: new Blob([bytes.subarray(end + 1)], { type: "image/jpeg" }),
  };
}

export function BrowserView({ menu }: { menu: ReactNode }) {
  const threads = useBrowserThreads();
  let content;
  if (threads.data) {
    content = <BrowserSession threads={threads.data.threads} />;
  } else if (threads.isPending) {
    content = <div className="empty" role="status"><LoaderCircle className="spin" aria-hidden="true" />{t("Caricamento…")}</div>;
  } else if (threads.isError) {
    const unavailable = threads.error instanceof ApiError && [403, 404].includes(threads.error.status);
    content = <div className="empty" role="status">
      <p>{unavailable ? t("Il browser remoto non è disponibile in questo workspace.") : t("Impossibile contattare il browser remoto.")}</p>
      {!unavailable && <button type="button" className="btn" onClick={() => void threads.refetch()}>{t("Riprova")}</button>}
    </div>;
  }
  return <main className="main">
    <header className="topbar">
      {menu}
      <div className="crumbs"><AppWindow size={18} aria-hidden="true" /><strong>{t("Browser remoto")}</strong></div>
    </header>
    {content}
  </main>;
}

function BrowserSession({ threads }: { threads: { id: string; updated_at: string }[] }) {
  const language = useLanguage();
  const client = useQueryClient();
  const [selection, setSelection] = useState("");
  const [status, setStatus] = useState<"closed" | "opening" | "ready" | "closing">("closed");
  const [pending, setPending] = useState(false);
  const [human, setHuman] = useState(false);
  const [changingControl, setChangingControl] = useState(false);
  const [error, setError] = useState("");
  const [tabs, setTabs] = useState<Tab[]>([]);
  const [active, setActive] = useState<string | null>(null);
  const activeTab = useRef<string | null>(null);
  const [frame, setFrame] = useState<BrowserFrame | null>(null);
  const [history, setHistory] = useState<Message[]>([]);
  const [address, setAddress] = useState("");
  const [prompt, setPrompt] = useState("");
  const socket = useRef<WebSocket | null>(null);
  const closingByUser = useRef(false);
  const openTimeout = useRef<number | undefined>(undefined);
  const closeTimeout = useRef<number | undefined>(undefined);
  const watchdog = useRef<number | undefined>(undefined);
  const controlButton = useRef<HTMLButtonElement>(null);
  const log = useRef<HTMLDivElement>(null);
  const tab = tabs.find((item) => item.targetId === active);
  const tabUrl = tab?.url === "about:blank" ? "" : tab?.url;

  const clearTimers = () => {
    if (openTimeout.current !== undefined) {
      window.clearTimeout(openTimeout.current);
      openTimeout.current = undefined;
    }
    if (closeTimeout.current !== undefined) {
      window.clearTimeout(closeTimeout.current);
      closeTimeout.current = undefined;
    }
    if (watchdog.current !== undefined) {
      window.clearInterval(watchdog.current);
      watchdog.current = undefined;
    }
  };

  useEffect(() => setAddress(tabUrl ?? ""), [active, tabUrl]);
  useEffect(() => { if (log.current) log.current.scrollTop = log.current.scrollHeight; }, [history, pending]);
  useEffect(() => () => {
    clearTimers();
    const ws = socket.current;
    socket.current = null;
    ws?.close();
  }, []);

  function start() {
    if (socket.current) return;
    clearTimers();
    closingByUser.current = false;
    setError("");
    setHistory([]);
    setStatus("opening");
    const id = selection || crypto.randomUUID();
    const url = new URL(`${base}/browser-agent/${id}`, location.href);
    url.protocol = location.protocol === "https:" ? "wss:" : "ws:";
    const ws = new WebSocket(url);
    ws.binaryType = "arraybuffer";
    socket.current = ws;
    let ready = false;
    let ended = false;
    let lastActivityAt = Date.now();
    openTimeout.current = window.setTimeout(() => {
      openTimeout.current = undefined;
      if (socket.current === ws) ws.close();
    }, 60_000);
    watchdog.current = window.setInterval(() => {
      if (socket.current !== ws) return;
      if (ready && Date.now() - lastActivityAt >= 40_000) ws.close();
    }, 5_000);
    ws.onopen = () => {
      if (socket.current !== ws) return;
      lastActivityAt = Date.now();
    };
    ws.onmessage = (event) => {
      if (socket.current !== ws) return;
      lastActivityAt = Date.now();
      if (event.data instanceof ArrayBuffer) {
        const next = decodeFrame(event.data);
        if (next?.tab === activeTab.current) setFrame(next);
        return;
      }
      let packet: BrowserEvent;
      try {
        packet = JSON.parse(event.data) as BrowserEvent;
      } catch {
        return;
      }
      switch (packet.type) {
        case "heartbeat":
          break;
        case "ready":
          ready = true;
          if (openTimeout.current !== undefined) {
            window.clearTimeout(openTimeout.current);
            openTimeout.current = undefined;
          }
          setStatus("ready");
          break;
        case "tabs":
          if (packet.active !== activeTab.current) setFrame(null);
          activeTab.current = packet.active;
          setActive(packet.active);
          setTabs(packet.tabs);
          break;
        case "history":
          setHistory(packet.messages);
          if (packet.messages.length) void client.invalidateQueries({ queryKey: ["browser-threads"] });
          break;
        case "control":
          setHuman(packet.human);
          setChangingControl(false);
          break;
        case "working":
          setPending(true);
          break;
        case "idle":
          setPending(false);
          break;
        case "error":
          setChangingControl(false);
          setPending(false);
          setError(packet.status === 403 ? t("Azione o sito non consentito.") : t("Impossibile completare l’azione."));
          break;
        case "ended":
          ended = true;
          if (packet.reason === "busy") setError(t("Questa sessione è già aperta."));
          else if (packet.reason !== "closed") setError(t("Impossibile avviare il browser remoto."));
          ws.close();
          break;
      }
    };
    ws.onclose = () => {
      if (socket.current !== ws) return;
      clearTimers();
      socket.current = null;
      activeTab.current = null;
      const userClosed = closingByUser.current;
      closingByUser.current = false;
      setStatus("closed");
      setActive(null);
      setTabs([]);
      setFrame(null);
      setHuman(false);
      setPending(false);
      setChangingControl(false);
      if (!ended && !userClosed) setError(ready ? t("Connessione al browser interrotta.") : t("Impossibile avviare il browser remoto."));
    };
  }

  function send(packet: object) {
    const ws = socket.current;
    if (ws?.readyState === WebSocket.OPEN && ws.bufferedAmount < 64 * 1024) {
      ws.send(JSON.stringify(packet));
      return true;
    }
    return false;
  }

  function ask(event: FormEvent) {
    event.preventDefault();
    if (!canAsk || !prompt.trim()) return;
    setError("");
    if (send({ type: "ask", prompt })) {
      setPending(true);
      setPrompt("");
    }
  }

  const canAsk = status === "ready" && !human && !pending && !changingControl;
  const canInput = status === "ready" && human && !changingControl;
  return <div className="page browser-page">
    <div className="browser-actions">
      <label className="field">
        <span>{t("Conversazione browser")}</span>
        <select className="select" value={selection} onChange={(event) => setSelection(event.target.value)} disabled={status !== "closed"}>
          <option value="">{t("Nuova conversazione")}</option>
          {threads.map((thread) => <option key={thread.id} value={thread.id}>
            {new Date(thread.updated_at).toLocaleString(language)}
          </option>)}
        </select>
      </label>
      {status === "closed"
        ? <button type="button" className="btn btn-primary" onClick={start}>{t("Avvia Kitesurf")}</button>
        : <button type="button" className="btn btn-danger" disabled={status === "closing"} onClick={() => {
          closingByUser.current = true;
          if (status === "opening") {
            clearTimers();
            const ws = socket.current;
            socket.current = null;
            ws?.close();
            setStatus("closed");
            return;
          }
          setStatus("closing");
          const ws = socket.current;
          if (!send({ type: "end" })) {
            ws?.close();
          } else {
            if (closeTimeout.current !== undefined) window.clearTimeout(closeTimeout.current);
            closeTimeout.current = window.setTimeout(() => {
              closeTimeout.current = undefined;
              if (socket.current === ws) ws?.close();
            }, 3000);
          }
        }}>{t("Termina sessione")}</button>}
    </div>
    {error && <p className="error-line" role="alert">{error}</p>}
    <div className="browser-layout">
      <section className="browser-surface" aria-label={t("Browser remoto")}>
        <div className="browser-tabs" aria-label={t("Schede del browser")}>
          {tabs.map((item) => <button
            key={item.targetId}
            type="button"
            className="btn"
            aria-pressed={item.targetId === active}
            disabled={!canInput}
            title={item.url}
            onClick={() => send({ type: "tab", id: item.targetId })}
          >{item.title || t("Nuova scheda")}</button>)}
          <button type="button" className="icon-btn" aria-label={t("Nuova scheda")} disabled={!canInput}
            onClick={() => send({ type: "new_tab" })}><Plus size={17} aria-hidden="true" /></button>
        </div>
        <div className="browser-tools">
          <form onSubmit={(event) => {
            event.preventDefault();
            if (!canInput) return;
            const raw = address.trim();
            if (!raw) return;
            const normalized = /^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(raw) ? raw : `https://${raw}`;
            setAddress(normalized);
            setError("");
            send({ type: "open", url: normalized });
          }}>
            <input className="input mono" type="text" inputMode="url" required maxLength={4096} value={address}
              readOnly={!canInput} onChange={(event) => setAddress(event.target.value)} aria-label={t("Indirizzo del browser")} />
            <button className="btn" disabled={!canInput}>{t("Apri")}</button>
          </form>
          <button type="button" className="btn" ref={controlButton} disabled={status !== "ready" || changingControl}
            onClick={() => {
              setError("");
              if (send({ type: human ? "release" : "take" })) setChangingControl(true);
            }}>{human ? t("Restituisci il controllo") : t("Prendi il controllo")}</button>
        </div>
        <div className="browser-stage">
          {status !== "closed" && <ErrorBoundary inline>
            <BrowserScreen
              frame={frame}
              human={canInput}
              exit={controlButton}
              onInput={(target, input) => {
                if (canInput && target === activeTab.current) send({ type: "input", tab: target, input });
              }}
              onFailure={() => setError(t("Impossibile visualizzare lo schermo remoto."))}
            />
          </ErrorBoundary>}
          {!frame && <div className="browser-placeholder" role="status">
            {status === "opening" && <LoaderCircle className="spin" aria-hidden="true" />}
            {status === "closed" ? t("Avvia Kitesurf per vedere il browser qui.") :
              status === "opening" ? t("Avvio del browser…") : t("In attesa dello schermo remoto…")}
          </div>}
        </div>
        {canInput && <p className="browser-hint">{t("Fai clic sullo schermo per digitare. Maiusc+Tab torna ai comandi.")}</p>}
      </section>
      <section className="browser-chat" aria-label={t("Assistente browser")}>
        <h2>{t("Assistente browser")}</h2>
        <div className="browser-messages" role="log" ref={log}>
          {history.map((message, index) => <div className="browser-message" key={index}>
            <strong>{message.role === "user" ? t("Tu") : t("Assistente browser")}</strong>
            <p>{message.content}</p>
          </div>)}
          {pending && !human && <p className="muted">{t("L’assistente sta navigando…")}</p>}
        </div>
        <form className="browser-prompt" onSubmit={ask}>
          <label className="field">
            <span>{t("Richiesta per l’assistente")}</span>
            <textarea className="textarea" value={prompt} rows={3} maxLength={6000}
              onChange={(event) => setPrompt(event.target.value)}
              onKeyDown={(event) => {
                if ((event.ctrlKey || event.metaKey) && event.key === "Enter") {
                  event.preventDefault();
                  ask(event);
                }
              }} />
          </label>
          <button className="btn btn-primary" disabled={!canAsk || !prompt.trim()}>{t("Invia")}</button>
        </form>
      </section>
    </div>
  </div>;
}
