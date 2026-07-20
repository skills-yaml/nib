#![cfg(target_os = "linux")]

use nib::sandbox::process::ProcessScopeBackend;
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

struct EnvironmentGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvironmentGuard {
    fn set(key: &'static str, value: &OsStr) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

#[test]
fn managed_process_probe_is_independent_from_broad_network_sandbox_probe() {
    let Some(real_bwrap) = find_bwrap() else {
        assert!(
            std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_none(),
            "CI requires an installed bwrap"
        );
        return;
    };
    let fixture = tempfile::tempdir().expect("wrapper directory");
    let marker = fixture.path().join("broad-probe.marker");
    let wrapper = fixture.path().join("bwrap");
    let script = format!(
        "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = \"--unshare-net\" ]; then\n    : > \"$NIB_BROAD_PROBE_MARKER\"\n    printf '%s\\n' 'bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted' >&2\n    exit 1\n  fi\ndone\nexec {} \"$@\"\n",
        shell_quote(&real_bwrap)
    );
    std::fs::write(&wrapper, script).expect("bwrap wrapper");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
        .expect("executable bwrap wrapper");

    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let mut path_entries = vec![fixture.path().to_path_buf()];
    path_entries.extend(std::env::split_paths(&original_path));
    let wrapped_path = std::env::join_paths(path_entries).expect("wrapped PATH");
    let _path = EnvironmentGuard::set("PATH", &wrapped_path);
    let _marker = EnvironmentGuard::set("NIB_BROAD_PROBE_MARKER", marker.as_os_str());

    let production = ProcessScopeBackend::production();
    assert!(
        !marker.exists(),
        "managed-process preflight invoked the broad network sandbox probe"
    );
    if let Err(error) = production {
        assert!(
            std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_none(),
            "CI requires the exact managed-process backend: {error}"
        );
        return;
    }

    let capabilities = nib::sandbox::detect_capabilities();
    assert!(capabilities.bwrap_installed);
    assert!(!capabilities.bwrap_available);
    assert!(marker.exists(), "broad sandbox probe did not run");
    assert!(capabilities
        .bwrap_error
        .as_deref()
        .is_some_and(|error| error.contains("RTM_NEWADDR")));
    assert!(capabilities.managed_process_available);
    assert!(capabilities.managed_process_error.is_none());
}

fn find_bwrap() -> Option<PathBuf> {
    let output = Command::new("sh")
        .args(["-c", "command -v bwrap"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = PathBuf::from(path.trim());
    path.is_file().then_some(path)
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}
