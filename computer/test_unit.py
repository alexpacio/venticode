"""Unit tests for computer/seccomp.py, computer/desktop.py, and computer/init.py."""
import asyncio
import json
import os
import stat
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from unittest import mock

COMPUTER_DIR = Path(__file__).resolve().parent
if str(COMPUTER_DIR) not in sys.path:
    sys.path.insert(0, str(COMPUTER_DIR))

import desktop
import init as computer_init
import seccomp


class SeccompTests(unittest.TestCase):
    def _base_profile(self, syscalls):
        return {
            "defaultAction": "SCMP_ACT_ERRNO",
            "defaultErrnoRet": 1,
            "architectures": ["SCMP_ARCH_X86_64", "SCMP_ARCH_X86", "SCMP_ARCH_X32"],
            "archMap": [
                {
                    "architecture": "SCMP_ARCH_X86_64",
                    "subArchitectures": ["SCMP_ARCH_X86", "SCMP_ARCH_X32"],
                },
                {
                    "architecture": "SCMP_ARCH_AARCH64",
                    "subArchitectures": ["SCMP_ARCH_ARM"],
                },
            ],
            "syscalls": syscalls,
        }

    def test_case_insensitive_arch_matching(self):
        base = self._base_profile([
            {"names": ["amd64_lower"], "action": "SCMP_ACT_ALLOW", "includes": {"arches": ["x86_64"]}},
            {"names": ["amd64_scmp"], "action": "SCMP_ACT_ALLOW", "includes": {"arches": ["SCMP_ARCH_X86_64"]}},
            {"names": ["amd64_alias"], "action": "SCMP_ACT_ALLOW", "includes": {"arches": ["AMD64"]}},
            {"names": ["arm64_lower"], "action": "SCMP_ACT_ALLOW", "includes": {"arches": ["aarch64"]}},
            {"names": ["arm64_scmp"], "action": "SCMP_ACT_ALLOW", "includes": {"arches": ["scmp_arch_aarch64"]}},
            {"names": ["x86_only"], "action": "SCMP_ACT_ALLOW", "includes": {"arches": ["x86"]}},
            {"names": ["not_on_amd64"], "action": "SCMP_ACT_ALLOW", "excludes": {"arches": ["x86_64"]}},
            {"names": ["not_on_arm64"], "action": "SCMP_ACT_ALLOW", "excludes": {"arches": ["AARCH64"]}},
        ])
        amd64 = seccomp.desktop_profile(base, "amd64", kernel="6.12.0")
        amd64_names = {name for rule in amd64["syscalls"] for name in rule["names"]}
        self.assertIn("amd64_lower", amd64_names)
        self.assertIn("amd64_scmp", amd64_names)
        self.assertIn("amd64_alias", amd64_names)
        self.assertIn("not_on_arm64", amd64_names)
        self.assertNotIn("arm64_lower", amd64_names)
        self.assertNotIn("arm64_scmp", amd64_names)
        self.assertNotIn("x86_only", amd64_names)
        self.assertNotIn("not_on_amd64", amd64_names)

        arm64 = seccomp.desktop_profile(base, "AARCH64", kernel="6.12.0")
        arm64_names = {name for rule in arm64["syscalls"] for name in rule["names"]}
        self.assertIn("arm64_lower", arm64_names)
        self.assertIn("arm64_scmp", arm64_names)
        self.assertIn("not_on_amd64", arm64_names)
        self.assertNotIn("amd64_lower", arm64_names)
        self.assertNotIn("not_on_arm64", arm64_names)

    def test_native_architecture_only(self):
        base = self._base_profile([{"names": ["read"], "action": "SCMP_ACT_ALLOW"}])
        self.assertEqual(
            seccomp.desktop_profile(base, "amd64", kernel="6.12.0")["architectures"],
            ["SCMP_ARCH_X86_64"],
        )
        self.assertEqual(
            seccomp.desktop_profile(base, "arm64", kernel="6.12.0")["architectures"],
            ["SCMP_ARCH_AARCH64"],
        )

    def test_kernel_tuple_two_part_vs_three_part(self):
        self.assertEqual(seccomp._kernel_tuple("6.12"), (6, 12, 0))
        self.assertEqual(seccomp._kernel_tuple("6.12.0"), (6, 12, 0))
        self.assertEqual(seccomp._kernel_tuple("6.12-rc1"), (6, 12, 0))
        self.assertEqual(seccomp._kernel_tuple("6.12.4-generic"), (6, 12, 4))

        base = self._base_profile([
            {"names": ["needs_6_12_0"], "action": "SCMP_ACT_ALLOW", "includes": {"minKernel": "6.12.0"}},
            {"names": ["excluded_6_12_0"], "action": "SCMP_ACT_ALLOW", "excludes": {"minKernel": "6.12.0"}},
            {"names": ["needs_6_13"], "action": "SCMP_ACT_ALLOW", "includes": {"minKernel": "6.13"}},
        ])
        out = seccomp.desktop_profile(base, "amd64", kernel="6.12")
        names = {name for rule in out["syscalls"] for name in rule["names"]}
        self.assertIn("needs_6_12_0", names)
        self.assertNotIn("excluded_6_12_0", names)
        self.assertNotIn("needs_6_13", names)

    def test_clone3_stripped_and_enosys_emitted(self):
        base = self._base_profile([
            {"names": ["read", "clone3", "ptrace"], "action": "SCMP_ACT_ALLOW"},
        ])
        out = seccomp.desktop_profile(base, "amd64", kernel="6.12.0")
        allow_names = {
            name for rule in out["syscalls"]
            if rule["action"] == "SCMP_ACT_ALLOW"
            for name in rule["names"]
        }
        self.assertIn("read", allow_names)
        self.assertNotIn("clone3", allow_names)
        self.assertNotIn("ptrace", allow_names)
        self.assertIn(
            {"names": ["clone3"], "action": "SCMP_ACT_ERRNO", "errnoRet": 38},
            out["syscalls"],
        )

    def test_clone_and_unshare_split_with_clone_newtime(self):
        base = self._base_profile([{"names": ["read"], "action": "SCMP_ACT_ALLOW"}])
        out = seccomp.desktop_profile(base, "amd64", kernel="6.12.0")
        clone_rules = [r for r in out["syscalls"] if "clone" in r["names"]]
        unshare_rules = [r for r in out["syscalls"] if "unshare" in r["names"]]
        self.assertEqual(len(clone_rules), 1)
        self.assertEqual(len(unshare_rules), 1)
        self.assertEqual(clone_rules[0]["names"], ["clone"])
        self.assertEqual(unshare_rules[0]["names"], ["unshare"])
        clone_mask = clone_rules[0]["args"][0]["value"]
        unshare_mask = unshare_rules[0]["args"][0]["value"]
        self.assertEqual(clone_mask, seccomp.FORBIDDEN_NAMESPACE_FLAGS)
        self.assertEqual(clone_mask & 0x00000080, 0)
        self.assertEqual(unshare_mask, seccomp.FORBIDDEN_NAMESPACE_FLAGS | 0x00000080)


