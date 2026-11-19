import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, base, list, ApiError, onUnauthorized } from "./api/client.ts";
import type { Bot, Conversation, Harness, Mention, Model, Project, Session, TranscriptEvent } from "./api/client.ts";
import type { operations } from "./api/schema.ts";
import { applyEvent, createTranscriptState, toTranscript, type Transcript } from "./transcript.ts";

const EMPTY_TRANSCRIPT: Transcript = toTranscript(createTranscriptState());
const SSE_INACTIVITY_MS = 40_000;
const SSE_WATCHDOG_INTERVAL_MS = 5_000;
const SSE_BACKOFF_MIN_MS = 1_000;
const SSE_BACKOFF_MAX_MS = 30_000;
const SSE_FLUSH_WINDOW_MS = 33;

export const useSession = () => {
  const client = useQueryClient();
  useEffect(
    () =>
      onUnauthorized(() => {
        void client.invalidateQueries({ queryKey: ["session"] });
      }),
    [client],
  );
  return useQuery({
    queryKey: ["session"],
    queryFn: ({ signal }) => api<Session>("/session", "GET", undefined, { signal }),
    retry: false,
  });
};
type BrowserThreads = operations["listBrowserThreads"]["responses"][200]["content"]["application/json"];
export const useBrowserThreads = (enabled = true) => useQuery({
  queryKey: ["browser-threads"],
  queryFn: ({ signal }) => api<BrowserThreads>("/browser-agent", "GET", undefined, { signal }),
  enabled,
  retry: false,
  staleTime: 30_000,
});
export const useComputerStatus = (enabled = true) => useQuery({
  queryKey: ["computer-status"],
  queryFn: ({ signal }) => api<{ state: "stopped" | "starting" | "ready"; harness_id?: string }>("/computer", "GET", undefined, { signal }),
  enabled,
  retry: false,
  refetchInterval: 10_000,
});
export const useModels = () => useQuery({
  queryKey: ["models"],
  queryFn: ({ signal }) => list<Model>("/models", { signal }),
  staleTime: 300_000,
});
// shortcut: harness presence is polled; switch to a pushed inventory stream when the backend has one.
export const useHarnesses = () => useQuery({
  queryKey: ["harnesses"],
  queryFn: ({ signal }) => list<Harness>("/harnesses", { signal }),
  refetchInterval: 10_000,
});
export const useProjects = () => useQuery({
  queryKey: ["projects"],
  queryFn: ({ signal }) => list<Project>("/projects", { signal }),
});
export const useConversations = () => useQuery({
  queryKey: ["conversations"],
  queryFn: ({ signal }) => list<Conversation>("/conversations", { signal }),
});
export const useConversation = (id: string) => useQuery({
  queryKey: ["conversation", id],
  queryFn: ({ signal }) => api<Conversation>(`/conversations/${id}`, "GET", undefined, { signal }),
  enabled: Boolean(id),
});
export const useBots = () => useQuery({
  queryKey: ["bots"],
  queryFn: ({ signal }) => list<Bot>("/bots", { signal }),
});
export const useBot = (id: string) => useQuery({
  queryKey: ["bot", id],
  queryFn: ({ signal }) => api<Bot>(`/bots/${id}`, "GET", undefined, { signal }),
  enabled: Boolean(id),
});
export const useMentions = (id: string) => useQuery({
  queryKey: ["mentions", id],
  queryFn: ({ signal }) => list<Mention>(`/bots/${id}/mentions`, { signal }),
  enabled: Boolean(id),
  refetchInterval: 15_000,
});

function backoffDelay(attempt: number): number {
  const baseDelay = Math.min(SSE_BACKOFF_MAX_MS, SSE_BACKOFF_MIN_MS * 2 ** Math.min(attempt, 5));
  const jitter = 0.75 + Math.random() * 0.5;
  return Math.max(SSE_BACKOFF_MIN_MS, Math.min(SSE_BACKOFF_MAX_MS, Math.round(baseDelay * jitter)));
}

