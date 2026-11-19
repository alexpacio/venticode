"""Generate an OCI Localhost profile for the Venticode computer container."""
import argparse
import json
import platform
import sys

# Allow the namespace operations used by Helium's user namespace sandbox.
# Mount, cgroup, IPC and UTS namespace creation follow the base policy.
FORBIDDEN_NAMESPACE_FLAGS = 0x00020000 | 0x02000000 | 0x04000000 | 0x08000000
CAPABILITIES = {"CAP_" + name for name in (
    "CHOWN", "DAC_OVERRIDE", "FOWNER", "KILL", "SETUID", "SETGID", "SETPCAP",
)}
ARCHITECTURES = {
    "amd64": ("SCMP_ARCH_X86_64", "amd64"),
    "arm64": ("SCMP_ARCH_AARCH64", "arm64"),
}
CROSS_PROCESS = {"ptrace", "process_vm_readv", "process_vm_writev", "pidfd_getfd"}


def desktop_profile(profile, architecture):
    if profile.get("defaultAction") != "SCMP_ACT_ERRNO" or not isinstance(profile.get("syscalls"), list):
        raise ValueError("an OCI default-deny allowlist is required")
    native, arch = ARCHITECTURES[architecture]
    arch_map = profile.get("archMap", [])
    valid_arches = {arch, native, native.removeprefix("SCMP_ARCH_")}
    result = {key: profile[key] for key in ("defaultAction", "defaultErrnoRet") if key in profile}
    arches = profile.get("architectures") or next(
        ([native, *entry.get("subArchitectures", [])] for entry in arch_map if entry["architecture"] == native),
        [native],
    )
    result["architectures"] = arches
    syscalls = []
    for rule in profile["syscalls"]:
        includes, excludes = rule.get("includes", {}), rule.get("excludes", {})
        if set(includes) - {"caps", "arches"} or set(excludes) - {"caps", "arches"}:
            raise ValueError("unsupported architecture or capability filter in base profile")
        if includes.get("arches") and not valid_arches.intersection(includes["arches"]):
            continue
        if excludes.get("arches") and valid_arches.intersection(excludes["arches"]):
            continue
        if not CAPABILITIES.issuperset(includes.get("caps", [])) or CAPABILITIES.intersection(excludes.get("caps", [])):
            continue
        names = [name for name in rule["names"] if name not in CROSS_PROCESS and name != "chroot"]
        if names:
            syscalls.append({**{key: rule[key] for key in ("action", "errnoRet", "args") if key in rule}, "names": names})
    syscalls.append({
        "names": ["clone", "unshare"], "action": "SCMP_ACT_ALLOW",
        "args": [{"index": 0, "value": FORBIDDEN_NAMESPACE_FLAGS,
                  "valueTwo": 0, "op": "SCMP_CMP_MASKED_EQ"}],
    })
    # The sandbox calls chroot inside its own user namespace. The pod's
    # capability set does not grant CAP_SYS_CHROOT to its initial processes.
    syscalls.append({"names": ["chroot"], "action": "SCMP_ACT_ALLOW"})
    return {**result, "syscalls": syscalls}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base_profile")
    parser.add_argument("--architecture", choices=ARCHITECTURES, default="arm64" if platform.machine() == "aarch64" else "amd64")
    args = parser.parse_args()
    with open(args.base_profile, encoding="utf-8") as source:
        json.dump(desktop_profile(json.load(source), args.architecture), sys.stdout, indent=2)
    print()
