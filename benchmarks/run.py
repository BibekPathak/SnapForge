#!/usr/bin/env python3
"""SnapForge reproducible benchmark harness.

Runs the SnapForge Rust CLI to measure storage, transfer, and incremental
behaviour, plus (where the host supports it) real CRIU checkpoint/restore.

Design goals:
  * Reproducibility: fixed parameters, repeated runs, explicit environment
    metadata recorded with every result.
  * Honesty: never fabricate values. Unsupported experiments are recorded as
    ``skipped`` with a reason (e.g. kernel without CONFIG_CHECKPOINT_RESTORE).
  * Portability: standard library only. Plots are optional (skipped if
    matplotlib is unavailable).

Outputs (under --out, default benchmarks/results):
  * raw.jsonl   : one JSON record per measurement
  * raw.csv     : flattened CSV
  * summary.md  : compact Markdown tables with median + spread
  * plots/*.png : optional line charts per sweep (if matplotlib present)

Usage:
  python3 benchmarks/run.py --repetitions 5 --sizes 64,256
  python3 benchmarks/run.py --only storage --quick
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

SCHEMA_VERSION = 1
REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_OUT = REPO_ROOT / "benchmarks" / "results"
TARGET_DIR = os.environ.get("CARGO_TARGET_DIR", "/tmp/snapforge-target")


class _Blobs:
    """Deterministic pseudo-random data generator with caching."""

    def __init__(self) -> None:
        import random

        self._random = random
        self._cache: dict[tuple[int, int], bytes] = {}

    def bytes_for(self, seed: int, length: int) -> bytes:
        key = (seed, length)
        cached = self._cache.get(key)
        if cached is None:
            cached = self._random.Random(seed).randbytes(length)
            self._cache[key] = cached
        return cached


_RAND = _Blobs()


# --------------------------------------------------------------------------- #
# Environment metadata
# --------------------------------------------------------------------------- #
def _read(path: str) -> str | None:
    try:
        return Path(path).read_text().strip()
    except OSError:
        return None


def _cmd(args: list[str]) -> str | None:
    try:
        out = subprocess.run(
            args, capture_output=True, text=True, timeout=10, check=False
        )
        return (out.stdout or out.stderr).strip() or None
    except (OSError, subprocess.SubprocessError):
        return None


def _kernel_config_flag(name: str) -> bool | None:
    import gzip

    try:
        with gzip.open("/proc/config.gz", "rt") as f:
            text = f.read()
    except OSError:
        return None
    for line in text.splitlines():
        if line.strip() == f"# {name} is not set":
            return False
        if line.startswith(f"{name}="):
            return line.split("=", 1)[1].strip() == "y"
    return None


def environment() -> dict[str, Any]:
    env: dict[str, Any] = {
        "hostname": platform.node(),
        "os": _read("/etc/os-release") and _os_pretty_name() or platform.platform(),
        "kernel": platform.release(),
        "arch": platform.machine(),
        "python": platform.python_version(),
        "cpu_model": None,
        "cpu_count": os.cpu_count(),
        "mem_total_kib": None,
        "filesystem_store": None,
        "governor": None,
        "criu_version": _cmd(["criu", "--version"]) if shutil.which("criu") else None,
        "config_checkpoint_restore": _kernel_config_flag("CONFIG_CHECKPOINT_RESTORE"),
        "ns_last_pid_present": Path("/proc/sys/kernel/ns_last_pid").exists(),
        "privileges": "root" if os.geteuid() == 0 else "unprivileged",
    }
    # CPU model from /proc/cpuinfo.
    cpuinfo = _read("/proc/cpuinfo") or ""
    for line in cpuinfo.splitlines():
        if line.lower().startswith("model name"):
            env["cpu_model"] = line.split(":", 1)[1].strip()
            break
    # Total memory.
    meminfo = _read("/proc/meminfo") or ""
    for line in meminfo.splitlines():
        if line.startswith("MemTotal:"):
            env["mem_total_kib"] = int(line.split()[1])
            break
    env["governor"] = _read(
        "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"
    )
    return env


def _os_pretty_name() -> str:
    text = _read("/etc/os-release") or ""
    for line in text.splitlines():
        if line.startswith("PRETTY_NAME="):
            return line.split("=", 1)[1].strip().strip('"')
    return platform.platform()


# --------------------------------------------------------------------------- #
# Command execution
# --------------------------------------------------------------------------- #
@dataclass
class Record:
    experiment: str
    kind: str
    parameters: dict[str, Any]
    metrics: dict[str, Any]
    repetition: int = 0
    status: str = "ok"
    skip_reason: str | None = None
    error: str | None = None
    label: str | None = None
    run_id: str = field(default_factory=lambda: uuid.uuid4().hex[:12])
    timestamp: str = field(
        default_factory=lambda: time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    )

    def to_json(self, env: dict[str, Any]) -> dict[str, Any]:
        return {
            "schema_version": SCHEMA_VERSION,
            "run_id": self.run_id,
            "timestamp": self.timestamp,
            "experiment": self.experiment,
            "kind": self.kind,
            "label": self.label,
            "repetition": self.repetition,
            "parameters": self.parameters,
            "environment": env,
            "metrics": self.metrics,
            "status": self.status,
            "skip_reason": self.skip_reason,
            "error": self.error,
        }


class Harness:
    def __init__(self, args: argparse.Namespace):
        self.args = args
        self.env = environment()
        self.bin = self._locate_binary()
        self.records: list[Record] = []
        self.workdir = Path(args.workdir or "/tmp/snapforge-bench")
        self.workdir.mkdir(parents=True, exist_ok=True)

    def free_bytes(self) -> int:
        return shutil.disk_usage(str(self.workdir)).free

    def has_space(self, needed_mib: int) -> bool:
        # Require ~2.5x the data size headroom for input + CAS + frame.
        return self.free_bytes() > needed_mib * 1024 * 1024 * 5 // 2

    # -- setup ------------------------------------------------------------- #
    def _locate_binary(self) -> Path:
        candidate = Path(TARGET_DIR) / "release" / "snapforge"
        if candidate.exists():
            return candidate
        raise SystemExit(
            f"snapforge binary not found at {candidate}. "
            "Run `cargo build --release -p snapforge-cli` first."
        )

    def _run(self, args: list[str], timeout: int = 600) -> subprocess.CompletedProcess:
        return subprocess.run(
            [str(self.bin), *args],
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )

    def add(self, record: Record) -> None:
        self.records.append(record)
        tag = f"[{record.status}]"
        print(f"  {tag} {record.experiment} {record.parameters}", flush=True)

    # -- experiments ------------------------------------------------------- #
    def experiment_storage(self) -> None:
        """Storage pipeline: sweep chunk size x concurrency x data size."""
        for size_mib in self.args.sizes:
            for chunk in self.args.chunk_sizes:
                for conc in self.args.concurrencies:
                    for rep in range(self.args.repetitions):
                        params = {
                            "size_mib": size_mib,
                            "chunk_size": chunk,
                            "concurrency": conc,
                        }
                        if not self.has_space(size_mib):
                            self.add(
                                Record(
                                    experiment="storage_pipeline",
                                    kind="storage_only",
                                    parameters=params,
                                    metrics={},
                                    repetition=rep,
                                    status="skipped",
                                    skip_reason=(
                                        "insufficient free space in workdir: "
                                        f"{self.free_bytes() // (1024 * 1024)} MiB free, "
                                        f"need ~{size_mib * 5 // 2} MiB for {size_mib} MiB input"
                                    ),
                                )
                            )
                            continue
                        store = self.workdir / f"storage-{size_mib}-{chunk}-{conc}-{rep}"
                        shutil.rmtree(store, ignore_errors=True)
                        cp = self._run(
                            [
                                "--store",
                                str(store),
                                "--chunk-size",
                                str(chunk),
                                "--concurrency",
                                str(conc),
                                "--json",
                                "bench",
                                "--size-mib",
                                str(size_mib),
                            ]
                        )
                        if cp.returncode != 0:
                            self.add(
                                Record(
                                    experiment="storage_pipeline",
                                    kind="storage_only",
                                    parameters=params,
                                    metrics={},
                                    repetition=rep,
                                    status="failed",
                                    error=cp.stderr.strip()[-500:],
                                )
                            )
                            shutil.rmtree(store, ignore_errors=True)
                            continue
                        data = json.loads(cp.stdout)
                        # Reclaim tmpfs/disk immediately; stores are disposable.
                        shutil.rmtree(store, ignore_errors=True)
                        self.add(
                            Record(
                                experiment="storage_pipeline",
                                kind="storage_only",
                                label=data.get("label"),
                                parameters=params,
                                metrics={
                                    "duration_ms": data["total_time_ns"] / 1e6,
                                    "chunking_ms": data["chunking_time_ns"] / 1e6,
                                    "storage_ms": data["storage_time_ns"] / 1e6,
                                    "throughput_mib_s": data["throughput_mib_s"],
                                    "bytes": data["total_bytes"],
                                    "peak_rss_kib": data.get("peak_rss_kib"),
                                    "dedup_saved_bytes": data["dedup_saved_bytes"],
                                    "chunks": data["chunks"],
                                    "peak_in_flight": data["peak_in_flight"],
                                },
                                repetition=rep,
                            )
                        )

    def experiment_transfer(self) -> None:
        """Snapshot transfer: sequential vs parallel, cold vs warm cache."""
        if not self.has_space(self.args.transfer_mib):
            self.add(
                Record(
                    experiment="transfer",
                    kind="transfer",
                    parameters={"transfer_mib": self.args.transfer_mib},
                    metrics={},
                    status="skipped",
                    skip_reason=(
                        "insufficient free space in workdir: "
                        f"{self.free_bytes() // (1024 * 1024)} MiB free"
                    ),
                )
            )
            return
        src_dir = self.workdir / "transfer-src"
        src_dir.mkdir(parents=True, exist_ok=True)
        # Deterministic source artifacts.
        total = self.args.transfer_mib
        n_files = 4
        per = (total * 1024 * 1024) // n_files
        for i in range(n_files):
            p = src_dir / f"artifact-{i}.img"
            if not p.exists() or p.stat().st_size != per:
                p.write_bytes(_blob(0x1234_5678 + i, per))

        for conc in self.args.transfer_concurrencies:
            for rep in range(self.args.repetitions):
                store_a = self.workdir / f"xfer-a-{conc}"
                store_b = self.workdir / f"xfer-b-{conc}-{rep}"
                shutil.rmtree(store_a, ignore_errors=True)
                shutil.rmtree(store_b, ignore_errors=True)
                # Store baseline.
                cp = self._run(
                    [
                        "--store",
                        str(store_a),
                        "--chunk-size",
                        str(self.args.transfer_chunk),
                        "store",
                        str(src_dir),
                        "--id",
                        "base",
                        "--workload",
                        "bench",
                        "--json",
                    ]
                )
                if cp.returncode != 0:
                    self.add(
                        Record(
                            experiment="transfer",
                            kind="transfer",
                            parameters={"concurrency": conc},
                            metrics={},
                            repetition=rep,
                            status="failed",
                            error=cp.stderr.strip()[-500:],
                        )
                    )
                    continue
                # Transfer and time it.
                t0 = time.perf_counter()
                cp = self._run(
                    [
                        "--store",
                        str(store_a),
                        "transfer",
                        "base",
                        str(store_b),
                        "--transfer-concurrency",
                        str(conc),
                        "--json",
                    ]
                )
                elapsed_ms = (time.perf_counter() - t0) * 1000.0
                if cp.returncode != 0:
                    self.add(
                        Record(
                            experiment="transfer",
                            kind="transfer",
                            parameters={"concurrency": conc},
                            metrics={},
                            repetition=rep,
                            status="failed",
                            error=cp.stderr.strip()[-500:],
                        )
                    )
                    continue
                data = json.loads(cp.stdout)
                # Second transfer into the same dest is an idempotent no-op.
                t1 = time.perf_counter()
                cp2 = self._run(
                    [
                        "--store",
                        str(store_a),
                        "transfer",
                        "base",
                        str(store_b),
                        "--transfer-concurrency",
                        str(conc),
                        "--json",
                    ]
                )
                warm_ms = (time.perf_counter() - t1) * 1000.0
                warm = json.loads(cp2.stdout) if cp2.returncode == 0 else {}
                bytes_x = data.get("bytes_transferred", 0)
                self.add(
                    Record(
                        experiment="transfer",
                        kind="transfer",
                        label="local-filesystem",
                        parameters={
                            "concurrency": conc,
                            "bytes": bytes_x,
                            "cache": "cold",
                        },
                        metrics={
                            "duration_ms": elapsed_ms,
                            "throughput_mib_s": (
                                (bytes_x / (1024 * 1024)) / (elapsed_ms / 1000.0)
                                if elapsed_ms > 0
                                else None
                            ),
                            "bytes": bytes_x,
                            "chunks_total": data.get("chunks_total"),
                            "chunks_skipped": data.get("chunks_skipped"),
                        },
                        repetition=rep,
                    )
                )
                self.add(
                    Record(
                        experiment="transfer",
                        kind="transfer",
                        label="local-filesystem (idempotent rerun)",
                        parameters={
                            "concurrency": conc,
                            "bytes": 0,
                            "cache": "warm",
                        },
                        metrics={
                            "duration_ms": warm_ms,
                            "bytes": 0,
                            "chunks_skipped": warm.get("chunks_skipped"),
                        },
                        repetition=rep,
                    )
                )
                shutil.rmtree(store_a, ignore_errors=True)
                shutil.rmtree(store_b, ignore_errors=True)

    def experiment_incremental(self) -> None:
        """Incremental reuse: capture child after a controlled change."""
        base_dir = self.workdir / "inc-base"
        child_dir = self.workdir / "inc-child"
        base_dir.mkdir(parents=True, exist_ok=True)
        child_dir.mkdir(parents=True, exist_ok=True)
        n_files = 8
        per = (64 * 1024 * 1024) // n_files
        for i in range(n_files):
            data = _blob(i, per)
            (base_dir / f"f{i}.img").write_bytes(data)
            # Child: change exactly one file.
            if i == 0:
                (child_dir / f"f{i}.img").write_bytes(_blob(1000, per))
            else:
                (child_dir / f"f{i}.img").write_bytes(data)

        for rep in range(self.args.repetitions):
            store = self.workdir / f"inc-store-{rep}"
            shutil.rmtree(store, ignore_errors=True)
            common = [
                "--store",
                str(store),
                "--chunk-size",
                str(self.args.incremental_chunk),
            ]
            cp = self._run(common + ["store", str(base_dir), "--id", "base", "--json"])
            if cp.returncode != 0:
                self.add(
                    Record(
                        experiment="incremental",
                        kind="incremental",
                        parameters={},
                        metrics={},
                        repetition=rep,
                        status="failed",
                        error=cp.stderr.strip()[-500:],
                    )
                )
                continue
            cp = self._run(
                common
                + [
                    "store",
                    str(child_dir),
                    "--id",
                    "child",
                    "--parent",
                    "base",
                    "--json",
                ]
            )
            if cp.returncode != 0:
                self.add(
                    Record(
                        experiment="incremental",
                        kind="incremental",
                        parameters={},
                        metrics={},
                        repetition=rep,
                        status="failed",
                        error=cp.stderr.strip()[-500:],
                    )
                )
                continue
            data = json.loads(cp.stdout)
            reuse = data.get("reuse", {})
            self.add(
                Record(
                    experiment="incremental",
                    kind="incremental",
                    label="content-addressed reuse",
                    parameters={
                        "chunk_size": self.args.incremental_chunk,
                        "files": n_files,
                    },
                    metrics={
                        "reuse_ratio": reuse.get("reuse_ratio"),
                        "reused_bytes": reuse.get("reused_bytes"),
                        "new_bytes": reuse.get("new_bytes"),
                        "reused_chunks": reuse.get("reused_chunks"),
                        "new_chunks": reuse.get("new_chunks"),
                        "bytes": data["stats"]["total_bytes"],
                    },
                    repetition=rep,
                )
            )
            shutil.rmtree(store, ignore_errors=True)

    def experiment_criu(self) -> None:
        """Real CRIU checkpoint/restore — recorded as skipped when unsupported."""
        can = (
            self.env.get("config_checkpoint_restore") is True
            and self.env.get("ns_last_pid_present") is True
            and self.env.get("criu_version") is not None
        )
        params = {"memory_mib": self.args.sizes[:1]}
        if not can:
            self.add(
                Record(
                    experiment="criu",
                    kind="csvr",
                    label="direct CRIU checkpoint/restore",
                    parameters=params,
                    metrics={},
                    status="skipped",
                    skip_reason=(
                        "host cannot run CRIU: "
                        f"CONFIG_CHECKPOINT_RESTORE={self.env.get('config_checkpoint_restore')}, "
                        f"ns_last_pid={self.env.get('ns_last_pid_present')}, "
                        f"criu={self.env.get('criu_version')}"
                    ),
                )
            )
            return
        # On a capable host this would launch the workload, run
        # `snapforge checkpoint`, then `snapforge restore`, and time each stage.
        self.add(
            Record(
                experiment="criu",
                kind="csvr",
                label="direct CRIU checkpoint/restore",
                parameters=params,
                metrics={},
                status="skipped",
                skip_reason="CRIU path not exercised by this harness build",
            )
        )

    # -- output ------------------------------------------------------------ #
    def write_outputs(self) -> None:
        out = Path(self.args.out)
        out.mkdir(parents=True, exist_ok=True)
        json_records = [r.to_json(self.env) for r in self.records]

        with open(out / "raw.jsonl", "w") as f:
            for rec in json_records:
                f.write(json.dumps(rec) + "\n")

        _write_csv(out / "raw.csv", json_records)
        _write_summary(out / "summary.md", json_records, self.env)
        _maybe_plot(out, json_records)
        print(f"\nWrote results to {out}")


def _blob(seed: int, length: int) -> bytes:
    # Deterministic per-seed pseudo-random bytes; C-level speed via random.
    return _RAND.bytes_for(seed, length)


# --------------------------------------------------------------------------- #
# Output writers
# --------------------------------------------------------------------------- #
def _write_csv(path: Path, records: list[dict[str, Any]]) -> None:
    metric_keys: set[str] = set()
    param_keys: set[str] = set()
    for r in records:
        metric_keys.update(r["metrics"].keys())
        param_keys.update(r["parameters"].keys())
    metric_cols = sorted(metric_keys)
    param_cols = sorted(param_keys)
    base = [
        "run_id",
        "timestamp",
        "experiment",
        "kind",
        "status",
        "repetition",
        "skip_reason",
        "error",
    ]
    with open(path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(base + [f"param.{k}" for k in param_cols] + metric_cols)
        for r in records:
            row = [r.get(c) for c in base]
            row += [r["parameters"].get(k) for k in param_cols]
            row += [r["metrics"].get(k) for k in metric_cols]
            w.writerow(row)


def _fmt(value: Any) -> str:
    if value is None:
        return "—"
    if isinstance(value, float):
        if value != value:  # NaN
            return "—"
        return f"{value:.2f}"
    return str(value)


def _median_spread(values: list[float]) -> tuple[float, float, float]:
    values = [v for v in values if v is not None]
    if not values:
        return (float("nan"), float("nan"), float("nan"))
    med = statistics.median(values)
    if len(values) == 1:
        return (med, med, med)
    return (med, min(values), max(values))


def _write_summary(path: Path, records: list[dict[str, Any]], env: dict[str, Any]) -> None:
    lines: list[str] = []
    lines.append("# SnapForge Benchmark Summary\n")
    lines.append("> Generated by `benchmarks/run.py`. Values are measured, not "
                 "fabricated. `—` means not applicable.\n")
    lines.append("## Environment\n")
    lines.append("| Field | Value |")
    lines.append("|---|---|")
    for k in [
        "os",
        "kernel",
        "arch",
        "cpu_model",
        "cpu_count",
        "mem_total_kib",
        "python",
        "governor",
        "criu_version",
        "config_checkpoint_restore",
        "ns_last_pid_present",
        "privileges",
    ]:
        lines.append(f"| {k} | {_fmt(env.get(k))} |")
    lines.append("")

    # Group by experiment + status.
    by_exp: dict[str, list[dict[str, Any]]] = {}
    skipped: list[dict[str, Any]] = []
    for r in records:
        if r["status"] == "skipped":
            skipped.append(r)
        elif r["status"] == "ok":
            by_exp.setdefault(r["experiment"], []).append(r)

    # Storage pipeline: median throughput + duration by chunk/concurrency/size.
    if "storage_pipeline" in by_exp:
        lines.append("## Storage pipeline\n")
        lines.append(
            "Median duration and throughput across repetitions. "
            "Peak RSS is a single-process maximum (KiB).\n"
        )
        lines.append(
            "| size MiB | chunk KiB | concurrency | n | duration ms (median) "
            "| throughput MiB/s (median) | peak RSS KiB |"
        )
        lines.append("|---|---|---|---|---|---|---|")
        groups: dict[tuple, list[dict]] = {}
        for r in by_exp["storage_pipeline"]:
            p = r["parameters"]
            key = (p["size_mib"], p["chunk_size"], p["concurrency"])
            groups.setdefault(key, []).append(r)
        for key in sorted(groups):
            recs = groups[key]
            dur = _median_spread([x["metrics"].get("duration_ms") for x in recs])
            thr = _median_spread([x["metrics"].get("throughput_mib_s") for x in recs])
            rss = max(
                (x["metrics"].get("peak_rss_kib") or 0 for x in recs), default=0
            )
            size_mib, chunk, conc = key
            lines.append(
                f"| {size_mib} | {chunk // 1024} | {conc} | {len(recs)} "
                f"| {_fmt(dur[0])} | {_fmt(thr[0])} | {rss} |"
            )
        lines.append("")

    # Transfer.
    if "transfer" in by_exp:
        lines.append("## Transfer (local filesystem)\n")
        lines.append(
            "Cold = fresh destination (chunks copied). Warm = idempotent rerun "
            "(all chunks skipped).\n"
        )
        lines.append("| cache | concurrency | n | duration ms (median) "
                     "| throughput MiB/s (median) |")
        lines.append("|---|---|---|---|---|")
        groups = {}
        for r in by_exp["transfer"]:
            key = (r["parameters"].get("cache"), r["parameters"]["concurrency"])
            groups.setdefault(key, []).append(r)
        for key in sorted(groups, key=lambda k: (str(k[0]), k[1])):
            recs = groups[key]
            dur = _median_spread([x["metrics"].get("duration_ms") for x in recs])
            thr = _median_spread([x["metrics"].get("throughput_mib_s") for x in recs])
            lines.append(
                f"| {key[0]} | {key[1]} | {len(recs)} | {_fmt(dur[0])} "
                f"| {_fmt(thr[0])} |"
            )
        lines.append("")

    # Incremental.
    if "incremental" in by_exp:
        lines.append("## Incremental reuse\n")
        lines.append("| chunk KiB | n | reuse ratio (median) | reused bytes "
                     "| new bytes |")
        lines.append("|---|---|---|---|---|")
        groups = {}
        for r in by_exp["incremental"]:
            key = r["parameters"].get("chunk_size")
            groups.setdefault(key, []).append(r)
        for key in sorted(groups):
            recs = groups[key]
            ratio = _median_spread([x["metrics"].get("reuse_ratio") for x in recs])
            reused = _median_spread([x["metrics"].get("reused_bytes") for x in recs])
            new = _median_spread([x["metrics"].get("new_bytes") for x in recs])
            lines.append(
                f"| {key // 1024} | {len(recs)} | {_fmt(ratio[0])} "
                f"| {_fmt(reused[0])} | {_fmt(new[0])} |"
            )
        lines.append("")

    # Skipped experiments.
    if skipped:
        lines.append("## Skipped experiments\n")
        lines.append("These were **not** run on this host; the reason is recorded "
                     "verbatim. No values are fabricated.\n")
        lines.append("| experiment | kind | reason |")
        lines.append("|---|---|---|")
        for r in skipped:
            lines.append(
                f"| {r['experiment']} | {r['kind']} | {_fmt(r.get('skip_reason'))} |"
            )
        lines.append("")

    path.write_text("\n".join(lines))


def _maybe_plot(out: Path, records: list[dict[str, Any]]) -> None:
    try:
        import matplotlib

        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except Exception:
        print("  (matplotlib not available; skipping plots)")
        return

    plots = out / "plots"
    plots.mkdir(exist_ok=True)

    # Storage throughput vs chunk size (per data size, concurrency=4).
    storage = [
        r
        for r in records
        if r["experiment"] == "storage_pipeline" and r["status"] == "ok"
    ]
    if storage:
        by_size: dict[int, dict[int, list[float]]] = {}
        for r in storage:
            p = r["parameters"]
            if p["concurrency"] != 4:
                continue
            by_size.setdefault(p["size_mib"], {}).setdefault(p["chunk_size"], []).append(
                r["metrics"]["throughput_mib_s"]
            )
        plt.figure()
        for size, chunks in sorted(by_size.items()):
            xs = sorted(chunks)
            ys = [statistics.median(chunks[c]) for c in xs]
            plt.plot([x // 1024 for x in xs], ys, marker="o", label=f"{size} MiB")
        plt.xlabel("chunk size (KiB)")
        plt.ylabel("throughput (MiB/s)")
        plt.title("Storage throughput vs chunk size (concurrency=4)")
        plt.legend()
        plt.grid(True, alpha=0.3)
        plt.tight_layout()
        plt.savefig(plots / "storage_throughput.png", dpi=120)
        plt.close()
        print(f"  wrote {plots / 'storage_throughput.png'}")

    # Transfer concurrency.
    xfer = [
        r for r in records if r["experiment"] == "transfer" and r["status"] == "ok"
        and r["parameters"].get("cache") == "cold"
    ]
    if xfer:
        by_conc: dict[int, list[float]] = {}
        for r in xfer:
            by_conc.setdefault(r["parameters"]["concurrency"], []).append(
                r["metrics"]["duration_ms"]
            )
        xs = sorted(by_conc)
        ys = [statistics.median(by_conc[c]) for c in xs]
        plt.figure()
        plt.plot(xs, ys, marker="s")
        plt.xlabel("transfer concurrency")
        plt.ylabel("duration (ms)")
        plt.title("Transfer duration vs concurrency")
        plt.grid(True, alpha=0.3)
        plt.tight_layout()
        plt.savefig(plots / "transfer_concurrency.png", dpi=120)
        plt.close()
        print(f"  wrote {plots / 'transfer_concurrency.png'}")


# --------------------------------------------------------------------------- #
# CLI
# --------------------------------------------------------------------------- #
def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description="SnapForge benchmark harness")
    p.add_argument("--out", default=str(DEFAULT_OUT))
    p.add_argument("--workdir", default=None)
    p.add_argument("--repetitions", type=int, default=5)
    p.add_argument(
        "--sizes",
        type=lambda s: [int(x) for x in s.split(",")],
        default=[64, 256],
        help="data sizes in MiB (storage + criu experiments)",
    )
    p.add_argument(
        "--chunk-sizes",
        type=lambda s: [int(x) for x in s.split(",")],
        default=[65536, 262144, 1048576, 4194304],
    )
    p.add_argument(
        "--concurrencies",
        type=lambda s: [int(x) for x in s.split(",")],
        default=[1, 4, 8],
    )
    p.add_argument(
        "--transfer-concurrencies",
        type=lambda s: [int(x) for x in s.split(",")],
        default=[1, 4, 8],
    )
    p.add_argument("--transfer-mib", type=int, default=64)
    p.add_argument("--transfer-chunk", type=int, default=262144)
    p.add_argument("--incremental-chunk", type=int, default=262144)
    p.add_argument(
        "--only",
        choices=["all", "storage", "transfer", "incremental", "criu"],
        default="all",
    )
    p.add_argument(
        "--quick",
        action="store_true",
        help="small/fast sweep for smoke-testing the harness",
    )
    args = p.parse_args(argv)
    if args.quick:
        args.repetitions = 2
        args.sizes = [64]
        args.chunk_sizes = [262144, 1048576]
        args.concurrencies = [4]
        args.transfer_concurrencies = [1, 4]
        args.transfer_mib = 64
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    harness = Harness(args)
    print(f"SnapForge benchmark harness -> {args.out}")
    print(f"  binary: {harness.bin}")
    print(f"  env:    {harness.env.get('kernel')} / {harness.env.get('cpu_model')}")

    if args.only in ("all", "storage"):
        print("\n[storage_pipeline]")
        harness.experiment_storage()
    if args.only in ("all", "transfer"):
        print("\n[transfer]")
        harness.experiment_transfer()
    if args.only in ("all", "incremental"):
        print("\n[incremental]")
        harness.experiment_incremental()
    if args.only in ("all", "criu"):
        print("\n[criu]")
        harness.experiment_criu()

    harness.write_outputs()
    return 0


if __name__ == "__main__":
    sys.exit(main())
