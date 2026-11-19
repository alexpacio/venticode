# Managed Venticode computers

Venticode can run bots in Kitesurf or on an Ubuntu 26.04 desktop with Helium.
Workspaces that use an existing native harness can continue to select a project.
Set a bot's runtime when connecting a provider account or in its bot settings.
Computer mode resolves the machine from the member whose linked account
triggered the bot. That member can open **Remote desktop** from the sidebar or
the bot conversation.

The image uses the static systemd 262 multicall manager and executor. The
manager runs as PID 1 and starts the desktop and enrolled harness. The harness
launches approved work in transient units as the unprivileged `venticode`
account. A service started by `project_service_start` can be opened by
`browser_open_preview` on its loopback port during that run. The service stops
when the run closes, with a ten-minute unit deadline as a fallback.

## Build

Build the computer image from this repository with
`docker build -f computer/Dockerfile -t venticode-computer:local .`.
The source archive, base image, and Helium packages are pinned. Helium runs
inside the visible Xfce session with its built-in ad blocker. The desktop
bridge controls its DevTools pipe through a private local socket.

The workload chart takes immutable registry digests. Use the Venticode
service and connector images from the same release as the chart. Obtain the
Kitesurf image from the Ventic distribution's `deploy/kitesurf/Dockerfile`
or its matching published artifact.

## Qualify a worker pool

- Use Kubernetes 1.36 or newer with cgroup v2, Linux user namespaces and a
  CNI that enforces NetworkPolicy. Provision dedicated, tainted worker nodes.
  Use an encrypted CSI storage class that supports idmapped mounts,
  `ReadWriteOncePod`, and reclaim policy `Delete`. The chart requests 10 GiB
  per member. Volume snapshots and backups follow the storage provider's
  deletion and retention policies.
- Merge `deploy/node/containerd.toml` into the worker containerd configuration
  and install `deploy/node/runtimeclass.yaml`. The handler needs a recent runc
  and containerd with writable cgroup support. Set the worker's default
  containerd runtime and kubelet to the systemd cgroup driver as well.
  The delegated cgroup directory must be owned by mapped UID 0 inside the
  pod. A writable mount with host-root ownership cannot boot the manager.
  The node's cgroup v2 mount must support namespace delegation. Kitesurf uses the distribution's `gvisor`
  RuntimeClass configured for Systrap.
- Generate the Localhost seccomp profile with
  `python3 computer/seccomp.py <default-oci-profile.json>` from the worker's OCI
  allowlist. Install the result in the kubelet seccomp directory as
  `venticode/desktop.json`. Qualify both the runtime profile and the node's LSM
  policy with Helium's user namespace sandbox enabled.
- Set exact browser origins and their approved IPv4 destinations in the chart.
  Both workloads deny ingress and egress by default. Computers additionally
  need the Venticode service relay. The Venticode server needs egress to the
  Kubernetes API and, when enabled, to Kitesurf's control port in the worker
  namespace. Keep cluster services, metadata endpoints and unrelated networks
  outside the approved destination list.

## Configure Venticode

Use the chart under `deploy/helm/venticode-computers`. Set `workerNamespace`,
`computer.image`, `computer.storageClass`, and the matching `network` values.
Set `kitesurf.image` to enable Kitesurf-only bots. Each mode is enabled by its
image value.

For **standalone**, set `standalone.enabled`, `standalone.image`,
`standalone.connectorImage`, and `standalone.host`. Supply the standalone
environment in the existing Secret named by `standalone.secretName`.
The main README lists the PostgreSQL, S3, and inference configuration. Set
`VENTICODE_LLM_VISION=true` in that Secret for an image-capable model. Use
`standalone.tlsSecretName` for the ingress template or route the service
through an existing HTTPS ingress that supports WebSockets and unbuffered SSE.

For **Ventic**, install the companion chart with `standalone.enabled=false`,
`server.namespace=<installation>-app`, and `server.serviceAccount=runtime`.
Set matching values in the distribution's `venticode.computers` block.
The Venticode Deployment passes in the image digests and projected API
identity. Existing Kitesurf pilot routes can use their own worker namespace.

## Use the model CLI

In Computer mode, the bot can use the `ask_model` tool when the harness
advertises it. Shell commands can also request model access when submitted for
approval. In an approved execution, pipe a single prompt using
`printf '%s\n' 'Explain this compiler warning.' | venticode-cli ask --json`.
The CLI returns one text completion and reports the model and available usage.
Inference goes through the parent run's Venticode provider and account.

## Smoke test

In a disposable cluster, start the desktop as an enrolled member. Confirm
the pod uses its assigned RuntimeClass, the workload has a distinct UID map,
the cgroup root is writable only where delegated, and PID 1 is the static
image binary. Start a transient project service in a scripted bot run and
open it in Helium. Verify the tab appears in the remote desktop, then take
and release control. Verify both model CLI entry points with a deterministic
provider. Delete the test pod and confirm the same harness identity and
workspace return. Repeat in standalone and distribution mode, and verify that
a Kitesurf-only bot creates only a disposable browser pod.
