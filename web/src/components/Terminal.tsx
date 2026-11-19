import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import init, { render } from "../generated/terminal/venticode_tui_browser.js";
import type { Conversation, Project } from "../api/client.ts";
import { t, useLanguage } from "../i18n/index.ts";
import type { Block } from "../transcript.ts";

type Cells = [string, string, string][][];
const empty: Block[] = [];
let moduleReady: ReturnType<typeof init> | undefined;

function estimateWrappedRows(items: { role: string; text: string }[], width: number, expanded: boolean): number {
  if (!items.length) return 0;
  let total = items.length - 1;
  for (const item of items) {
    const w = Math.max(1, item.role === "Assistant" ? width : width - 2);
    const compact = item.role === "Output" && !expanded;
    let itemRows = 0;
    for (const raw of item.text.split("\n")) {
      itemRows += Math.max(1, Math.ceil(raw.length / w));
      if (compact && itemRows >= 4) {
        itemRows = 4;
        break;
      }
    }
    total += itemRows;
  }
  return total;
}

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
  const [loadAttempt, setLoadAttempt] = useState(0);
  const [offset, setOffset] = useState(0);
  const [expanded, setExpanded] = useState(false);
  const [copied, setCopied] = useState(false);
  const touch = useRef(0);
  const copyTimer = useRef<number | undefined>(undefined);

  useEffect(() => {
    let active = true;
    setFailure("");
    (moduleReady ??= init())
      .then(() => {
        if (active) setReady(true);
      })
      .catch((e: Error) => {
        moduleReady = undefined;
        if (active) setFailure(e.message || String(e));
      });
    return () => {
      active = false;
    };
  }, [loadAttempt]);

  useEffect(() => {
    let active = true;
    const resize = () => {
      const el = area.current,
        ruler = measure.current;
      if (!el || !ruler) return;
      const cell = ruler.getBoundingClientRect();
      if (cell.width && cell.height) {
        const w = Math.max(1, Math.min(240, Math.floor(el.clientWidth / cell.width)));
        const h = Math.max(1, Math.min(98, Math.floor(el.clientHeight / cell.height)));
        setSize((prev) => (prev.width === w && prev.height === h ? prev : { width: w, height: h }));
      }
    };
    const observer = new ResizeObserver(resize);
    if (area.current) observer.observe(area.current);
    if (measure.current) observer.observe(measure.current);
    void document.fonts?.ready?.then(() => {
      if (active) resize();
    });
    return () => {
      active = false;
      observer.disconnect();
    };
  }, []);

  useEffect(() => () => {
    if (copyTimer.current !== undefined) window.clearTimeout(copyTimer.current);
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

  const hasTools = useMemo(() => blocks.some((b) => b.kind === "tool"), [blocks]);

  const lastReply = useMemo(() => {
    for (let i = blocks.length - 1; i >= 0; i--) {
      const b = blocks[i];
      if (b.kind === "text" && b.content.trim()) return b.content;
    }
    return "";
  }, [blocks]);

  const projectName = project?.name;
  const chatTitle = chat?.title;
  const chatState = chat?.state;
  const chatMode = chat?.permission_mode;
  const chatModel = chat?.model;
  const headerPayload = useMemo(
    () => ({
      project: projectName ? { name: projectName } : undefined,
      chat:
        chatTitle || chatState || chatMode
          ? { title: chatTitle, state: chatState, permission_mode: chatMode }
          : undefined,
      model: chatModel,
    }),
    [projectName, chatTitle, chatState, chatMode, chatModel],
  );

  const estimatedRows = useMemo(
    () => estimateWrappedRows(lines, size.width, expanded),
    [lines, size.width, expanded],
  );
  const prevMetrics = useRef({ rows: estimatedRows, width: size.width, expanded });
  const lastRenderedScroll = useRef(0);

  useLayoutEffect(() => {
    const prev = prevMetrics.current;
    prevMetrics.current = { rows: estimatedRows, width: size.width, expanded };
    if (prev.width === size.width && prev.expanded === expanded) {
      const delta = estimatedRows - prev.rows;
      if (delta > 0) {
        setOffset((current) => {
          if (current <= 0) return 0;
          const baseScroll = lastRenderedScroll.current > 0 ? lastRenderedScroll.current : current;
          return Math.min(65535, baseScroll + delta);
        });
      }
    }
  }, [estimatedRows, size.width, expanded]);

  const { frame, renderError } = useMemo(() => {
    if (!ready) return { frame: undefined, renderError: "" };
    try {
      const parsed = JSON.parse(
        render(
          JSON.stringify({
            ...headerPayload,
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
  }, [ready, headerPayload, lines, language, expanded, offset, size]);

  useLayoutEffect(() => {
    if (frame) lastRenderedScroll.current = frame.scroll;
  }, [frame]);

  const move = (n: number) =>
    setOffset(Math.max(0, Math.min(65535, (frame?.scroll ?? 0) + n)));
  const moveRef = useRef(move);
  moveRef.current = move;

  useEffect(() => {
    const el = area.current;
    if (!el) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const linesDelta = e.deltaMode === 1 ? e.deltaY : e.deltaY / 24;
      const step = Math.sign(linesDelta) * Math.max(1, Math.min(8, Math.round(Math.abs(linesDelta))));
      moveRef.current(-step);
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, []);

  const copyLast = async () => {
    if (!lastReply) return;
    try {
      await navigator.clipboard.writeText(lastReply);
      if (copyTimer.current !== undefined) window.clearTimeout(copyTimer.current);
      setCopied(true);
      copyTimer.current = window.setTimeout(() => {
        copyTimer.current = undefined;
        setCopied(false);
      }, 1800);
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
          <div role="alert" style={{ display: "flex", flexDirection: "column", alignItems: "flex-start", gap: 8 }}>
            <p style={{ margin: 0 }}>{errMsg}</p>
            {failure && (
              <button type="button" className="btn" onClick={() => setLoadAttempt((n) => n + 1)}>
                {t("Riprova")}
              </button>
            )}
          </div>
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
      {(hasTools || !!frame?.scroll || !!lastReply) && (
        <div className="terminal-keys">
          {hasTools && (
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
