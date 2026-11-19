"""Private X11 and Helium bridge for the managed harness."""
import asyncio
import base64
import fcntl
import json
import logging
import math
import os
import re
import secrets
from urllib.parse import urlsplit

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
LOG = logging.getLogger("venticode-desktop")

WIDTH, HEIGHT = 1280, 800
RUNTIME = "/run/venticode-desktop"
SOCKET = RUNTIME + "/bridge.sock"
STDERR_LIMIT = 4096
KEYS = {
    "Enter": "Return", "Backspace": "BackSpace", "Escape": "Escape",
    "ArrowLeft": "Left", "ArrowRight": "Right", "ArrowUp": "Up", "ArrowDown": "Down",
    "Tab": "Tab", "Delete": "Delete", "Home": "Home", "End": "End",
    "PageUp": "Prior", "PageDown": "Next", "Space": "space",
}
SNAPSHOT = """(() => ({
title: String(document.title ?? ''),
text: String(document.body?.innerText ?? '').slice(0, 10000),
links: Array.from(document.querySelectorAll('a[href]')).slice(0, 50).map(a => ({
label: String(a.innerText ?? '').slice(0, 140),
url: String(a.href ?? ''),
})),
}))()"""


class RequestError(Exception):
    pass


def normalize_origin(raw):
    if not isinstance(raw, str) or len(raw) > 4096 or any(ch.isspace() or ord(ch) < 32 for ch in raw):
        return None
    try:
        parsed = urlsplit(raw)
        port = parsed.port
    except ValueError:
        return None
    scheme = parsed.scheme.lower()
    host = parsed.hostname.lower() if parsed.hostname else None
    if scheme not in ("http", "https") or not host or parsed.username is not None or parsed.password is not None:
        return None
    if ":" in host:
        host = f"[{host}]"
    default = 80 if scheme == "http" else 443
    return f"{scheme}://{host}" + (f":{port}" if port and port != default else "")


def load_origins(values):
    if not isinstance(values, (list, tuple, set)):
        return set()
    origins = set()
    for value in values:
        origin = normalize_origin(value)
        if origin is not None:
            origins.add(origin)
    return origins


ORIGINS = load_origins(json.loads(os.getenv("VENTICODE_BROWSER_ORIGINS", "[]")))


async def _read_bounded(stream, limit):
    chunks = []
    size = 0
    while True:
        chunk = await stream.read(4096)
        if not chunk:
            break
        if size < limit:
            take = chunk[:limit - size]
            chunks.append(take)
            size += len(take)
    return b"".join(chunks)


async def command(*args, data=None):
    proc = await asyncio.create_subprocess_exec(
        *args, stdin=asyncio.subprocess.PIPE if data is not None else asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
    )

    async def feed_stdin():
        if data is not None and proc.stdin is not None:
            try:
                proc.stdin.write(data)
                await proc.stdin.drain()
            except (BrokenPipeError, ConnectionResetError):
                pass
            finally:
                proc.stdin.close()

    try:
        async with asyncio.timeout(7):
            _, output, stderr = await asyncio.gather(
                feed_stdin(),
                proc.stdout.read(),
                _read_bounded(proc.stderr, STDERR_LIMIT),
            )
            await proc.wait()
    finally:
        if proc.returncode is None:
            proc.kill()
            await proc.wait()
    if proc.returncode:
        detail = stderr.decode("utf-8", errors="replace").strip()
        LOG.warning(
            "command %s exited with %s%s",
            args[0] if args else "<empty>",
            proc.returncode,
            f": {detail}" if detail else "",
        )
        raise RequestError("Desktop command failed")
    return output


def allowed(url):
    origin = normalize_origin(url)
    if origin is None:
        return False
    parsed = urlsplit(url)
    port = parsed.port
    if parsed.hostname in ("127.0.0.1", "localhost") and parsed.scheme == "http" and port and port >= 1024:
        return True
    return origin in ORIGINS