class _DummyWrite:
    def __init__(self):
        self.writes = []

    def write(self, data):
        self.writes.append(data)
        return len(data)


class DesktopTests(unittest.IsolatedAsyncioTestCase):
    async def test_cdp_limit_overrun_preserves_next_frame_alignment(self):
        browser = desktop.Browser()
        browser.write = _DummyWrite()
        browser.reader = asyncio.StreamReader(limit=64)

        oversized = json.dumps({"id": 1, "result": {"blob": "x" * 512}}).encode() + b"\0"
        follow_up = json.dumps({"id": 2, "result": {"aligned": True}}).encode() + b"\0"
        browser.reader.feed_data(oversized + follow_up)
        browser.reader.feed_eof()

        with self.assertRaises(desktop.RequestError):
            await browser.call("Test.oversized")

        result = await browser.call("Test.next")
        self.assertEqual(result, {"aligned": True})

    async def test_cdp_write_timeout(self):
        release = threading.Event()

        class BlockingWrite:
            def write(self, _data):
                release.wait(timeout=2)
                return 0

        browser = desktop.Browser()
        browser.write = BlockingWrite()
        browser.reader = asyncio.StreamReader(limit=64)
        try:
            with self.assertRaises(TimeoutError):
                await browser.call("Test.stalled", timeout_sec=0.05)
        finally:
            release.set()

    def test_allowed_origin_normalization(self):
        normalized = desktop.load_origins([
            "HTTPS://Example.COM:443",
            "http://APP.Example.Test:80/",
            "https://Api.Example.Test:8443",
            "not-a-url",
            "javascript:alert(1)",
        ])
        self.assertEqual(
            normalized,
            {"https://example.com", "http://app.example.test", "https://api.example.test:8443"},
        )
        with mock.patch.object(desktop, "ORIGINS", normalized):
            self.assertTrue(desktop.allowed("https://example.com/page"))
            self.assertTrue(desktop.allowed("HTTPS://EXAMPLE.COM:443/page"))
            self.assertTrue(desktop.allowed("http://APP.EXAMPLE.TEST:80/foo"))
            self.assertTrue(desktop.allowed("http://app.example.test/foo"))
            self.assertTrue(desktop.allowed("https://API.EXAMPLE.TEST:8443/v1"))
            self.assertTrue(desktop.allowed("http://127.0.0.1:8080/preview"))
            self.assertTrue(desktop.allowed("HTTP://LOCALHOST:3000/"))
            self.assertFalse(desktop.allowed("http://example.com/"))
            self.assertFalse(desktop.allowed("https://example.com:8443/"))
            self.assertFalse(desktop.allowed("http://127.0.0.1:80/"))
            self.assertFalse(desktop.allowed("https://user@example.com/"))
            self.assertFalse(desktop.allowed("https://example.com/\nfoo"))

    async def test_tabs_frozen_tab_timeout_resilience_and_session_eviction(self):
        browser = desktop.Browser()
        browser.sessions = {"t1": "s1", "t2": "s2"}
        timeouts_seen = []

        async def fake_call(method, params=None, session=None, timeout_sec=6):
            if method == "Target.getTargets":
                return {
                    "targetInfos": [
                        {"targetId": "t1", "type": "page", "title": "Frozen", "url": "https://a.example"},
                        {"targetId": "t2", "type": "page", "title": "Live", "url": "https://b.example"},
                    ]
                }
            if method == "Runtime.evaluate":
                timeouts_seen.append(timeout_sec)
                if session == "s1":
                    raise TimeoutError("tab frozen")
                if session == "s2":
                    return {"result": {"value": "visible"}}
            raise AssertionError(f"unexpected call: {method}")

        browser.call = fake_call
        result = await browser.tabs()
        self.assertEqual(result["active"], "t2")
        self.assertEqual(len(result["tabs"]), 2)
        self.assertNotIn("t1", browser.sessions)
        self.assertEqual(browser.sessions["t2"], "s2")
        self.assertTrue(all(t <= 1.5 for t in timeouts_seen))

    async def test_cdp_session_error_evicts_session(self):
        browser = desktop.Browser()
        browser.write = _DummyWrite()
        browser.reader = asyncio.StreamReader(limit=1024)
        browser.sessions = {"t1": "s1"}
        browser.reader.feed_data(
            json.dumps({"id": 1, "error": {"code": -32000, "message": "Session closed"}}).encode() + b"\0"
        )
        browser.reader.feed_eof()

        with self.assertRaises(desktop.RequestError):
            await browser.call("Page.navigate", {"url": "about:blank"}, session="s1")
        self.assertNotIn("t1", browser.sessions)

    async def test_snapshot_checks_authoritative_cdp_url_before_and_after(self):
        self.assertNotIn("JSON.stringify", desktop.SNAPSHOT)

        with mock.patch.object(desktop, "ORIGINS", {"https://allowed.example"}):
            # Disallowed URL before snapshot: Runtime.evaluate must not run.
            browser = desktop.Browser()
            browser.sessions = {"t1": "s1"}
            eval_called = False

            async def call_disallowed(method, params=None, session=None, timeout_sec=6):
                nonlocal eval_called
                if method == "Target.getTargets":
                    return {"targetInfos": [{"targetId": "t1", "type": "page", "title": "Bad", "url": "https://evil.example"}]}
                if method == "Runtime.evaluate" and params["expression"] == "document.visibilityState":
                    return {"result": {"value": "visible"}}
                if method == "Target.getTargetInfo":
                    return {"targetInfo": {"targetId": "t1", "url": "https://evil.example/secret"}}
                if method == "Runtime.evaluate":
                    eval_called = True
                    return {"result": {"value": {"url": "https://allowed.example", "title": "Spoof", "text": "x", "links": []}}}
                raise AssertionError(f"unexpected call: {method}")

            browser.call = call_disallowed
            with self.assertRaises(desktop.RequestError):
                await browser.snapshot()
            self.assertFalse(eval_called)

            # TOCTOU navigation to disallowed origin during snapshot must be rejected.
            browser = desktop.Browser()
            browser.sessions = {"t1": "s1"}
            target_info_urls = iter(["https://allowed.example/start", "https://evil.example/redirected"])

            async def call_toctou(method, params=None, session=None, timeout_sec=6):
                if method == "Target.getTargets":
                    return {"targetInfos": [{"targetId": "t1", "type": "page", "title": "Ok", "url": "https://allowed.example/start"}]}
                if method == "Runtime.evaluate" and params["expression"] == "document.visibilityState":
                    return {"result": {"value": "visible"}}
                if method == "Target.getTargetInfo":
                    return {"targetInfo": {"targetId": "t1", "url": next(target_info_urls)}}
                if method == "Runtime.evaluate":
                    return {"result": {"value": {"title": "Ok", "text": "hi", "links": []}}}
                raise AssertionError(f"unexpected call: {method}")

            browser.call = call_toctou
            with self.assertRaises(desktop.RequestError):
                await browser.snapshot()

            # Allowed URL returns authoritative CDP URL even if page value attempts to spoof it.
            browser = desktop.Browser()
            browser.sessions = {"t1": "s1"}

            async def call_allowed(method, params=None, session=None, timeout_sec=6):
                if method == "Target.getTargets":
                    return {"targetInfos": [{"targetId": "t1", "type": "page", "title": "Ok", "url": "https://allowed.example/p"}]}
                if method == "Runtime.evaluate" and params["expression"] == "document.visibilityState":
                    return {"result": {"value": "visible"}}
                if method == "Target.getTargetInfo":
                    return {"targetInfo": {"targetId": "t1", "url": "https://allowed.example/p"}}
                if method == "Runtime.evaluate":
                    self.assertTrue(params.get("returnByValue"))
                    return {
                        "result": {
                            "value": {
                                "url": "https://spoofed.example/",
                                "title": "Allowed Page",
                                "text": "body text",
                                "links": [{"label": "Go", "url": "https://allowed.example/next"}],
                            }
                        }
                    }
                raise AssertionError(f"unexpected call: {method}")

            browser.call = call_allowed
            page = await browser.snapshot()
            self.assertEqual(page["url"], "https://allowed.example/p")
            self.assertEqual(page["title"], "Allowed Page")
            self.assertEqual(page["text"], "body text")
            self.assertEqual(page["links"], [{"label": "Go", "url": "https://allowed.example/next"}])

    async def test_non_finite_and_overflow_inputs_rejected(self):
        d = desktop.Desktop()
        bad_payloads = [
            {"kind": "move", "x": float("nan"), "y": 10},
            {"kind": "move", "x": float("inf"), "y": 10},
            {"kind": "move", "x": 10, "y": float("-inf")},
            {"kind": "move", "x": 10**400, "y": 10},
            {"kind": "move", "x": True, "y": 10},
            {"kind": "scroll", "x": 10, "y": 10, "delta": float("nan")},
            {"kind": "scroll", "x": 10, "y": 10, "delta": float("inf")},
            {"kind": "scroll", "x": 10, "y": 10, "delta": float("-inf")},
            {"kind": "scroll", "x": 10, "y": 10, "delta": 10**400},
        ]
        for payload in bad_payloads:
            with self.subTest(payload=payload):
                with self.assertRaises(desktop.RequestError):
                    await d.input(payload)

    async def test_pointer_tracking_and_release_resilience(self):
        d = desktop.Desktop()

        async def fail_cmd(*_args, **_kwargs):
            raise desktop.RequestError("xdotool failed")

        with mock.patch.object(desktop, "command", side_effect=fail_cmd):
            with self.assertRaises(desktop.RequestError):
                await d.input({"kind": "pointer", "x": 10, "y": 10, "button": 1, "down": True})
        self.assertEqual(d.buttons, set())

        async def ok_cmd(*_args, **_kwargs):
            return b""

        with mock.patch.object(desktop, "command", side_effect=ok_cmd):
            await d.input({"kind": "pointer", "x": 10, "y": 10, "button": 1, "down": True})
            await d.input({"kind": "pointer", "x": 10, "y": 10, "button": 2, "down": True})
        self.assertEqual(d.buttons, {1, 2})

        attempted = []

        async def partial_fail(*args, **_kwargs):
            btn = int(args[-1])
            attempted.append(btn)
            if btn == 1:
                raise desktop.RequestError("mouseup 1 failed")
            return b""

        with mock.patch.object(desktop, "command", side_effect=partial_fail):
            with self.assertRaises(desktop.RequestError):
                await d.release()
        self.assertEqual(set(attempted), {1, 2})
        self.assertEqual(d.buttons, {1})

    async def test_desktop_frame_serialized_with_lock(self):
        d = desktop.Desktop()
        active = 0
        max_active = 0

        async def fake_ffmpeg(*_args, **_kwargs):
            nonlocal active, max_active
            active += 1
            max_active = max(max_active, active)
            await asyncio.sleep(0.03)
            active -= 1
            return b"\xff\xd8jpeg\xff\xd9"

        class FakeWriter:
            def __init__(self):
                self.buf = bytearray()

            def write(self, data):
                self.buf.extend(data)

            async def drain(self):
                return None

            def close(self):
                return None

            async def wait_closed(self):
                return None

        def make_reader():
            r = asyncio.StreamReader()
            r.feed_data(json.dumps({"operation": "desktop_frame", "arguments": {}}).encode() + b"\n")
            r.feed_eof()
            return r

        w1, w2 = FakeWriter(), FakeWriter()
        with mock.patch.object(desktop, "command", side_effect=fake_ffmpeg):
            await asyncio.gather(d.handle(make_reader(), w1), d.handle(make_reader(), w2))
        self.assertEqual(max_active, 1)
        self.assertIn("value", json.loads(w1.buf.decode()))
        self.assertIn("value", json.loads(w2.buf.decode()))

    async def test_command_captures_bounded_stderr_and_logs(self):
        with self.assertLogs(desktop.LOG, level="WARNING") as logs:
            with self.assertRaises(desktop.RequestError):
                await desktop.command(
                    sys.executable, "-c", "import sys; sys.stderr.write('E' * 10000); sys.exit(2)"
                )
        self.assertEqual(len(logs.output), 1)
        self.assertEqual(logs.output[0].count("E"), desktop.STDERR_LIMIT)

    async def test_handle_writer_drain_handles_broken_pipe(self):
        d = desktop.Desktop()

        class BrokenWriter:
            def write(self, _data):
                return None

            async def drain(self):
                raise BrokenPipeError("client gone")

            def close(self):
                return None

            async def wait_closed(self):
                raise BrokenPipeError("client gone")

        reader = asyncio.StreamReader()
        reader.feed_data(json.dumps({"operation": "desktop_release", "arguments": {}}).encode() + b"\n")
        reader.feed_eof()

        with self.assertLogs(desktop.LOG, level="WARNING"):
            await d.handle(reader, BrokenWriter())


