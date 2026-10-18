//! Host capability probing.
//!
//! SnapForge owns workflow, storage, and measurement. It delegates actual
//! process checkpoint/restore to CRIU. This module answers, truthfully, what
//! the current host can and cannot do, so that higher layers can refuse
//! operations instead of fabricating results.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// Broad summary of whether real CRIU checkpoint/restore is usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CriuReadiness {
    /// CRIU is present and the kernel/privilege preconditions look satisfied.
    Ready,
    /// CRIU binary is missing.
    MissingBinary,
    /// CRIU is present but the kernel lacks the required feature.
    UnsupportedKernel,
    /// CRIU is present but the current process lacks needed privileges.
    InsufficientPrivileges,
}

/// Result of probing a single capability. `ok == false` always carries a
/// human-readable `detail` explaining what is missing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

impl Check {
    fn ok(name: &str, detail: impl Into<String>) -> Self {
        Check {
            name: name.into(),
            ok: true,
            detail: detail.into(),
        }
    }
    fn fail(name: &str, detail: impl Into<String>) -> Self {
        Check {
            name: name.into(),
            ok: false,
            detail: detail.into(),
        }
    }
}

/// A structured snapshot of the host environment relevant to SnapForge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostReport {
    pub os: String,
    pub kernel_release: String,
    pub arch: String,
    pub uid: u32,
    pub is_root: bool,
    pub criu_version: Option<String>,
    pub criu_path: Option<PathBuf>,
    pub criu_readiness: CriuReadiness,
    /// Kernel built with `CONFIG_CHECKPOINT_RESTORE`.
    pub kernel_checkpoint_restore: Option<bool>,
    /// `kernel.unprivileged_userns_clone` (0 often means user namespaces are restricted).
    pub unprivileged_userns_clone: Option<i64>,
    /// Presence of the `ns_last_pid` sysctl CRIU relies on for PID restoration.
    pub ns_last_pid_present: bool,
    /// Bounded set of individual checks with explanations.
    pub checks: Vec<Check>,
}

impl HostReport {
    /// True only when every blocking check passed.
    pub fn all_ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }

    /// Human-readable multi-line report.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("SnapForge host report\n");
        out.push_str("=====================\n");
        out.push_str(&format!("OS:      {}\n", self.os));
        out.push_str(&format!("Kernel:  {}\n", self.kernel_release));
        out.push_str(&format!("Arch:    {}\n", self.arch));
        out.push_str(&format!("UID:     {} (root: {})\n", self.uid, self.is_root));
        out.push_str(&format!(
            "CRIU:    {} (readiness: {:?})\n",
            self.criu_version.as_deref().unwrap_or("not found"),
            self.criu_readiness
        ));
        out.push('\n');
        for c in &self.checks {
            let mark = if c.ok { "OK  " } else { "FAIL" };
            out.push_str(&format!("  [{}] {:<28} {}\n", mark, c.name, c.detail));
        }
        out.push('\n');
        if self.criu_readiness == CriuReadiness::Ready {
            out.push_str("Real CRIU checkpoint/restore: SUPPORTED on this host.\n");
        } else {
            out.push_str(
                "Real CRIU checkpoint/restore: NOT available on this host.\n\
                 SnapForge-owned storage/transfer/verification features remain usable.\n",
            );
        }
        out
    }
}

/// Probe the host environment.
pub fn probe() -> HostReport {
    let kernel_release = read_trimmed("/proc/sys/kernel/osrelease")
        .unwrap_or_else(|| std::env::consts::OS.to_string());
    let os = read_os_pretty_name().unwrap_or_else(|| std::env::consts::OS.to_string());
    let arch = std::env::consts::ARCH.to_string();
    let uid = unsafe { libc::geteuid() };
    let is_root = uid == 0;

    let kernel_checkpoint_restore = read_kernel_config("CONFIG_CHECKPOINT_RESTORE");
    let unprivileged_userns_clone = read_i64("/proc/sys/kernel/unprivileged_userns_clone");
    let ns_last_pid_present = Path::new("/proc/sys/kernel/ns_last_pid").exists();

    let (criu_path, criu_version) = detect_criu();

    let mut checks = Vec::new();

    match kernel_checkpoint_restore {
        Some(true) => checks.push(Check::ok(
            "kernel.CHECKPOINT_RESTORE",
            "CONFIG_CHECKPOINT_RESTORE=y",
        )),
        Some(false) => checks.push(Check::fail(
            "kernel.CHECKPOINT_RESTORE",
            "kernel built without CONFIG_CHECKPOINT_RESTORE; CRIU cannot capture a process",
        )),
        None => checks.push(Check::fail(
            "kernel.CHECKPOINT_RESTORE",
            "could not read /proc/config.gz; install kernel config or use a kernel with the feature",
        )),
    }

    if ns_last_pid_present {
        checks.push(Check::ok("kernel.ns_last_pid", "sysctl present"));
    } else {
        checks.push(Check::fail(
            "kernel.ns_last_pid",
            "/proc/sys/kernel/ns_last_pid missing; CRIU needs it to restore PIDs",
        ));
    }

    if let Some(ref path) = criu_path {
        checks.push(Check::ok(
            "criu.binary",
            format!("found at {}", path.display()),
        ));
    } else {
        checks.push(Check::fail("criu.binary", "criu not found on PATH"));
    }

    match unprivileged_userns_clone {
        Some(0) => checks.push(Check::ok(
            "kernel.unprivileged_userns_clone",
            "restricted (typical for CRIU on hardened hosts)",
        )),
        Some(v) => checks.push(Check::ok(
            "kernel.unprivileged_userns_clone",
            format!("value {v}"),
        )),
        None => checks.push(Check::ok(
            "kernel.unprivileged_userns_clone",
            "not present (kernel may not expose this knob)",
        )),
    }

    if is_root || has_ptrace_cap() {
        checks.push(Check::ok(
            "privileges",
            if is_root {
                "running as root"
            } else {
                "CAP_SYS_PTRACE available"
            },
        ));
    } else {
        checks.push(Check::fail(
            "privileges",
            "not root and no CAP_SYS_PTRACE; CRIU needs ptrace/namespace privileges",
        ));
    }

    let criu_readiness = classify(criu_version.is_some(), kernel_checkpoint_restore, is_root);

    HostReport {
        os,
        kernel_release,
        arch,
        uid,
        is_root,
        criu_version,
        criu_path,
        criu_readiness,
        kernel_checkpoint_restore,
        unprivileged_userns_clone,
        ns_last_pid_present,
        checks,
    }
}

