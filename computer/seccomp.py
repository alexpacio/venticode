"""Generate an OCI Localhost profile for the Venticode computer container."""
import argparse
import json
import platform
import sys

# Allow the namespace operations used by Helium's user namespace sandbox.
# Mount, cgroup, IPC and UTS namespace creation follow the base policy.
FORBIDDEN_NAMESPACE_FLAGS = 0x00020000 | 0x02000000 | 0x04000000 | 0x08000000
CLONE_NEWTIME = 0x00000080
CAPABILITIES = {"CAP_" + name for name in (
    "CHOWN", "DAC_OVERRIDE", "FOWNER", "KILL", "SETUID", "SETGID", "SETPCAP",
)}
ARCHITECTURES = {
    "amd64": ("SCMP_ARCH_X86_64", frozenset({"amd64", "x86_64"})),
    "arm64": ("SCMP_ARCH_AARCH64", frozenset({"arm64", "aarch64"})),
}
ARCH_ALIASES = {alias: key for key, (_, aliases) in ARCHITECTURES.items() for alias in aliases}
CROSS_PROCESS = {"ptrace", "process_vm_readv", "process_vm_writev", "pidfd_getfd", "clone3"}
ENOSYS = 38


def _normalize_arch(value):
    return str(value).strip().lower().removeprefix("scmp_arch_")


def _kernel_tuple(raw):
    parts = []
    for part in str(raw).split("-")[0].split("."):
        if part.isdigit():
            parts.append(int(part))
        else:
            break
    while len(parts) < 3:
        parts.append(0)
    return tuple(parts[:3])


def desktop_profile(profile, architecture, kernel=None):
    if profile.get("defaultAction") != "SCMP_ACT_ERRNO" or not isinstance(profile.get("syscalls"), list):
        raise ValueError("an OCI default-deny allowlist is required")
    arch_key = ARCH_ALIASES.get(_normalize_arch(architecture))
    if arch_key is None:
        raise ValueError(f"unsupported architecture: {architecture}")
    native, valid_arches = ARCHITECTURES[arch_key]
    target_kernel = _kernel_tuple(kernel if kernel is not None else platform.release())
    result = {key: profile[key] for key in ("defaultAction", "defaultErrnoRet") if key in profile}
    result["architectures"] = [native]
    syscalls = []
    for rule in profile["syscalls"]:
        includes, excludes = rule.get("includes", {}), rule.get("excludes", {})
        if set(includes) - {"caps", "arches", "minKernel"} or set(excludes) - {"caps", "arches", "minKernel"}:
            raise ValueError("unsupported architecture or capability filter in base profile")
        if includes.get("minKernel") and target_kernel < _kernel_tuple(includes["minKernel"]):
            continue
        if excludes.get("minKernel") and target_kernel >= _kernel_tuple(excludes["minKernel"]):
            continue
        if includes.get("arches") and not any(_normalize_arch(a) in valid_arches for a in includes["arches"]):
            continue
        if excludes.get("arches") and any(_normalize_arch(a) in valid_arches for a in excludes["arches"]):
            continue
        if not CAPABILITIES.issuperset(includes.get("caps", [])) or CAPABILITIES.intersection(excludes.get("caps", [])):
            continue
        names = [name for name in rule["names"] if name not in CROSS_PROCESS and name != "chroot"]
        if names:
            syscalls.append({**{key: rule[key] for key in ("action", "errnoRet", "args") if key in rule}, "names": names})
    # On clone(2), bits 0..7 are CSIGNAL; only mask forbidden namespace flags.
    syscalls.append({
        "names": ["clone"], "action": "SCMP_ACT_ALLOW",
        "args": [{"index": 0, "value": FORBIDDEN_NAMESPACE_FLAGS,
                  "valueTwo": 0, "op": "SCMP_CMP_MASKED_EQ"}],
    })
    syscalls.append({
        "names": ["unshare"], "action": "SCMP_ACT_ALLOW",
        "args": [{"index": 0, "value": FORBIDDEN_NAMESPACE_FLAGS | CLONE_NEWTIME,
                  "valueTwo": 0, "op": "SCMP_CMP_MASKED_EQ"}],
    })
    # Return ENOSYS so glibc pthread_create falls back to clone(2) instead of failing with EPERM.
    syscalls.append({"names": ["clone3"], "action": "SCMP_ACT_ERRNO", "errnoRet": ENOSYS})
    # The sandbox calls chroot inside its own user namespace. The pod's
    # capability set does not grant CAP_SYS_CHROOT to its initial processes.
    syscalls.append({"names": ["chroot"], "action": "SCMP_ACT_ALLOW"})
    return {**result, "syscalls": syscalls}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base_profile")
    parser.add_argument(
        "--architecture",
        choices=ARCHITECTURES,
        default="arm64" if platform.machine().lower() in ("aarch64", "arm64") else "amd64",
    )
    parser.add_argument("--kernel", default=None, help="Target kernel version for minKernel filters (defaults to host kernel)")
    args = parser.parse_args()
    with open(args.base_profile, encoding="utf-8") as source:
        json.dump(desktop_profile(json.load(source), args.architecture, args.kernel), sys.stdout, indent=2)
    print()
