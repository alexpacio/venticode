// The web app is the WebAssembly build of venticode-tui. This shell only
// paints the cells it returns and forwards keyboard, paste, mouse and resize
// events; every screen, string and API call lives in the Rust core.
import init, { Terminal } from "./generated/terminal/venticode_tui_browser.js";
import "./styles.css";

type Span = [string, string, string, number];
type Image = { x: number; y: number; w: number; h: number; serial: number; width: number; height: number; human: boolean; desktop: boolean };
type Payload = { rows: Span[][]; cursor: [number, number] | null; image: Image | null };

const root = document.querySelector<HTMLElement>(".terminal")!;
const screen = document.querySelector<HTMLElement>("#screen")!;
const ruler = document.querySelector<HTMLElement>(".cell-ruler")!;
const cursor = document.querySelector<HTMLElement>(".cursor")!;
const remote = document.querySelector<HTMLImageElement>(".remote")!;
const keys = document.querySelector<HTMLTextAreaElement>(".keys")!;
const boot = document.querySelector<HTMLElement>(".boot")!;
const LANGUAGE = "venticode.language";

let cell = { width: 8.4, height: 14 };
let image: Image | null = null;
let imageUrl = "";

const storage = {
  get(key: string) {
    try { return localStorage.getItem(key); } catch { return null; }
  },
  set(key: string, value: string) {
    try { localStorage.setItem(key, value); } catch { /* storage unavailable */ }
  },
};

function measure() {
  const box = ruler.getBoundingClientRect();
  if (box.width && box.height) cell = { width: box.width, height: box.height };
  return {
    width: Math.max(1, Math.floor(screen.clientWidth / cell.width)),
    height: Math.max(1, Math.floor(screen.clientHeight / cell.height)),
  };
}

// Vertical box-drawing glyphs are stretched to the row height so borders
// and pane separators stay continuous with the taller line height.
const escape = (text: string) =>
  text
    .replace(/[&<>]/g, (c) => (c === "&" ? "&amp;" : c === "<" ? "&lt;" : "&gt;"))
    .replace(/[│╭╮╰╯]+/g, (run) => `<i class="box">${run}</i>`);

function draw(json: string) {
  const frame = JSON.parse(json) as Payload;
  let html = "";
  for (const row of frame.rows) {
    html += "<div>";
    for (const [fg, bg, text, flags] of row) {
      const style = [
        fg && `color:${fg}`,
        bg && `background:${bg}`,
        flags & 1 && "font-weight:700",
        flags & 2 && "font-style:italic",
        flags & 4 && "text-decoration:line-through",
      ].filter(Boolean).join(";");
      html += style ? `<span style="${style}">${escape(text)}</span>` : escape(text);
    }
    html += "</div>";
  }
  screen.innerHTML = html;
  const left = screen.offsetLeft;
  const top = screen.offsetTop;
  if (frame.cursor) {
    const [x, y] = frame.cursor;
    Object.assign(cursor.style, { left: `${left + x * cell.width}px`, top: `${top + y * cell.height}px` });
    Object.assign(keys.style, { left: `${left + x * cell.width}px`, top: `${top + y * cell.height}px` });
    cursor.hidden = false;
  } else {
    cursor.hidden = true;
  }
  image = frame.image;
  remote.hidden = !image || !imageUrl;
  if (image) {
    Object.assign(remote.style, {
      left: `${left + image.x * cell.width}px`,
      top: `${top + image.y * cell.height}px`,
      width: `${image.w * cell.width}px`,
      height: `${image.h * cell.height}px`,
    });
    remote.classList.toggle("human", image.human);
  }
}

const hooks = {
  draw,
  image(bytes: Uint8Array) {
    const previous = imageUrl;
    imageUrl = URL.createObjectURL(new Blob([bytes as BlobPart], { type: "image/jpeg" }));
    remote.src = imageUrl;
    remote.hidden = !image;
    if (previous) setTimeout(() => URL.revokeObjectURL(previous), 1000);
  },
  copy(text: string) {
    void navigator.clipboard?.writeText(text).catch(() => {});
  },
  open(url: string) {
    window.open(url, "_blank", "noopener,noreferrer");
  },
  navigate(url: string) {
    location.assign(url);
  },
  location(path: string) {
    if (path !== location.pathname) history.replaceState(null, "", path);
  },
  language(en: boolean) {
    storage.set(LANGUAGE, en ? "en" : "it");
    document.documentElement.lang = en ? "en" : "it";
  },
  signedOut(redirect: string, portal: string) {
    if (window.parent !== window) {
      try {
        window.parent.postMessage({ type: "venticode:signed-out" }, new URL(portal).origin);
      } catch { /* invalid portal URL */ }
      location.replace("/");
      return;
    }
    location.assign(redirect || "/");
  },
  signIn(dev: boolean) {
    const browser = location.pathname === "/browser" ? "?browser=1" : "";
    location.assign((dev ? "/__dev/login" : "/venticode/login") + browser);
  },
};

