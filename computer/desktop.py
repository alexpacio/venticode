"""Private X11 and Helium bridge for the managed harness."""
import asyncio
import base64
import fcntl
import json
import logging
import os
import re
import secrets
from urllib.parse import urlsplit

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
LOG = logging.getLogger("venticode-desktop")

WIDTH, HEIGHT = 1280, 800
RUNTIME = "/run/venticode-desktop"
SOCKET = RUNTIME + "/bridge.sock"
ORIGINS = set(json.loads(os.getenv("VENTICODE_BROWSER_ORIGINS", "[]")))
KEYS = {
    "Enter": "Return", "Backspace": "BackSpace", "Escape": "Escape",
    "ArrowLeft": "Left", "ArrowRight": "Right", "ArrowUp": "Up", "ArrowDown": "Down",
    "Tab": "Tab", "Delete": "Delete", "Home": "Home", "End": "End",
    "PageUp": "Prior", "PageDown": "Next", "Space": "space",
}
SNAPSHOT = """JSON.stringify({url:location.href,title:document.title,
text:(document.body?.innerText??'').slice(0,10000),
links:Array.from(document.querySelectorAll('a[href]')).slice(0,50).map(a=>({label:a.innerText.slice(0,140),url:a.href}))})"""


class RequestError(Exception):
    pass


async def command(*args, data=None):
    proc = await asyncio.create_subprocess_exec(
        *args, stdin=asyncio.subprocess.PIPE if data is not None else asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
    )
    try:
        output, _ = await asyncio.wait_for(proc.communicate(data), 7)
    finally:
        if proc.returncode is None:
            proc.kill()
            await proc.wait()
    if proc.returncode:
        raise RequestError("Desktop command failed")
    return output


def allowed(url):
    if not isinstance(url, str) or len(url) > 4096 or any(ch.isspace() or ord(ch) < 32 for ch in url):
        return False
    try:
        parsed = urlsplit(url)
        port = parsed.port
    except ValueError:
        return False
    if parsed.scheme not in ("http", "https") or not parsed.hostname or parsed.username is not None:
        return False
    if parsed.hostname in ("127.0.0.1", "localhost") and parsed.scheme == "http" and port and port >= 1024:
        return True
    default = 80 if parsed.scheme == "http" else 443
    origin = f"{parsed.scheme}://{parsed.hostname}" + (f":{port}" if port and port != default else "")
    return origin in ORIGINS


