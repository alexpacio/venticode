import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, base, list, ApiError } from "./api/client.ts";
import type { Bot, Conversation, Harness, Mention, Model, Project, Session, TranscriptEvent } from "./api/client.ts";

export const useSession = () => useQuery({ queryKey: ["session"], queryFn: () => api<Session>("/session"), retry: false });
export const useModels = () => useQuery({ queryKey: ["models"], queryFn: () => list<Model>("/models"), staleTime: 300_000 });
// shortcut: harness presence is polled; switch to a pushed inventory stream when the backend has one.
export const useHarnesses = () => useQuery({ queryKey: ["harnesses"], queryFn: () => list<Harness>("/harnesses"), refetchInterval: 10_000 });
export const useProjects = () => useQuery({ queryKey: ["projects"], queryFn: () => list<Project>("/projects") });
export const useConversations = () => useQuery({ queryKey: ["conversations"], queryFn: () => list<Conversation>("/conversations") });
export const useConversation = (id: string) => useQuery({ queryKey: ["conversation", id], queryFn: () => api<Conversation>(`/conversations/${id}`) });
export const useBots = () => useQuery({ queryKey: ["bots"], queryFn: () => list<Bot>("/bots") });
export const useBot = (id: string) => useQuery({ queryKey: ["bot", id], queryFn: () => api<Bot>(`/bots/${id}`) });
export const useMentions = (id: string) => useQuery({ queryKey: ["mentions", id], queryFn: () => list<Mention>(`/bots/${id}/mentions`), refetchInterval: 15_000 });

/** Live transcript: the stream replays from the start, then follows; EventSource resumes with Last-Event-ID. */
export function useTranscript(id: string) {
  const client = useQueryClient();
  const [events, setEvents] = useState<TranscriptEvent[]>([]);
  useEffect(() => {
    setEvents([]);
    const source = new EventSource(`${base}/conversations/${id}/stream?after=0`);
    source.addEventListener("transcript", (message) => {
      const event = JSON.parse((message as MessageEvent).data) as TranscriptEvent;
      setEvents((previous) => (event.sequence > (previous.at(-1)?.sequence ?? 0) ? [...previous, event] : previous));
      if (event.type === "run" || event.type === "user_message") {
        void client.invalidateQueries({ queryKey: ["conversations"] });
        void client.invalidateQueries({ queryKey: ["conversation", id] });
      }
    });
    source.addEventListener("authorization_expired", () => {
      source.close();
      void client.invalidateQueries({ queryKey: ["session"] });
    });
    return () => source.close();
  }, [id, client]);
  return events;
}

export const isUnauthorized = (error: unknown) => error instanceof ApiError && error.status === 401;
