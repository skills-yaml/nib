#![cfg(target_os = "linux")]

use nib::sandbox::process::ProcessScopeBackend;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const HIGH_DESCRIPTOR_CHILD: &str = "NIB_TEST_HIGH_DESCRIPTOR_MANAGED_PROBE";

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

#[test]
fn managed_process_probe_survives_high_descriptor_allocation() {
    if std::env::var_os(HIGH_DESCRIPTOR_CHILD).is_some() {
        let mut reservations = Vec::new();
        loop {
            let file = File::open("/dev/null").expect("descriptor reservation");
            let descriptor = file.as_raw_fd();
            reservations.push(file);
            if descriptor >= 63 {
                break;
            }
        }
        ProcessScopeBackend::production().expect("managed probe with high descriptors");
        std::hint::black_box(&reservations);
        return;
    }

    let Some(real_bwrap) = find_bwrap() else {
        assert!(
            std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_none(),
            "CI requires an installed bwrap"
        );
        return;
    };
    let fixture = tempfile::tempdir().expect("dynamic-descriptor wrapper directory");
    let wrapper = fixture.path().join("bwrap");
    let script = format!(
        "#!/bin/sh\nprevious=\nfound_info=\nfor arg in \"$@\"; do\n  if [ \"$arg\" = \"--sync-fd\" ]; then exit 91; fi\n  if [ \"$previous\" = \"--info-fd\" ]; then\n    case $arg in ''|*[!0-9]*) exit 92;; esac\n    if [ \"$arg\" -lt 64 ]; then exit 93; fi\n    found_info=1\n  fi\n  previous=$arg\ndone\nif [ -z \"$found_info\" ]; then exit 94; fi\nexec {} \"$@\"\n",
        shell_quote(&real_bwrap)
    );
    std::fs::write(&wrapper, script).expect("dynamic-descriptor bwrap wrapper");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
        .expect("executable dynamic-descriptor wrapper");
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let mut path_entries = vec![fixture.path().to_path_buf()];
    path_entries.extend(std::env::split_paths(&original_path));
    let wrapped_path = std::env::join_paths(path_entries).expect("dynamic-descriptor PATH");
    let status = Command::new(std::env::current_exe().expect("current test executable"))
        .args([
            "--exact",
            "managed_process_probe_survives_high_descriptor_allocation",
            "--nocapture",
        ])
        .env(HIGH_DESCRIPTOR_CHILD, "1")
        .env("NIB_POSIX_SHELL", "/bin/sh")
        .env("NIB_REQUIRE_BWRAP_TESTS", "1")
        .env("PATH", wrapped_path)
        .status()
        .expect("high-descriptor probe child");
    assert!(
        status.success(),
        "high-descriptor probe child failed: {status}"
    );
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