class Browser:
    def __init__(self):
        self.sequence = 0
        self.active = None
        self.sessions = {}
        self.pid = None

    def start(self):
        browser_in, relay_out = os.pipe()
        relay_in, browser_out = os.pipe()
        descriptors = [browser_in, relay_out, relay_in, browser_out]
        source = [fcntl.fcntl(fd, fcntl.F_DUPFD_CLOEXEC, 10) for fd in (browser_in, browser_out)]
        exe = "/opt/helium/helium"
        argv = [
            exe, "--remote-debugging-pipe", "--disable-setuid-sandbox", "--no-first-run",
            "--no-default-browser-check", "--disable-background-networking",
            "--user-data-dir=/data/desktop/helium", "--window-size=1280,760", "about:blank",
        ]
        try:
            self.pid = os.posix_spawn(exe, argv, os.environ, file_actions=[
                (os.POSIX_SPAWN_DUP2, source[0], 3), (os.POSIX_SPAWN_DUP2, source[1], 4),
            ])
            self.read = os.fdopen(os.dup(relay_in), "rb", buffering=0)
            self.write = os.fdopen(os.dup(relay_out), "wb", buffering=0)
        finally:
            for fd in descriptors + source:
                os.close(fd)

    async def wait_exit(self):
        if self.pid is None:
            return 1
        while True:
            pid, status = await asyncio.to_thread(os.waitpid, self.pid, os.WNOHANG)
            if pid == self.pid:
                return status
            await asyncio.sleep(0.5)

    async def connect(self):
        self.reader = asyncio.StreamReader(limit=2 * 1024 * 1024)
        protocol = asyncio.StreamReaderProtocol(self.reader)
        await asyncio.get_running_loop().connect_read_pipe(lambda: protocol, self.read)

    async def call(self, method, params=None, session=None, timeout_sec=6):
        self.sequence += 1
        request = {"id": self.sequence, "method": method, "params": params or {}}
        if session:
            request["sessionId"] = session
        payload = json.dumps(request).encode() + b"\0"
        await asyncio.to_thread(self.write.write, payload)
        async with asyncio.timeout(timeout_sec):
            while True:
                try:
                    raw = await self.reader.readuntil(b"\0")
                except asyncio.LimitOverrunError as err:
                    # Drain the oversized message up to the NUL terminator so future CDP calls stay in sync.
                    await self.reader.readexactly(err.consumed)
                    while True:
                        chunk = await self.reader.read(65536)
                        if not chunk or b"\0" in chunk:
                            break
                    raise RequestError("Browser response exceeded size limit") from err
                reply = json.loads(raw[:-1])
                if reply.get("id") == request["id"]:
                    if "error" in reply:
                        raise RequestError("Helium rejected a browser operation")
                    return reply["result"]

    async def session(self, target):
        if target not in self.sessions:
            reply = await self.call("Target.attachToTarget", {"targetId": target, "flatten": True})
            self.sessions[target] = reply["sessionId"]
        return self.sessions[target]

    async def tabs(self):
        reply = await self.call("Target.getTargets")
        pages = [{"targetId": t["targetId"], "title": t["title"], "url": t["url"]}
                 for t in reply["targetInfos"] if t["type"] == "page"]
        live = {p["targetId"] for p in pages}
        self.sessions = {t: s for t, s in self.sessions.items() if t in live}
        for page in pages:
            target = page["targetId"]
            try:
                state = await self.call("Runtime.evaluate", {
                    "expression": "document.visibilityState", "returnByValue": True,
                }, await self.session(target))
            except RequestError:
                continue
            if state["result"].get("value") == "visible":
                self.active = target
                break
        if self.active not in live:
            self.active = pages[0]["targetId"] if pages else None
        return {"type": "tabs", "tabs": pages, "active": self.active}

    async def select(self, target):
        tabs = await self.tabs()
        if not any(p["targetId"] == target for p in tabs["tabs"]):
            raise RequestError("Unknown browser tab")
        await self.call("Target.activateTarget", {"targetId": target})
        self.active = target
        return {**tabs, "active": target}

    async def navigate(self, url):
        if not allowed(url):
            raise RequestError("Destination is outside the allowed browser origins")
        await self.tabs()
        if self.active is None:
            await self.new_tab()
        result = await self.call("Page.navigate", {"url": url}, await self.session(self.active))
        if result.get("errorText"):
            raise RequestError("Helium navigation failed")
        return {"navigation": "started"}

    async def snapshot(self):
        await self.tabs()
        if self.active is None:
            raise RequestError("No active browser tab")
        result = await self.call("Runtime.evaluate", {"expression": SNAPSHOT, "returnByValue": True},
                                 await self.session(self.active))
        page = json.loads(result["result"]["value"])
        if page["url"] != "about:blank" and not allowed(page["url"]):
            raise RequestError("The active page is outside the allowed browser origins")
        return page

    async def new_tab(self):
        if len((await self.tabs())["tabs"]) >= 6:
            raise RequestError("Browser tab limit reached")
        result = await self.call("Target.createTarget", {"url": "about:blank"})
        return await self.select(result["targetId"])

    async def dispatch(self, op, args):
        if op == "browser_tabs":
            return await self.tabs()
        if op == "browser_snapshot":
            return await self.snapshot()
        if op == "browser_new_tab":
            return await self.new_tab()
        if op == "browser_select_tab":
            return await self.select(args["id"])
        if op == "browser_open":
            return await self.navigate(args["url"])
        if op == "browser_follow_link":
            matches = [item for item in (await self.snapshot())["links"]
                       if item == {"url": args["url"], "label": args["label"]}]
            if not matches:
                raise RequestError("The link changed or is no longer present")
            return await self.navigate(args["url"])
        if op == "browser_open_preview":
            port = args["port"]
            path = args.get("path", "/")
            if type(port) is not int or not 1024 <= port <= 65535 or not path.startswith("/") or path.startswith("//"):
                raise RequestError("Invalid preview address")
            return await self.navigate(f"http://127.0.0.1:{port}{path}")
        raise RequestError("Unknown browser operation")


