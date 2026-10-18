# SnapForge Compatibility

This document records the capabilities of the environment SnapForge was
developed and tested on, and states precisely what is and is not supported.

## Real checkpoint/restore prerequisites

| Requirement | Why | Checked by |
|---|---|---|
| `CONFIG_CHECKPOINT_RESTORE=y` | CRIU core kernel support | `snapforge doctor` |
| `/proc/sys/kernel/ns_last_pid` present | CRIU restores PIDs | `snapforge doctor` |
| `criu` binary on `PATH` (>= 3.15) | Capture/restore engine | `snapforge doctor` |
| root or `CAP_SYS_PTRACE` + namespace caps | ptrace/namespace access | `snapforge doctor` |
| Compatible kernel on restore | CRIU artifact compatibility | documented below |

## Development host snapshot

Captured from `snapforge doctor` on the development machine:

```
SnapForge host report
=====================
OS:      Arch Linux
Kernel:  7.1.9-hardened1-1-hardened
Arch:    x86_64
UID:     1000 (root: false)
CRIU:    not found (readiness: MissingBinary)

  [FAIL] kernel.CHECKPOINT_RESTORE    kernel built without CONFIG_CHECKPOINT_RESTORE; CRIU cannot capture a process
  [FAIL] kernel.ns_last_pid           /proc/sys/kernel/ns_last_pid missing; CRIU needs it to restore PIDs
  [FAIL] criu.binary                  criu not found on PATH
  [OK  ] kernel.unprivileged_userns_clone restricted (typical for CRIU on hardened hosts)
  [FAIL] privileges                   not root and no CAP_SYS_PTRACE; CRIU needs ptrace/namespace privileges
```

| Resource | Value |
|---|---|
| CPU | Intel(R) Core(TM) 5 120U, 12 logical CPUs |
| Memory | 15 GiB total |
| Root filesystem | ext4 on NVMe (`/dev/nvme0n1p3`), nearly full |
| `/tmp` | tmpfs, ~7 GiB available |
| Rust | 1.93.1 |
| Python | 3.12.7 |
| Docker | 29.7.2 (shares the host kernel) |

## Verdict on this host

**Real CRIU checkpoint/restore is NOT available here.** Two independent,
confirmed blockers:

1. The kernel is built **without `CONFIG_CHECKPOINT_RESTORE`**
   (`# CONFIG_CHECKPOINT_RESTORE is not set` in `/proc/config.gz`), and the
   `ns_last_pid` sysctl is absent.
2. The interactive user is unprivileged (UID 1000, empty effective capability
   set, no passwordless sudo, unprivileged user namespaces disabled).

This was verified empirically, not assumed: CRIU 3.17.1 installed inside a
privileged Debian container on this same kernel fails immediately with

```
vdso: Unexpected rt vDSO area bounds
vdso: Failed to fill self vdso symtable
kerndat_vdso_fill_symtable failed when initializing kerndat.
Could not initialize kernel features detection.
```

Because containers share the host kernel, no container configuration can add
the missing kernel feature.

## What still works here

Everything SnapForge owns is host-independent and is built/tested regardless:

- `snapforge doctor` host probing.
- The deterministic workload harness and its state-validation protocol.
- Snapshot format, validation, content-addressed storage, and transfer
  (Phases 3–5).

## Enabling real checkpoint/restore

To run the real CRIU path, use a host (or VM) whose kernel is built with
`CONFIG_CHECKPOINT_RESTORE=y`, with `criu` installed and root or
`CAP_SYS_PTRACE`. Then:

```bash
criu check --all          # confirm kernel feature detection succeeds
snapforge doctor          # should report readiness: ready
snapforge checkpoint ...  # captures a real checkpoint
snapforge restore ...     # restores it
```

The `docs/troubleshooting.md` (later phase) will cover common CRIU failures.

## Unsupported workload characteristics

Even on a CRIU-capable host, the harness deliberately avoids, and documentation
flags, features that are not reliably restorable:

- Arbitrary external network sockets connected to services that may be gone.
- GPU / device contexts without explicit CRIU support and drivers.
- Open files whose backing storage changed between capture and restore.
- Kernel resources whose state cannot be reconstructed (e.g. some eBPF objects).
