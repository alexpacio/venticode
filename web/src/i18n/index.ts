import { useSyncExternalStore } from "react";
import en from "./en.json";

// Italian copy is the message id; English comes from en.json.
export type Language = "it" | "en";
const key = "venticode.language";
const read = (): Language => {
  try { return localStorage.getItem(key) === "en" ? "en" : "it"; } catch { return "it"; }
};
let language = read();
const listeners = new Set<() => void>();
document.documentElement.lang = language;

export function setLanguage(next: Language) {
  language = next;
  try { localStorage.setItem(key, next); } catch { /* storage unavailable: keep in memory */ }
  document.documentElement.lang = next;
  listeners.forEach((listener) => listener());
}

export function t(message: string, ...values: Array<string | number>) {
  const text = language === "en" ? (en as Record<string, string>)[message] ?? message : message;
  return text.replace(/\{(\d+)\}/g, (_, index) => String(values[Number(index)] ?? ""));
}

export function useLanguage() {
  return useSyncExternalStore((listener) => { listeners.add(listener); return () => listeners.delete(listener); }, () => language);
}