class Desktop:
    def __init__(self):
        self.buttons = set()
        self.input_lock = asyncio.Lock()
        self.browser_lock = asyncio.Lock()
        self.browser = Browser()

    async def release(self):
        for button in list(self.buttons):
            await command("xdotool", "mouseup", str(button))
            self.buttons.discard(button)
        return True

    async def input(self, args):
        kind = args["kind"]
        if kind == "batch":
            actions = args["actions"]
            if not isinstance(actions, list) or not 1 <= len(actions) <= 12 or any(
                not isinstance(item, dict) or item.get("kind") == "batch" for item in actions
            ):
                raise RequestError("Invalid desktop action batch")
            try:
                for item in actions:
                    await self.input(item)
            finally:
                await self.release()
            return True
        if kind in ("click", "move", "pointer", "scroll"):
            x, y = int(args["x"]), int(args["y"])
            if not 0 <= x < WIDTH or not 0 <= y < HEIGHT:
                raise RequestError("Pointer is outside the display")
            argv = ["xdotool", "mousemove", str(x), str(y)]
            if kind == "scroll":
                delta = float(args["delta"])
                steps = min(10, max(1, int(abs(delta) / 50)))
                argv += ["click", "--repeat", str(steps), "5" if delta > 0 else "4"]
            if kind == "click":
                argv += ["click", "1"]
            if kind == "pointer":
                button = args.get("button")
                if button not in (1, 2, 3) or type(args.get("down")) is not bool:
                    raise RequestError("Invalid mouse button")
                if args["down"]:
                    self.buttons.add(button)
                else:
                    self.buttons.discard(button)
                argv += ["mousedown" if args["down"] else "mouseup", str(button)]
            await command(*argv)
        elif kind == "text":
            text = args["text"]
            if not isinstance(text, str) or not 1 <= len(text.encode()) <= 4096:
                raise RequestError("Invalid pasted text")
            await command("xdotool", "type", "--clearmodifiers", "--delay", "1", "--", text)
        elif kind == "key":
            key = args["key"]
            parts = key.split("+")
            name = KEYS.get(parts[-1], parts[-1] if re.fullmatch(r"[a-zA-Z0-9]|F(?:[1-9]|1[0-2])", parts[-1]) else "")
            if not name or not all(item in ("ctrl", "alt", "shift", "super") for item in parts[:-1]) or len(parts) > 5:
                raise RequestError("Invalid keyboard shortcut")
            await command("xdotool", "key", "--clearmodifiers", "+".join(parts[:-1] + [name]))
        else:
            raise RequestError("Invalid desktop input")
        return True

    async def handle(self, reader, writer):
        op = "<unknown>"
        try:
            request = json.loads(await asyncio.wait_for(reader.readline(), 3))
            op, args = request["operation"], request["arguments"]
            if op == "desktop_frame":
                frame = await command("ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "x11grab",
                                      "-video_size", f"{WIDTH}x{HEIGHT}", "-i", ":0", "-frames:v", "1",
                                      "-threads", "1", "-f", "image2pipe", "-vcodec", "mjpeg", "pipe:1")
                if not frame or len(frame) > 1024 * 1024:
                    raise RequestError("Invalid desktop frame")
                value = {"width": WIDTH, "height": HEIGHT, "jpeg": base64.b64encode(frame).decode()}
            elif op == "desktop_input":
                async with self.input_lock:
                    value = await self.input(args)
            elif op == "desktop_release":
                async with self.input_lock:
                    value = await self.release()
            else:
                async with self.browser_lock:
                    value = await self.browser.dispatch(op, args)
            reply = {"value": value}
        except RequestError as error:
            reply = {"error": str(error)}
        except (TypeError, ValueError, KeyError, AttributeError, OSError, TimeoutError, EOFError, asyncio.LimitOverrunError) as error:
            LOG.warning("operation %s failed: %s: %s", op, type(error).__name__, error)
            reply = {"error": "Desktop or browser operation unavailable"}
        try:
            writer.write(json.dumps(reply).encode() + b"\n")
            await writer.drain()
        finally:
            writer.close()
            await writer.wait_closed()


async def main():
    os.environ["DISPLAY"] = ":0"
    os.environ["XAUTHORITY"] = RUNTIME + "/Xauthority"
    with open(os.environ["XAUTHORITY"], "a", encoding="utf-8"):
        pass
    await command("xauth", "add", ":0", ".", secrets.token_hex(16))
    x11 = await asyncio.create_subprocess_exec(
        "Xvfb", ":0", "-screen", "0", f"{WIDTH}x{HEIGHT}x24", "-nolisten", "tcp", "-auth", os.environ["XAUTHORITY"],
    )
    for _ in range(40):
        try:
            await command("xdotool", "getdisplaygeometry")
            break
        except RequestError:
            await asyncio.sleep(0.25)
    else:
        raise RuntimeError("Xvfb did not start")
    os.environ["DBUS_SESSION_BUS_ADDRESS"] = (await command("dbus-daemon", "--session", "--fork", "--print-address")).decode().strip()
    xfce = await asyncio.create_subprocess_exec("xfce4-session")
    desktop = Desktop()
    desktop.browser.start()
    await desktop.browser.connect()
    await desktop.browser.call("Browser.getVersion", timeout_sec=20)
    if os.path.exists(SOCKET):
        os.unlink(SOCKET)
    server = await asyncio.start_unix_server(desktop.handle, SOCKET, limit=64 * 1024)
    LOG.info("desktop bridge ready on %s", SOCKET)
    async with server:
        tasks = [
            asyncio.create_task(x11.wait()),
            asyncio.create_task(xfce.wait()),
            asyncio.create_task(desktop.browser.wait_exit()),
        ]
        _, pending = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        for task in pending:
            task.cancel()
    raise RuntimeError("Desktop session exited")


if __name__ == "__main__":
    asyncio.run(main())
