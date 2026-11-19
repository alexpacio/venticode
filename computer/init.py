"""Pass container configuration to systemd services, then become PID 1."""
import json
import os

keys = ("VENTICODE_MANAGED_ID", "VENTICODE_MANAGED_ORIGIN", "VENTICODE_BROWSER_ORIGINS")
old_mask = os.umask(0o077)
with open("/run/venticode.env", "w", encoding="utf-8") as environment:
    for key in keys:
        if key in os.environ:
            environment.write(f"{key}={json.dumps(os.environ[key], ensure_ascii=False)}\n")
os.umask(old_mask)
os.execv("/usr/lib/systemd/systemd", ["/usr/lib/systemd/systemd"])
