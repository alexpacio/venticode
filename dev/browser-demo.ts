import { createRequire } from "node:module";

const require = createRequire(new URL("../web/package.json", import.meta.url));
const { WebSocketServer } = require("ws");
const { createCanvas } = require("@napi-rs/canvas");

const width = 960;
const height = 600;
const site = "https://status.example.test";
type Tab = { targetId: string; title: string; url: string; note: string; saved: string; scroll: number; focused: boolean };
type Message = { role: string; content: string };

export function createBrowserDemo() {
  const server = new WebSocketServer({ noServer: true, maxPayload: 24 * 1024 });
  const conversations = new Map<string, { messages: Message[]; updated_at: string }>();
  const live = new Set<string>();

  server.on("connection", (socket, thread: string) => {
    const send = (packet: object) => socket.send(JSON.stringify(packet));
    if (live.has(thread)) {
      send({ type: "ended", reason: "busy" });
      socket.close();
      return;
    }
    live.add(thread);
    let human = false;
    let task: ReturnType<typeof setTimeout> | undefined;
    let prompt = "";
    const tabs: Tab[] = [];
    function newTab(url = "about:blank") {
      const tab = {
        targetId: String(tabs.length + 1), title: title(url), url,
        note: "", saved: "", scroll: 0, focused: false,
      };
      tabs.push(tab);
      return tab;
    }
    newTab(site + "/metrics");
    let active = newTab(site);
    const messages = conversations.get(thread)?.messages ?? [];
    const writeHistory = () => send({ type: "history", messages });
    const sendTabs = () => send({
      type: "tabs", active: active.targetId,
      tabs: tabs.map(({ targetId, title, url }) => ({ targetId, title, url })),
    });
    const canvas = createCanvas(width, height);
    const ctx = canvas.getContext("2d");
    function show() {
      ctx.fillStyle = "#eff4f7";
      ctx.fillRect(0, 0, width, height);
      ctx.fillStyle = "#152c42";
      ctx.fillRect(0, 0, width, 70);
      label("NORTHSTAR  /  RELEASE STATUS", 48, 43, 19, "#ffffff", true);
      label("SYNTHETIC DEMO", 728, 43, 14, "#b4c6d5", true);
      const metrics = active.url.endsWith("/metrics");
      ctx.save();
      ctx.beginPath();
      ctx.rect(0, 70, width, height - 70);
      ctx.clip();
      ctx.translate(0, -active.scroll);
      label(active.title, 48, 141, 34, "#152c42", true);
      if (active.url !== "about:blank") {
        label(metrics ? "Release metrics" : "Production · Release 2.18.4", 48, 174, 18, "#4b647a");
        tile(48, metrics ? "Availability" : "API", metrics ? "99.99%" : "Operational");
        tile(341, metrics ? "P95 latency" : "Workers", metrics ? "134 ms" : "Operational");
        tile(634, metrics ? "Error rate" : "Database", metrics ? "0.02%" : "Operational");
        label("Release note", 48, 375, 21, "#152c42", true);
        ctx.fillStyle = "#ffffff"; ctx.fillRect(48, 400, 695, 54);
        ctx.strokeStyle = active.focused ? "#1e69b6" : "#9eb0bf";
        ctx.lineWidth = 2; ctx.strokeRect(49, 401, 693, 52);
        label(active.note.slice(-68) || "Type a note to try remote input", 64, 435, 18, active.note ? "#152c42" : "#516a80");
        ctx.fillStyle = "#1f5f95"; ctx.fillRect(765, 400, 148, 54);
        label("Save note", 795, 434, 17, "#ffffff", true);
        if (active.saved) label("Saved: " + active.saved.slice(0, 62), 48, 498, 18, "#176547");
        label("Activity", 48, 595, 22, "#152c42", true);
        label("14:12 · Release 2.18.4 reached production", 48, 642, 18, "#4b647a");
      }
      ctx.restore();
      const header = JSON.stringify({ type: "frame", tab: active.targetId, width, height }) + "\n";
      socket.send(Buffer.concat([Buffer.from(header), canvas.toBuffer("image/jpeg")]));
    }
    function label(text: string, x: number, y: number, size: number, color: string, bold = false) {
      ctx.font = `${bold ? "bold " : ""}${size}px sans-serif`;
      ctx.fillStyle = color;
      ctx.fillText(text, x, y);
    }
    function tile(x: number, name: string, value: string) {
      ctx.fillStyle = "#ffffff"; ctx.fillRect(x, 212, 279, 104);
      label(name, x + 20, 244, 16, "#536d83", true);
      label(value, x + 20, 286, 25, "#176547", true);
    }
    function run() {
      send({ type: "working" });
      task = setTimeout(() => {
        messages.push({ role: "assistant", content: "Release 2.18.4 is live. The API, workers, and database are operational. The metrics tab shows 99.99% availability and 134 ms p95 latency." });
        conversations.set(thread, { messages, updated_at: new Date().toISOString() });
        prompt = "";
        writeHistory();
        send({ type: "idle" });
      }, 1400);
    }
    writeHistory();
    send({ type: "ready" });
    sendTabs();
    show();
    socket.on("message", (data) => {
      let command;
      try { command = JSON.parse(data.toString()); } catch { return; }
      if (command.type === "end") {
        send({ type: "ended", reason: "closed" });
        socket.close();
        return;
      }
      if (command.type === "ask" && !human && !prompt && typeof command.prompt === "string") {
        prompt = command.prompt;
        messages.push({ role: "user", content: prompt });
        conversations.set(thread, { messages, updated_at: new Date().toISOString() });
        writeHistory();
        run();
        return;
      }
      if (command.type === "take" || command.type === "release") {
        human = command.type === "take";
        clearTimeout(task);
        send({ type: "control", human });
        if (!human && prompt) run();
        return;
      }
      if (!human) return;
      if (command.type === "new_tab") active = newTab();
      else if (command.type === "tab") active = tabs.find((tab) => tab.targetId === command.id) ?? active;
      else if (command.type === "open") {
        if (![site, site + "/", site + "/metrics"].includes(command.url)) {
          send({ type: "error", status: 403 });
          return;
        }
        Object.assign(active, { title: title(command.url), url: command.url, scroll: 0, focused: false });
      } else if (command.type === "input" && command.tab === active.targetId && command.input) {
        const { kind, x, y, delta, text, key } = command.input;
        if (kind === "click") {
          const row = y + active.scroll;
          active.focused = row >= 400 && row <= 454 && x >= 48 && x <= 743;
          if (row >= 400 && row <= 454 && x >= 765 && x <= 913) active.saved = active.note;
        }
        if (kind === "text" && active.focused && typeof text === "string") active.note = (active.note + text).slice(0, 128);
        if (kind === "key" && active.focused && key === "Backspace") active.note = active.note.slice(0, -1);
        if (kind === "key" && active.focused && key === "Enter") active.saved = active.note;
        if (kind === "scroll" && Number.isFinite(delta)) active.scroll = Math.max(0, Math.min(140, active.scroll + delta));
        show();
        return;
      } else return;
      sendTabs();
      show();
    });
    socket.once("close", () => {
      clearTimeout(task);
      live.delete(thread);
    });
  });

  return {
    threads: () => [...conversations.entries()]
      .map(([id, { updated_at }]) => ({ id, updated_at }))
      .sort((a, b) => b.updated_at.localeCompare(a.updated_at)),
    upgrade: server.handleUpgrade.bind(server),
    connected: (socket, thread: string) => server.emit("connection", socket, thread),
    close: () => { for (const socket of server.clients) socket.terminate(); },
  };
}

function title(url: string) {
  return url === "about:blank" ? "New tab" : url.endsWith("/metrics") ? "Metrics" : "Deployment overview";
}