def _finite_float(value, name):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise RequestError(f"Invalid desktop {name}")
    try:
        number = float(value)
    except OverflowError as error:
        raise RequestError(f"Invalid desktop {name}") from error
    if not math.isfinite(number):
        raise RequestError(f"Invalid desktop {name}")
    return number


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

    def _evict_session(self, session_id):
        for target_id, sid in list(self.sessions.items()):
            if sid == session_id:
                self.sessions.pop(target_id, None)

    async def call(self, method, params=None, session=None, timeout_sec=6):
        self.sequence += 1
        request = {"id": self.sequence, "method": method, "params": params or {}}
        if session:
            request["sessionId"] = session
        payload = json.dumps(request).encode() + b"\0"
        async with asyncio.timeout(timeout_sec):
            await asyncio.to_thread(self.write.write, payload)
            while True:
                try:
                    raw = await self.reader.readuntil(b"\0")
                except asyncio.LimitOverrunError as err:
                    # Drain the oversized message up to the NUL terminator without consuming
                    # bytes belonging to the next CDP frame.
                    await self.reader.readexactly(err.consumed)
                    while True:
                        try:
                            await self.reader.readuntil(b"\0")
                            break
                        except asyncio.LimitOverrunError as drain_err:
                            await self.reader.readexactly(drain_err.consumed)
                    raise RequestError("Browser response exceeded size limit") from err
                reply = json.loads(raw[:-1])
                if reply.get("method") == "Target.detachedFromTarget":
                    detached = reply.get("params", {}).get("sessionId")
                    if detached:
                        self._evict_session(detached)
                if reply.get("id") == request["id"]:
                    if "error" in reply:
                        if session is not None:
                            self._evict_session(session)
                        raise RequestError("Helium rejected a browser operation")
                    return reply["result"]

    async def session(self, target):
        if target not in self.sessions:
            reply = await self.call("Target.attachToTarget", {"targetId": target, "flatten": True})
            self.sessions[target] = reply["sessionId"]
        return self.sessions[target]

    async def call_target(self, target, method, params=None, timeout_sec=6):
        session = await self.session(target)
        try:
            return await self.call(method, params, session, timeout_sec=timeout_sec)
        except (RequestError, TimeoutError):
            self.sessions.pop(target, None)
            raise

    async def _target_url(self, target):
        info = await self.call("Target.getTargetInfo", {"targetId": target})
        url = info.get("targetInfo", {}).get("url")
        if not isinstance(url, str):
            raise RequestError("Unable to determine active page URL")
        return url

    async def tabs(self):
        reply = await self.call("Target.getTargets")
        pages = [{"targetId": t["targetId"], "title": t["title"], "url": t["url"]}
                 for t in reply["targetInfos"] if t["type"] == "page"]
        live = {p["targetId"] for p in pages}
        self.sessions = {t: s for t, s in self.sessions.items() if t in live}
        for page in pages:
            target = page["targetId"]
            try:
                state = await self.call_target(target, "Runtime.evaluate", {
                    "expression": "document.visibilityState", "returnByValue": True,
                }, timeout_sec=1.5)
            except (RequestError, TimeoutError):
                continue
            if state.get("result", {}).get("value") == "visible":
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
        result = await self.call_target(self.active, "Page.navigate", {"url": url})
        if result.get("errorText"):
            raise RequestError("Helium navigation failed")
        return {"navigation": "started"}

    async def snapshot(self):
        await self.tabs()
        if self.active is None:
            raise RequestError("No active browser tab")
        target = self.active
        before_url = await self._target_url(target)
        if before_url != "about:blank" and not allowed(before_url):
            raise RequestError("The active page is outside the allowed browser origins")
        result = await self.call_target(target, "Runtime.evaluate", {
            "expression": SNAPSHOT,
            "returnByValue": True,
        })
        after_url = await self._target_url(target)
        if after_url != "about:blank" and not allowed(after_url):
            raise RequestError("The active page is outside the allowed browser origins")
        value = result.get("result", {}).get("value")
        if not isinstance(value, dict):
            raise RequestError("Unable to snapshot active page")
        title = value.get("title")
        text = value.get("text")
        raw_links = value.get("links")
        if not isinstance(title, str) or not isinstance(text, str) or not isinstance(raw_links, list):
            raise RequestError("Unable to snapshot active page")
        links = [
            {"label": item["label"][:140], "url": item["url"]}
            for item in raw_links[:50]
            if isinstance(item, dict) and isinstance(item.get("label"), str) and isinstance(item.get("url"), str)
        ]
        return {"url": after_url, "title": title, "text": text[:10000], "links": links}

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
        self.frame_lock = asyncio.Lock()
        self.input_lock = asyncio.Lock()
        self.browser_lock = asyncio.Lock()
        self.browser = Browser()

    async def release(self):
        failed = None
        for button in list(self.buttons):
            try:
                await command("xdotool", "mouseup", str(button))
            except (RequestError, TimeoutError, OSError) as error:
                failed = error
            else:
                self.buttons.discard(button)
        if failed is not None:
            raise failed
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
            x = int(_finite_float(args["x"], "coordinate"))
            y = int(_finite_float(args["y"], "coordinate"))
            if not 0 <= x < WIDTH or not 0 <= y < HEIGHT:
                raise RequestError("Pointer is outside the display")
            argv = ["xdotool", "mousemove", str(x), str(y)]
            if kind == "scroll":
                delta = _finite_float(args["delta"], "scroll delta")
                steps = min(10, max(1, int(min(500.0, abs(delta)) / 50)))
                argv += ["click", "--repeat", str(steps), "5" if delta > 0 else "4"]
            if kind == "click":
                argv += ["click", "1"]
            if kind == "pointer":
                button = args.get("button")
                if type(button) is not int or button not in (1, 2, 3) or type(args.get("down")) is not bool:
                    raise RequestError("Invalid mouse button")
                argv += ["mousedown" if args["down"] else "mouseup", str(button)]
            await command(*argv)
            if kind == "pointer":
                if args["down"]:
                    self.buttons.add(button)
                else:
                    self.buttons.discard(button)
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
                async with self.frame_lock:
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
        except (TypeError, ValueError, OverflowError, KeyError, AttributeError, OSError, TimeoutError, EOFError, asyncio.LimitOverrunError) as error:
            LOG.warning("operation %s failed: %s: %s", op, type(error).__name__, error)
            reply = {"error": "Desktop or browser operation unavailable"}
        try:
            writer.write(json.dumps(reply).encode() + b"\n")
            async with asyncio.timeout(3):
                await writer.drain()
        except (OSError, TimeoutError) as error:
            LOG.warning("failed to write reply for %s: %s: %s", op, type(error).__name__, error)
        finally:
            writer.close()
            try:
                async with asyncio.timeout(1):
                    await writer.wait_closed()
            except (OSError, TimeoutError):
                pass


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
    try:
        os.unlink(SOCKET)
    except FileNotFoundError:
        pass
    server = await asyncio.start_unix_server(desktop.handle, SOCKET, limit=64 * 1024)
    LOG.info("desktop bridge ready on %s", SOCKET)
    try:
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
    finally:
        try:
            os.unlink(SOCKET)
        except FileNotFoundError:
            pass


if __name__ == "__main__":
    asyncio.run(main())