/** Live transcript with incremental folding, frame-batched flushes, inactivity watchdog, and managed reconnects. */
export function useTranscript(id: string, enabled = true) {
  const client = useQueryClient();
  const [feed, setFeed] = useState<{ id: string; transcript: Transcript; disconnected: boolean; expired: boolean }>({
    id,
    transcript: EMPTY_TRANSCRIPT,
    disconnected: false,
    expired: false,
  });

  useEffect(() => {
    setFeed({ id, transcript: EMPTY_TRANSCRIPT, disconnected: false, expired: false });
    if (!id || !enabled) return;

    const state = createTranscriptState();
    let disposed = false;
    let expired = false;
    let isDisconnected = false;
    let lastSequence = 0;
    let lastActivityAt = Date.now();
    let attempt = 0;
    let source: EventSource | null = null;
    let dirty = false;

    let invalidateTimer: number | undefined;
    let reconnectTimer: number | undefined;
    let flushTimer: number | undefined;
    let flushRaf: number | undefined;

    const setDisconnected = (next: boolean) => {
      isDisconnected = next;
      setFeed((previous) =>
        previous.id === id && previous.disconnected !== next ? { ...previous, disconnected: next } : previous,
      );
    };

    const flushNow = () => {
      if (flushTimer !== undefined) {
        window.clearTimeout(flushTimer);
        flushTimer = undefined;
      }
      if (flushRaf !== undefined) {
        window.cancelAnimationFrame(flushRaf);
        flushRaf = undefined;
      }
      if (!dirty || disposed) return;
      dirty = false;
      isDisconnected = false;
      const snapshot = toTranscript(state);
      setFeed((previous) =>
        previous.id === id ? { ...previous, transcript: snapshot, disconnected: false } : previous,
      );
    };

    const scheduleFlush = () => {
      dirty = true;
      if (flushTimer !== undefined || flushRaf !== undefined) return;
      flushTimer = window.setTimeout(() => {
        flushTimer = undefined;
        if (disposed) return;
        if (document.visibilityState === "visible" && typeof window.requestAnimationFrame === "function") {
          flushRaf = window.requestAnimationFrame(() => {
            flushRaf = undefined;
            flushNow();
          });
        } else {
          flushNow();
        }
      }, SSE_FLUSH_WINDOW_MS);
    };

    const scheduleInvalidate = () => {
      if (invalidateTimer !== undefined) return;
      invalidateTimer = window.setTimeout(() => {
        invalidateTimer = undefined;
        if (disposed) return;
        void client.invalidateQueries({ queryKey: ["conversations"] });
        void client.invalidateQueries({ queryKey: ["conversation", id] });
      }, 250);
    };

    const closeSource = () => {
      if (source) {
        source.onopen = null;
        source.onerror = null;
        source.close();
        source = null;
      }
    };

    const scheduleReconnect = () => {
      if (disposed || expired || reconnectTimer !== undefined) return;
      const delay = backoffDelay(attempt);
      attempt += 1;
      reconnectTimer = window.setTimeout(() => {
        reconnectTimer = undefined;
        connect();
      }, delay);
    };

    const connect = () => {
      if (disposed || expired) return;
      if (reconnectTimer !== undefined) {
        window.clearTimeout(reconnectTimer);
        reconnectTimer = undefined;
      }
      closeSource();
      lastActivityAt = Date.now();
      const next = new EventSource(`${base}/conversations/${id}/stream?after=${lastSequence}`);
      source = next;

      next.onopen = () => {
        if (disposed || source !== next) return;
        attempt = 0;
        lastActivityAt = Date.now();
        setDisconnected(false);
      };

      next.onerror = () => {
        if (disposed || source !== next) return;
        setDisconnected(true);
        if (next.readyState === EventSource.CLOSED) {
          closeSource();
          if (attempt % 3 === 0) {
            void client.invalidateQueries({ queryKey: ["session"] });
            void client.invalidateQueries({ queryKey: ["conversation", id] });
          }
          scheduleReconnect();
        }
      };

      next.addEventListener("heartbeat", () => {
        if (disposed || source !== next) return;
        lastActivityAt = Date.now();
        if (isDisconnected) setDisconnected(false);
      });

      next.addEventListener("transcript", (message) => {
        if (disposed || source !== next) return;
        lastActivityAt = Date.now();
        let event: TranscriptEvent;
        try {
          event = JSON.parse((message as MessageEvent).data) as TranscriptEvent;
        } catch {
          return;
        }
        if (typeof event.sequence !== "number" || event.sequence <= lastSequence) {
          if (isDisconnected) setDisconnected(false);
          return;
        }
        applyEvent(state, event);
        lastSequence = event.sequence;
        scheduleFlush();
        if (
          (event.type === "run" && (event.state !== "running" || !event.usage)) ||
          ["user_message", "goal_cancelled", "plan", "question", "goal_completed"].includes(event.type)
        ) {
          scheduleInvalidate();
        }
      });

      next.addEventListener("authorization_expired", () => {
        if (disposed || source !== next) return;
        expired = true;
        closeSource();
        if (reconnectTimer !== undefined) {
          window.clearTimeout(reconnectTimer);
          reconnectTimer = undefined;
        }
        isDisconnected = true;
        setFeed((previous) => (previous.id === id ? { ...previous, disconnected: true, expired: true } : previous));
        void client.invalidateQueries({ queryKey: ["session"] });
        void client.invalidateQueries({ queryKey: ["conversation", id] });
      });
    };

    const reconnectNowIfNeeded = () => {
      if (disposed || expired) return;
      if (!source || source.readyState !== EventSource.OPEN || isDisconnected) {
        attempt = 0;
        connect();
      }
    };

    const onOnline = () => reconnectNowIfNeeded();
    const onVisibilityChange = () => {
      if (document.visibilityState === "visible") reconnectNowIfNeeded();
    };

    window.addEventListener("online", onOnline);
    document.addEventListener("visibilitychange", onVisibilityChange);

    const watchdog = window.setInterval(() => {
      if (disposed || expired) return;
      if (source && Date.now() - lastActivityAt >= SSE_INACTIVITY_MS) {
        closeSource();
        setDisconnected(true);
        attempt = 0;
        connect();
      }
    }, SSE_WATCHDOG_INTERVAL_MS);

    connect();

    return () => {
      disposed = true;
      window.removeEventListener("online", onOnline);
      document.removeEventListener("visibilitychange", onVisibilityChange);
      window.clearInterval(watchdog);
      if (invalidateTimer !== undefined) window.clearTimeout(invalidateTimer);
      if (reconnectTimer !== undefined) window.clearTimeout(reconnectTimer);
      if (flushTimer !== undefined) window.clearTimeout(flushTimer);
      if (flushRaf !== undefined) window.cancelAnimationFrame(flushRaf);
      closeSource();
    };
  }, [id, enabled, client]);

  return {
    transcript: feed.id === id ? feed.transcript : EMPTY_TRANSCRIPT,
    disconnected: feed.id === id ? feed.disconnected : false,
    expired: feed.id === id ? feed.expired : false,
  };
}

export const isUnauthorized = (error: unknown) => error instanceof ApiError && error.status === 401;
