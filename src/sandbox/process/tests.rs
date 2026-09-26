use super::*;

#[cfg(target_os = "linux")]
pub(crate) struct LinuxIdentityKillGuard(pub(crate) Option<ProcessIdentity>);

#[cfg(target_os = "linux")]
impl LinuxIdentityKillGuard {
    pub(crate) fn new(identity: ProcessIdentity) -> Self {
        Self(Some(identity))
    }

    pub(crate) fn disarm(&mut self) {
        self.0 = None;
    }
}

#[cfg(target_os = "linux")]
impl Drop for LinuxIdentityKillGuard {
    fn drop(&mut self) {
        if let Some(identity) = &self.0 {
            let _ = signal_linux_process_identity(identity);
        }
    }
}

pub(crate) fn process_namespace_snapshot(path: &Path) -> Vec<(OsString, Vec<u8>)> {
    let mut snapshot = std::fs::read_dir(path)
        .expect("read process namespace")
        .map(|entry| {
            let entry = entry.expect("process namespace entry");
            (
                entry.file_name(),
                std::fs::read(entry.path()).expect("process namespace bytes"),
            )
        })
        .collect::<Vec<_>>();
    snapshot.sort_by(|left, right| left.0.cmp(&right.0));
    snapshot
}

pub(crate) fn git_project() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("temp project");
    std::fs::create_dir(root.path().join(".nib")).expect("state root");
    root
}

#[cfg(target_os = "linux")]
pub(crate) fn linux_process_command_contains(token: &str) -> bool {
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return false;
    };
    processes.flatten().any(|entry| {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .filter(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
            .map(str::to_string)
        else {
            return false;
        };
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|command| {
                String::from_utf8_lossy(&command)
                    .split('\0')
                    .any(|argument| argument.contains(token))
            })
            .unwrap_or(false)
    })
}

#[path = "test_part_0.rs"]
mod test_part_0;
#[path = "test_part_1.rs"]
mod test_part_1;