fn classify(criu_present: bool, kernel_cr: Option<bool>, is_root: bool) -> CriuReadiness {
    if !criu_present {
        return CriuReadiness::MissingBinary;
    }
    if kernel_cr != Some(true) || {
        // ns_last_pid is required; treat its absence as unsupported kernel too.
        !Path::new("/proc/sys/kernel/ns_last_pid").exists()
    } {
        return CriuReadiness::UnsupportedKernel;
    }
    if !is_root && !has_ptrace_cap() {
        return CriuReadiness::InsufficientPrivileges;
    }
    CriuReadiness::Ready
}

fn has_ptrace_cap() -> bool {
    // Parse CapEff from /proc/self/status; CAP_SYS_PTRACE is bit 19.
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(hex) = line.strip_prefix("CapEff:") {
                if let Ok(mask) = u64::from_str_radix(hex.trim(), 16) {
                    return mask & (1 << 19) != 0;
                }
            }
        }
    }
    false
}

fn detect_criu() -> (Option<PathBuf>, Option<String>) {
    let path = which("criu");
    let Some(ref p) = path else {
        return (None, None);
    };
    let version = Command::new(p)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        });
    (path, version)
}

/// Minimal `which` that avoids a shell and searches `PATH`.
fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_i64(path: &str) -> Option<i64> {
    read_trimmed(path)?.parse().ok()
}

fn read_os_pretty_name() -> Option<String> {
    let content = std::fs::read_to_string("/etc/os-release").ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("PRETTY_NAME=") {
            return Some(rest.trim_matches('"').to_string());
        }
    }
    None
}

/// Read a kernel config symbol from `/proc/config.gz` (gzip) or
/// `/boot/config-<release>`. Returns `None` when the symbol is absent or the
/// config cannot be read.
fn read_kernel_config(symbol: &str) -> Option<bool> {
    let text = read_kernel_config_text()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(&format!("{symbol}=")) {
            return Some(rest.trim() == "y");
        }
        if line.trim() == format!("# {symbol} is not set") {
            return Some(false);
        }
    }
    Some(false)
}

fn read_kernel_config_text() -> Option<String> {
    // Preferred: decompress /proc/config.gz using flate2-free approach is not
    // available here; shell out to `zcat` only if present, else fall back to
    // the boot config file. We avoid adding a compression dependency in Phase 1.
    if let Ok(raw) = std::fs::read("/proc/config.gz") {
        if let Some(text) = gunzip(&raw) {
            return Some(text);
        }
    }
    let release = read_trimmed("/proc/sys/kernel/osrelease")?;
    for candidate in [
        format!("/boot/config-{release}"),
        "/proc/config".to_string(),
    ] {
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            return Some(text);
        }
    }
    None
}

/// A tiny gzip (DEFLATE) decoder is out of scope; instead try to reuse a
/// system `zcat`. Returns `None` if unavailable.
fn gunzip(data: &[u8]) -> Option<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("zcat")
        .arg("/proc/config.gz")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .ok()?;
    let _ = data; // zcat reads the file directly
    let mut out = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        std::io::Read::read_to_end(&mut stdout, &mut out).ok()?;
    }
    let mut sink = std::io::sink();
    let _ = sink.flush();
    let status = child.wait().ok()?;
    if !status.success() {
        return None;
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_returns_something() {
        let report = probe();
        assert!(!report.arch.is_empty());
        assert!(!report.checks.is_empty());
    }

    #[test]
    fn classify_missing_binary() {
        assert_eq!(
            classify(false, Some(true), true),
            CriuReadiness::MissingBinary
        );
    }

    #[test]
    fn classify_unsupported_kernel() {
        assert_eq!(
            classify(true, Some(false), true),
            CriuReadiness::UnsupportedKernel
        );
    }

    #[test]
    fn render_contains_status_line() {
        let report = probe();
        let s = report.render();
        assert!(s.contains("SnapForge host report"));
        assert!(s.contains("Real CRIU checkpoint/restore"));
    }
}
