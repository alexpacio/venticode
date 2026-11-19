import { useEffect, useMemo, useRef, useState } from "react";
import init, { render } from "../generated/terminal/venticode_tui_browser.js";
import type { Conversation, Project } from "../api/client.ts";
import { t, useLanguage } from "../i18n/index.ts";
import type { Block } from "../transcript.ts";

type Cells = [string, string, string][][];
const empty: Block[] = [];
let moduleReady: ReturnType<typeof init> | undefined;

export function Terminal({
  project,
  chat,
  blocks = empty,
  notice,
}: {
  project?: Project;
  chat?: Conversation;
  blocks?: Block[];
  notice?: string;
}) {
  const language = useLanguage();
  const area = useRef<HTMLDivElement>(null);
  const measure = useRef<HTMLSpanElement>(null);
  const [size, setSize] = useState({ width: 80, height: 24 });
  const [ready, setReady] = useState(false);
  const [failure, setFailure] = useState("");
  const [offset, setOffset] = useState(0);
  const [expanded, setExpanded] = useState(false);
  const [copied, setCopied] = useState(false);
  const touch = useRef(0);
  useEffect(() => {
    let active = true;
    (moduleReady ??= init())
      .then(() => {
        if (active) setReady(true);
      })
      .catch((e: Error) => {
        moduleReady = undefined;
        if (active) setFailure(e.message);
      });
    const resize = () => {
      const el = area.current,
        ruler = measure.current;
      if (!el || !ruler) return;
      const cell = ruler.getBoundingClientRect();
      if (cell.width && cell.height)
        setSize({
          width: Math.max(
            1,
            Math.min(240, Math.floor(el.clientWidth / cell.width)),
          ),
          height: Math.max(
            1,
            Math.min(98, Math.floor(el.clientHeight / cell.height)),
          ),
        });
    };
    const observer = new ResizeObserver(resize);
    observer.observe(area.current!);
    observer.observe(measure.current!);
    void document.fonts?.ready?.then(() => { if (active) resize(); });
    return () => {
      active = false;
      observer.disconnect();
    };
  }, []);
  const lines = useMemo(() => {
    const items: { role: string; text: string }[] = [];
    for (const block of blocks) {
      if (block.kind === "tool") {
        items.push({
          role: "Tool",
          text: [block.operation, block.target, block.summary]
            .filter(Boolean)
            .join(" · "),
        });
        if (block.output) items.push({ role: "Output", text: block.output });
        if (block.diff)
          items.push({
            role: "Output",
            text: block.diff.path + "\n" + block.diff.patch,
          });
      } else if (block.kind === "error")
        items.push({ role: "Error", text: block.message });
      else
        items.push({
          role: block.kind === "user" ? "User" : "Assistant",
          text: block.content,
        });
    }
    return items.length
      ? items
      : [
          {
            role: "Notice",
            text: notice || t("Scrivi il primo messaggio e premi Invio"),
          },
        ];
  }, [blocks, notice, language]);
  const lastReply = useMemo(() => {
    for (let i = blocks.length - 1; i >= 0; i--) {
      const b = blocks[i];
      if (b.kind === "text" && b.content.trim()) return b.content;
    }
    return "";
  }, [blocks]);
  const { frame, renderError } = useMemo(() => {
    if (!ready) return { frame: undefined, renderError: "" };
    try {
      const parsed = JSON.parse(
        render(
          JSON.stringify({
            project,
            chat,
            model: chat?.model,
            lines,
            en: language === "en",
            compact_tools: !expanded,
            scroll: offset,
          }),
          size.width,
          size.height + 2,
        ),
      ) as { rows: Cells; scroll: number };
      return { frame: parsed, renderError: "" };
    } catch (err) {
      return { frame: undefined, renderError: (err as Error).message };
    }
  }, [ready, project, chat, lines, language, expanded, offset, size]);
  const move = (n: number) =>
    setOffset(Math.max(0, Math.min(65535, (frame?.scroll ?? 0) + n)));
  const copyLast = async () => {
    if (!lastReply) return;
    try {
      await navigator.clipboard.writeText(lastReply);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1800);
    } catch { /* clipboard unavailable */ }
  };
  const errMsg = failure || renderError;
  return (
    <section className="terminal" aria-label={t("Terminale")}>
      <div
        className="terminal-viewport"
        role="log"
        aria-live="off"
        tabIndex={0}
        ref={area}
        onWheel={(e) => {
          e.preventDefault();
          const linesDelta = e.deltaMode === 1 ? e.deltaY : e.deltaY / 24;
          const step = Math.sign(linesDelta) * Math.max(1, Math.min(8, Math.round(Math.abs(linesDelta))));
          move(-step);
        }}
        onTouchStart={(e) => {
          touch.current = e.touches[0].clientY;
        }}
        onTouchMove={(e) => {
          const y = e.touches[0].clientY,
            n = Math.trunc((y - touch.current) / 20);
          if (n) {
            move(n);
            touch.current = y;
          }
        }}
        onKeyDown={(e) => {
          if (e.key === "PageUp" || e.key === "PageDown") {
            e.preventDefault();
            move(size.height * (e.key === "PageUp" ? 1 : -1));
          } else if (e.key === "ArrowUp" || e.key === "ArrowDown") {
            e.preventDefault();
            move(e.key === "ArrowUp" ? 1 : -1);
          } else if (e.key === "Home") {
            e.preventDefault();
            setOffset(65535);
          } else if (e.key === "End") {
            setOffset(0);
          } else if (e.ctrlKey && e.key.toLowerCase() === "o") {
            e.preventDefault();
            setExpanded((v) => !v);
          }
        }}
      >
        <span aria-hidden="true" className="cell-ruler" ref={measure}>
          M
        </span>
        {errMsg ? (
          <p role="alert">{errMsg}</p>
        ) : !frame ? (
          t("Caricamento…")
        ) : (
          frame.rows.slice(0, -2).map((row, y) => (
            <div className="terminal-row" key={y}>
              {row.map(([fg, bg, text], i) => (
                <span
                  key={i}
                  style={{
                    color: fg || undefined,
                    backgroundColor: bg || undefined,
                  }}
                >
                  {text}
                </span>
              ))}
            </div>
          ))
        )}
      </div>
      {(blocks.some((b) => b.kind === "tool") || !!frame?.scroll || !!lastReply) && (
        <div className="terminal-keys">
          {blocks.some((b) => b.kind === "tool") && (
            <button
              type="button"
              aria-pressed={expanded}
              onClick={() => setExpanded((v) => !v)}
            >
              {expanded ? t("Comprimi output") : t("Espandi output")}
            </button>
          )}
          {!!lastReply && (
            <button type="button" onClick={() => void copyLast()}>
              {copied ? t("Copiato") : t("Copia risposta")}
            </button>
          )}
          {!!frame?.scroll && (
            <button type="button" onClick={() => setOffset(0)}>
              {t("Torna in fondo")}
            </button>
          )}
        </div>
      )}
    </section>
  );
}
