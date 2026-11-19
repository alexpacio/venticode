import { useEffect, useLayoutEffect, useRef } from "react";
import type { RefObject } from "react";
import init, { RemoteScreen } from "../generated/terminal/venticode_remote_screen.js";
import { t } from "../i18n/index.ts";

let moduleReady: ReturnType<typeof init> | undefined;

export type BrowserFrame = {
  tab: string;
  jpeg: Blob;
  width: number;
  height: number;
};

type ScreenProps = {
  frame: BrowserFrame | null;
  human: boolean;
  exit: RefObject<HTMLButtonElement | null>;
  onInput: (tab: string, input: object) => void;
  onFailure: () => void;
};

export function BrowserScreen({ frame, human, exit, onInput, onFailure }: ScreenProps) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const present = useRef<(next: BrowserFrame | null) => void>(ignore);
  const control = useRef<(human: boolean) => void>(ignore);
  const current = useRef({ frame, human, onInput, onFailure });
  useLayoutEffect(() => { current.current = { frame, human, onInput, onFailure }; });

  useEffect(() => {
    const element = canvas.current!;
    const scratch = document.createElement("canvas").getContext("2d", { willReadFrequently: true });
    let disposed = false;
    let screen: RemoteScreen | undefined;
    let pending: BrowserFrame | null = null;
    let tab: string | undefined;
    let decoding = false;
    let started = false;

    function destroy() {
      screen?.destroy();
      screen?.free();
      screen = undefined;
    }

    async function draw() {
      if (disposed || decoding || !pending || !scratch || !screen) return;
      const next = pending;
      pending = null;
      decoding = true;
      try {
        const image = await createImageBitmap(next.jpeg);
        try {
          if (disposed || !screen || current.current.frame?.tab !== next.tab) return;
          scratch.canvas.width = image.width;
          scratch.canvas.height = image.height;
          scratch.drawImage(image, 0, 0);
          const pixels = scratch.getImageData(0, 0, image.width, image.height).data;
          screen.set_frame(new Uint8Array(pixels.buffer), image.width, image.height, next.width, next.height);
          tab = next.tab;
        } finally {
          image.close();
        }
      } catch {
        if (!disposed) current.current.onFailure();
      } finally {
        decoding = false;
        if (pending) void draw();
      }
    }

    // Let the first development-mode effect clean up before binding a canvas.
    const starting = window.setTimeout(async () => {
      try {
        if (!scratch) throw new Error("2D canvas unavailable");
        await (moduleReady ??= init().catch((error) => {
          moduleReady = undefined;
          throw error;
        }));
        if (disposed) return;
        screen = new RemoteScreen();
        await screen.start(element, (payload: string) => {
          if (tab && current.current.human && current.current.frame?.tab === tab)
            current.current.onInput(tab, JSON.parse(payload) as object);
        });
        if (disposed) {
          destroy();
          return;
        }
        started = true;
        control.current = (allowed) => {
          element.tabIndex = allowed ? 0 : -1;
          screen?.set_control(allowed);
        };
        control.current(current.current.human);
        present.current = (next) => {
          pending = next;
          if (tab && tab !== next?.tab) {
            tab = undefined;
            screen?.clear();
          }
          void draw();
        };
        present.current(current.current.frame);
      } catch {
        destroy();
        if (!disposed) current.current.onFailure();
      }
    }, 0);

    return () => {
      disposed = true;
      window.clearTimeout(starting);
      present.current = ignore;
      control.current = ignore;
      if (started) destroy();
    };
  }, []);

  useEffect(() => present.current(frame), [frame]);
  useEffect(() => control.current(human), [human]);
  return <canvas
    ref={canvas}
    className="browser-canvas"
    data-controlled={human}
    aria-label={t("Schermo del browser remoto")}
    tabIndex={-1}
    onKeyDownCapture={(event) => {
      if (event.key === "Tab" && event.shiftKey) {
        event.preventDefault();
        event.stopPropagation();
        exit.current?.focus();
      }
    }}
  />;
}

function ignore() {}
