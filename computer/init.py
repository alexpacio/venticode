"""Pass container configuration to systemd services, then become PID 1."""
import json
import os

HARNESS_KEYS = ("VENTICODE_MANAGED_ID", "VENTICODE_MANAGED_ORIGIN", "VENTICODE_BROWSER_ORIGINS")
DESKTOP_KEYS = ("VENTICODE_BROWSER_ORIGINS",)


def write_env_file(path, keys, environ=None):
    source = os.environ if environ is None else environ
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as environment:
            fd = None
            for key in keys:
                if key in source:
                    environment.write(f"{key}={json.dumps(source[key], ensure_ascii=False)}\n")
    finally:
        if fd is not None:
            os.close(fd)


def write_env_files(run_dir="/run", environ=None):
    write_env_file(os.path.join(run_dir, "venticode.env"), HARNESS_KEYS, environ=environ)
    write_env_file(os.path.join(run_dir, "venticode-desktop.env"), DESKTOP_KEYS, environ=environ)


def main():
    write_env_files()
    os.execv("/usr/lib/systemd/systemd", ["/usr/lib/systemd/systemd"])


if __name__ == "__main__":
    main()