class InitTests(unittest.TestCase):
    def test_env_split_and_0600_permissions(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            harness_path = Path(tmpdir) / "venticode.env"
            desktop_path = Path(tmpdir) / "venticode-desktop.env"
            # Pre-create with overly broad mode to verify os.fchmod tightens it to 0o600.
            harness_path.write_text("stale=1\n", encoding="utf-8")
            os.chmod(harness_path, 0o644)

            env = {
                "VENTICODE_MANAGED_ID": "managed-123",
                "VENTICODE_MANAGED_ORIGIN": "https://portal.example",
                "VENTICODE_BROWSER_ORIGINS": '["https://docs.example"]',
                "EXTRA_SECRET": "must-not-appear",
            }
            computer_init.write_env_files(run_dir=tmpdir, environ=env)

            self.assertEqual(stat.S_IMODE(harness_path.stat().st_mode), 0o600)
            self.assertEqual(stat.S_IMODE(desktop_path.stat().st_mode), 0o600)

            harness_text = harness_path.read_text(encoding="utf-8")
            desktop_text = desktop_path.read_text(encoding="utf-8")

            self.assertIn("VENTICODE_MANAGED_ID=", harness_text)
            self.assertIn("VENTICODE_MANAGED_ORIGIN=", harness_text)
            self.assertIn("VENTICODE_BROWSER_ORIGINS=", harness_text)
            self.assertNotIn("EXTRA_SECRET", harness_text)

            self.assertIn("VENTICODE_BROWSER_ORIGINS=", desktop_text)
            self.assertNotIn("VENTICODE_MANAGED_ID", desktop_text)
            self.assertNotIn("VENTICODE_MANAGED_ORIGIN", desktop_text)
            self.assertNotIn("EXTRA_SECRET", desktop_text)

            # When VENTICODE_BROWSER_ORIGINS is absent, desktop env file stays 0o600 and empty.
            computer_init.write_env_files(
                run_dir=tmpdir,
                environ={"VENTICODE_MANAGED_ID": "managed-123"},
            )
            self.assertEqual(stat.S_IMODE(desktop_path.stat().st_mode), 0o600)
            self.assertEqual(desktop_path.read_text(encoding="utf-8"), "")

    def test_desktop_unit_uses_desktop_env_file(self):
        unit = (COMPUTER_DIR / "units" / "venticode-desktop.service").read_text(encoding="utf-8")
        self.assertIn("EnvironmentFile=-/run/venticode-desktop.env", unit)
        self.assertNotIn("EnvironmentFile=/run/venticode.env", unit)


if __name__ == "__main__":
    unittest.main()
