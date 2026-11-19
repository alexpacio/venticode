import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, base, list, ApiError, onUnauthorized } from "./api/client.ts";
import type { Bot, Conversation, Harness, Mention, Model, Project, Session, TranscriptEvent } from "./api/client.ts";
import type { operations } from "./api/schema.ts";

export const useSession = () => {
  const client = useQueryClient();
  useEffect(() => {
    onUnauthorized(() => {
      void client.invalidateQueries({ queryKey: ["session"] });
    });
  }, [client]);
  return useQuery({ queryKey: ["session"], queryFn: () => api<Session>("/session"), retry: false });
};
type BrowserThreads = operations["listBrowserThreads"]["responses"][200]["content"]["application/json"];
export const useBrowserThreads = (enabled = true) => useQuery({
  queryKey: ["browser-threads"],
  queryFn: () => api<BrowserThreads>("/browser-agent"),
  enabled,
  retry: false,
  staleTime: 30_000,
});
export const useComputerStatus = (enabled = true) => useQuery({
  queryKey: ["computer-status"],
  queryFn: () => api<{ state: "stopped" | "starting" | "ready"; harness_id?: string }>("/computer"),
  enabled,
  retry: false,
  refetchInterval: 10_000,
});
export const useModels = () => useQuery({ queryKey: ["models"], queryFn: () => list<Model>("/models"), staleTime: 300_000 });
// shortcut: harness presence is polled; switch to a pushed inventory stream when the backend has one.
export const useHarnesses = () => useQuery({ queryKey: ["harnesses"], queryFn: () => list<Harness>("/harnesses"), refetchInterval: 10_000 });
export const useProjects = () => useQuery({ queryKey: ["projects"], queryFn: () => list<Project>("/projects") });
export const useConversations = () => useQuery({ queryKey: ["conversations"], queryFn: () => list<Conversation>("/conversations") });
export const useConversation = (id: string) => useQuery({ queryKey: ["conversation", id], queryFn: () => api<Conversation>(`/conversations/${id}`), enabled: Boolean(id) });
export const useBots = () => useQuery({ queryKey: ["bots"], queryFn: () => list<Bot>("/bots") });
export const useBot = (id: string) => useQuery({ queryKey: ["bot", id], queryFn: () => api<Bot>(`/bots/${id}`), enabled: Boolean(id) });
export const useMentions = (id: string) => useQuery({ queryKey: ["mentions", id], queryFn: () => list<Mention>(`/bots/${id}/mentions`), enabled: Boolean(id), refetchInterval: 15_000 });

/** Live transcript: the stream replays from the start, then follows; EventSource resumes with Last-Event-ID. */
export function useTranscript(id: string, enabled = true) {
  const client = useQueryClient();
  const [feed, setFeed] = useState<{ id: string; events: TranscriptEvent[]; disconnected: boolean }>({ id, events: [], disconnected: false });
  useEffect(() => {
    setFeed({ id, events: [], disconnected: false });
    if (!id || !enabled) return;
    let invalidateTimer: number | undefined;
    const scheduleInvalidate = () => {
      if (invalidateTimer !== undefined) return;
      invalidateTimer = window.setTimeout(() => {
        invalidateTimer = undefined;
        void client.invalidateQueries({ queryKey: ["conversations"] });
        void client.invalidateQueries({ queryKey: ["conversation", id] });
      }, 250);
    };
    const source = new EventSource(`${base}/conversations/${id}/stream?after=0`);
    source.onopen = () => {
      setFeed((previous) => (previous.id === id && previous.disconnected ? { ...previous, disconnected: false } : previous));
    };
    source.onerror = () => {
      setFeed((previous) => (previous.id === id ? { ...previous, disconnected: true } : previous));
      if (source.readyState === EventSource.CLOSED) {
        void client.invalidateQueries({ queryKey: ["session"] });
      }
    };
    source.addEventListener("transcript", (message) => {
      let event: TranscriptEvent;
      try {
        event = JSON.parse((message as MessageEvent).data) as TranscriptEvent;
      } catch {
        return;
      }
      setFeed((previous) => (
        previous.id === id && event.sequence > (previous.events.at(-1)?.sequence ?? 0)
          ? { id, events: [...previous.events, event], disconnected: false }
          : previous
      ));
      if ((event.type === "run" && (event.state !== "running" || !event.usage)) || ["user_message", "goal_cancelled", "plan", "question", "goal_completed"].includes(event.type)) {
        scheduleInvalidate();
      }
    });
    source.addEventListener("authorization_expired", () => {
      source.close();
      void client.invalidateQueries({ queryKey: ["session"] });
    });
    return () => {
      if (invalidateTimer !== undefined) window.clearTimeout(invalidateTimer);
      source.close();
    };
  }, [id, enabled, client]);
  return {
    events: feed.id === id ? feed.events : [],
    disconnected: feed.id === id ? feed.disconnected : false,
  };
}

export const isUnauthorized = (error: unknown) => error instanceof ApiError && error.status === 401;