/** Map a pointer position on the remote image to remote pixels (object-fit: contain). */
function remotePoint(event: MouseEvent | WheelEvent) {
  if (!image) return null;
  const box = remote.getBoundingClientRect();
  const scale = Math.min(box.width / image.width, box.height / image.height);
  const x = (event.clientX - box.left - (box.width - image.width * scale) / 2) / scale;
  const y = (event.clientY - box.top - (box.height - image.height * scale) / 2) / scale;
  if (x < 0 || y < 0 || x >= image.width || y >= image.height) return null;
  return { x, y };
}

async function main() {
  const en = (storage.get(LANGUAGE) ?? (navigator.language.startsWith("it") ? "it" : "en")) === "en";
  document.documentElement.lang = en ? "en" : "it";
  await document.fonts?.ready;
  await init();
  const size = measure();
  const term = new Terminal(hooks, en, import.meta.env.DEV, size.width, size.height);
  boot.remove();
  term.start(location.pathname + location.search);

  new ResizeObserver(() => {
    const next = measure();
    term.resize(next.width, next.height);
  }).observe(screen);

  const focus = () => keys.focus({ preventScroll: true });
  focus();
  root.addEventListener("mousedown", () => setTimeout(focus));
  window.addEventListener("focus", focus);

  keys.addEventListener("keydown", (event) => {
    if (event.isComposing) return;
    const ctrl = event.ctrlKey || event.metaKey;
    // Let the browser paste (the paste event carries the text) and reload.
    if (ctrl && ["v", "V"].includes(event.key)) return;
    if (event.key === "F5" || (ctrl && event.key.toLowerCase() === "r") || (ctrl && event.shiftKey && event.key === "I")) return;
    if (term.key(event.key, event.ctrlKey, event.altKey, event.shiftKey, event.metaKey)) event.preventDefault();
  });
  keys.addEventListener("compositionend", (event) => {
    if (event.data) term.paste(event.data);
    keys.value = "";
  });
  keys.addEventListener("input", () => {
    if (!(keys as unknown as { isComposing?: boolean }).isComposing) keys.value = "";
  });
  keys.addEventListener("paste", (event) => {
    event.preventDefault();
    const text = event.clipboardData?.getData("text/plain");
    if (text) term.paste(text);
  });

  const at = (event: MouseEvent) => {
    const box = screen.getBoundingClientRect();
    return {
      x: Math.max(0, Math.floor((event.clientX - box.left) / cell.width)),
      y: Math.max(0, Math.floor((event.clientY - box.top) / cell.height)),
    };
  };
  let dragging = false;
  screen.addEventListener("mousedown", (event) => {
    const { x, y } = at(event);
    dragging = true;
    term.mouse("down", event.button, x, y);
  });
  window.addEventListener("mousemove", (event) => {
    if (!dragging) return;
    const { x, y } = at(event);
    term.mouse("drag", 0, x, y);
  });
  window.addEventListener("mouseup", (event) => {
    if (!dragging) return;
    dragging = false;
    const { x, y } = at(event);
    term.mouse("up", event.button, x, y);
  });
  screen.addEventListener("wheel", (event) => {
    event.preventDefault();
    const { x, y } = at(event);
    const steps = Math.max(1, Math.min(5, Math.round(Math.abs(event.deltaY) / 40)));
    for (let i = 0; i < steps; i++) term.mouse(event.deltaY < 0 ? "wheelup" : "wheeldown", 0, x, y);
  }, { passive: false });
  let touchY = 0;
  screen.addEventListener("touchstart", (event) => { touchY = event.touches[0].clientY; }, { passive: true });
  screen.addEventListener("touchmove", (event) => {
    const y = event.touches[0].clientY;
    const steps = Math.trunc((y - touchY) / cell.height);
    if (!steps) return;
    touchY = y;
    const { x } = at(event.touches[0] as unknown as MouseEvent);
    for (let i = 0; i < Math.abs(steps); i++) term.mouse(steps > 0 ? "wheelup" : "wheeldown", 0, x, 0);
  }, { passive: true });

  // The remote screen takes pointer input only while you have control.
  const button = (b: number) => (b === 1 ? 2 : b === 2 ? 3 : 1);
  let lastMove = 0;
  remote.addEventListener("mousedown", (event) => {
    event.preventDefault();
    const point = remotePoint(event);
    if (!point || !image) return;
    term.remote(JSON.stringify(image.desktop
      ? { kind: "pointer", ...point, button: button(event.button), down: true }
      : { kind: "click", ...point }));
  });
  remote.addEventListener("mouseup", (event) => {
    const point = remotePoint(event);
    if (point && image?.desktop) term.remote(JSON.stringify({ kind: "pointer", ...point, button: button(event.button), down: false }));
  });
  remote.addEventListener("mousemove", (event) => {
    const now = performance.now();
    if (!image?.desktop || now - lastMove < 33) return;
    lastMove = now;
    const point = remotePoint(event);
    if (point) term.remote(JSON.stringify({ kind: "move", ...point }));
  });
  remote.addEventListener("wheel", (event) => {
    event.preventDefault();
    const point = remotePoint(event);
    if (point) term.remote(JSON.stringify({ kind: "scroll", ...point, delta: Math.max(-1500, Math.min(1500, event.deltaY)) }));
  }, { passive: false });
  remote.addEventListener("contextmenu", (event) => event.preventDefault());
}

main().catch((error: Error) => {
  boot.classList.add("error");
  boot.textContent = `Venticode: ${error.message || error}`;
});
