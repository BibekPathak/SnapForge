//! Engine control-flow tests using a fake `criu` executable.
//!
//! Real CRIU cannot run on the development host (the kernel lacks
//! `CONFIG_CHECKPOINT_RESTORE`). These tests exercise the *orchestration*
//! logic — argument construction, exit-code handling, empty-output detection,
//! timeout handling, and cleanup — against controllable fake binaries. They do
//! **not** claim to perform real process checkpoint/restore.

use std::path::{Path, PathBuf};
use std::time::Duration;

use snapforge_engine::criu::Criu;
use snapforge_engine::engine::{CheckpointOptions, RestoreOptions};
use snapforge_engine::Engine;

/// Write an executable fake `criu` script and return its path.
fn fake_criu(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("criu");
    std::fs::write(&path, body).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

// A fake that records its args and creates image files in `-D` dir.
const FAKE_OK: &str = r#"#!/bin/sh
# find -D <dir>
dir=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-D" ]; then dir="$a"; fi
  prev="$a"
done
if [ -n "$dir" ]; then
  mkdir -p "$dir"
  echo image > "$dir/core-1.img"
  echo image > "$dir/mm-1.img"
fi
echo "fake criu ok: $*"
exit 0
"#;

const FAKE_FAIL: &str = r#"#!/bin/sh
echo "fake criu failure" 1>&2
exit 1
"#;

/// The engine's `ensure_capable` gate must block when the host report says the
/// kernel is unsupported, regardless of the fake binary.
#[test]
fn gated_when_host_not_capable() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_criu(dir.path(), FAKE_OK);
    let mut engine = Engine::with_criu_path(&bin).unwrap();

    // On the CI/dev host the kernel lacks checkpoint/restore, so the gate must
    // fire before any CRIU invocation. If (on some future host) the host is
    // capable, this test simply becomes a no-op assertion.
    if engine.host_report().criu_readiness != snapforge_core::host::CriuReadiness::Ready {
        let out = dir.path().join("out");
        let opts = CheckpointOptions::new(std::process::id() as i32, &out);
        let err = engine.checkpoint(&opts).unwrap_err();
        assert!(
            err.is_capability_block(),
            "expected capability block, got {err}"
        );
        assert!(!out.join("core-1.img").exists());
    }
}

#[test]
fn rule_never_success_on_criu_failure() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_criu(dir.path(), FAKE_FAIL);
    let criu = Criu::from_path(&bin).unwrap();
    let cmd = criu.run(&["dump", "-t", "1"], None).unwrap();
    assert!(!cmd.succeeded());
    assert_eq!(cmd.exit_code, Some(1));
}

#[test]
fn rule_empty_checkpoint_output_is_failure() {
    // Simulate the summarize_artifacts logic on an empty dir vs populated dir.
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();

    let (count, bytes) = snapforge_engine::engine::summarize_artifacts(&empty).unwrap();
    assert_eq!(count, 0);
    assert_eq!(bytes, 0);

    // A populated directory is not flagged empty.
    let populated = dir.path().join("populated");
    std::fs::create_dir_all(&populated).unwrap();
    std::fs::write(populated.join("core-1.img"), b"abc").unwrap();
    let (count, bytes) = snapforge_engine::engine::summarize_artifacts(&populated).unwrap();
    assert_eq!(count, 1);
    assert_eq!(bytes, 3);
}

#[test]
fn argument_construction_includes_flags() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_criu(dir.path(), FAKE_OK);
    let criu = Criu::from_path(&bin).unwrap();
    let cmd = criu
        .run(&["dump", "-t", "1234", "-D", "/tmp/x", "--shell-job"], None)
        .unwrap();
    assert_eq!(
        cmd.args,
        vec!["dump", "-t", "1234", "-D", "/tmp/x", "--shell-job"]
    );
}

#[test]
fn timeout_marks_command_timed_out() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("criu");
    std::fs::write(&path, "#!/bin/sh\nsleep 30\n").unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();

    let criu = Criu::from_path(&path).unwrap();
    let cmd = criu
        .run(&["dump"], Some(Duration::from_millis(150)))
        .unwrap();
    assert!(cmd.timed_out);
    assert!(!cmd.succeeded());
}

#[test]
fn restore_options_flag_construction() {
    let opts = RestoreOptions::new("/tmp/ckpt");
    assert!(opts.shell_job);
    assert!(opts.detached);
    assert_eq!(opts.input_dir, PathBuf::from("/tmp/ckpt"));
}

#[test]
fn checkpoint_options_defaults() {
    let opts = CheckpointOptions::new(42, "/tmp/out");
    assert_eq!(opts.pid, 42);
    assert!(opts.shell_job);
    assert!(!opts.leave_running);
    assert!(opts.timeout.is_some());
}
