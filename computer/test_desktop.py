"""Smoke-test the real Helium bridge inside the disposable computer image."""
import base64
import json
import socket
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


class Fixture(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.end_headers()
        if self.path == "/details":
            self.wfile.write(b"<title>Details</title><p>Fixture details</p>")
        else:
            self.wfile.write(b'<title>Preview</title><a href="/details">Details</a><p>Fixture preview</p>')


def eventually(predicate):
    for _ in range(60):
        try:
            if result := predicate():
                return result
        except (ConnectionError, FileNotFoundError, AssertionError):
            pass
        time.sleep(0.25)
    raise AssertionError("expected desktop state did not appear")


def rpc(operation, **arguments):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(12)
        connection.connect("/run/venticode-desktop/bridge.sock")
        connection.sendall(json.dumps({"operation": operation, "arguments": arguments}).encode() + b"\n")
        response = json.loads(connection.makefile("rb").readline())
    assert "error" not in response, response
    return response["value"]


def snapshot(title):
    page = rpc("browser_snapshot")
    return page if page["title"] == title else None


def main():
    with ThreadingHTTPServer(("127.0.0.1", 0), Fixture) as server:
        threading.Thread(target=server.serve_forever, daemon=True).start()
        initial = eventually(lambda: rpc("browser_tabs"))
        origin = f"http://127.0.0.1:{server.server_port}"
        tab = rpc("browser_new_tab")["active"]
        assert rpc("browser_select_tab", id=tab)["active"] == tab
        rpc("browser_open_preview", port=server.server_port)
        assert eventually(lambda: snapshot("Preview"))["text"].endswith("Fixture preview")
        rpc("browser_follow_link", label="Details", url=f"{origin}/details")
        assert eventually(lambda: snapshot("Details"))["text"] == "Fixture details"
        rpc("browser_open", url=origin)
        assert eventually(lambda: snapshot("Preview"))["title"] == "Preview"
        if initial["active"]:
            assert rpc("browser_select_tab", id=initial["active"])["active"] == initial["active"]
        image = rpc("desktop_frame")
        assert (image["width"], image["height"]) == (1280, 800)
        assert base64.b64decode(image["jpeg"], validate=True).startswith(b"\xff\xd8")
        assert rpc("desktop_input", kind="move", x=640, y=480)
        assert rpc("desktop_input", kind="key", key="ctrl+l")
        assert rpc("desktop_release")
        renderer = eventually(lambda: next(
            (entry for entry in Path("/proc").iterdir() if entry.name.isdigit()
             and (entry / "cmdline").exists()
             and b"--type=renderer" in (entry / "cmdline").read_bytes()), None))
        assert "Seccomp:\t2" in (renderer / "status").read_text()
        print("Helium tab tools, local preview, full JPEG, input and renderer seccomp passed")


if __name__ == "__main__":
    main()
