//! Sandbox levels (T080 section 2, phase 6): how much a tool command may
//! touch. `workspace-write` is the mount plan of phase 1; `read-only` keeps
//! the same plan with nothing writable except a private `/tmp`;
//! `full-access` runs commands directly on the host.

/// Profile name that selects the read-only mount plan. It travels in
/// `ExecutionConfig::default_profile` of the effective per-call config only;
/// configuration files cannot select it.
pub const READ_ONLY_PROFILE: &str = "read-only";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxLevel {
    ReadOnly,
    WorkspaceWrite,
    FullAccess,
}

impl SandboxLevel {
    pub const NAMES: &'static str = "read-only, workspace-write or full-access";

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "read-only" | "read_only" => Some(Self::ReadOnly),
            "workspace-write" | "workspace_write" => Some(Self::WorkspaceWrite),
            "full-access" | "full_access" | "danger-full-access" => Some(Self::FullAccess),
            _ => None,
        }
    }

    /// Unknown configured values are refused by config validation; anything
    /// that slips through is treated as the default.
    pub fn from_config(name: &str) -> Self {
        Self::parse(name).unwrap_or(Self::WorkspaceWrite)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::FullAccess => "full-access",
        }
    }
}

/// Output fragments that show a command failed because of the sandbox: a
/// write to a read-only mount, or a lookup or connection without network.
const BLOCK_SIGNATURES: [&str; 5] = [
    "read-only file system",
    "could not resolve host",
    "temporary failure in name resolution",
    "network is unreachable",
    "failed to lookup address information",
];

/// Whether a failed sandboxed command's output looks like a sandbox block.
/// A false match only costs the user a prompt.
pub fn looks_blocked_by_sandbox(output: &str) -> bool {
    let output = output.to_ascii_lowercase();
    BLOCK_SIGNATURES
        .iter()
        .any(|signature| output.contains(signature))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_parse_and_print_their_names() {
        for level in [
            SandboxLevel::ReadOnly,
            SandboxLevel::WorkspaceWrite,
            SandboxLevel::FullAccess,
        ] {
            assert_eq!(SandboxLevel::parse(level.as_str()), Some(level));
        }
        assert_eq!(
            SandboxLevel::parse("danger-full-access"),
            Some(SandboxLevel::FullAccess)
        );
        assert_eq!(SandboxLevel::parse("off"), None);
        assert_eq!(
            SandboxLevel::from_config("nonsense"),
            SandboxLevel::WorkspaceWrite
        );
    }

    #[test]
    fn sandbox_blocks_are_recognised_in_command_output() {
        for output in [
            "touch: cannot touch 'x': Read-only file system",
            "fatal: Unable to create '/p/.git/index.lock': Read-only file system",
            "curl: (6) Could not resolve host: example.com",
            "ssh: Temporary failure in name resolution",
            "connect: Network is unreachable",
            "error: failed to lookup address information: Name or service not known",
        ] {
            assert!(looks_blocked_by_sandbox(output), "{output}");
        }
        for output in [
            "error[E0308]: mismatched types",
            "touch: cannot touch 'x': Permission denied",
            "",
        ] {
            assert!(!looks_blocked_by_sandbox(output), "{output}");
        }
    }
}
