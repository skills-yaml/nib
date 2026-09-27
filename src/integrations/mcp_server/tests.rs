use super::*;
use crate::config::{
    load_nib_config_full, save_nib_config_full, LlmApiMode, ProfileConfig, ProfilesConfig,
    ProviderEntry,
};
use crate::llm::test_support::serve_once;
use std::pin::Pin;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use tempfile::tempdir;

pub(crate) const MANAGED_PROCESS_FIXTURE_CHILD_ENV: &str = "NIB_TEST_MCP_PROCESS_SCOPE_CHILD";
pub(crate) const MANAGED_PROCESS_FIXTURE_CHILD_TEST: &str =
    "integrations::mcp_server::tests::managed_process_scope_fixture_child";

pub(crate) struct EnvironmentGuard {
    pub(crate) name: &'static str,
    pub(crate) previous: Option<std::ffi::OsString>,
}

impl EnvironmentGuard {
    pub(crate) fn set(name: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(name);
        std::env::set_var(name, value);
        Self { name, previous }
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.name, value),
            None => std::env::remove_var(self.name),
        }
    }
}

pub(crate) struct ManagedProcessFixture {
    pub(crate) store: crate::sandbox::process::ProcessScopeStore,
    pub(crate) scope: crate::sandbox::process::ProcessScopeRecord,
    pub(crate) cleanup_lease: Option<crate::sandbox::process::CleanupLease>,
    pub(crate) child: Option<std::process::Child>,
}

impl ManagedProcessFixture {
    pub(crate) fn start(project_root: &Path, scope_id: &str, execution_generation: u64) -> Self {
        let store = crate::sandbox::process::ProcessScopeStore::open(project_root)
            .expect("managed-process fixture store");
        let prepared = store
            .prepare(
                scope_id,
                "subagent",
                execution_generation,
                crate::sandbox::process::ProcessIdentity::current()
                    .expect("managed-process fixture owner"),
                native_process_scope_backend(),
            )
            .expect("prepare managed-process fixture");
        let cleanup_lease = store
            .acquire_cleanup_lease(&prepared)
            .expect("acquire managed-process fixture cleanup lease");
        let child = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                MANAGED_PROCESS_FIXTURE_CHILD_TEST,
                "--test-threads=1",
            ])
            .env(MANAGED_PROCESS_FIXTURE_CHILD_ENV, "1")
            .current_dir(project_root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn managed-process fixture child");
        let direct_child = crate::sandbox::process::ProcessIdentity::capture(child.id())
            .expect("capture managed-process fixture child");
        let mut fixture = Self {
            store,
            scope: prepared,
            cleanup_lease: Some(cleanup_lease),
            child: Some(child),
        };
        fixture.scope = fixture
            .store
            .mark_running(
                scope_id,
                execution_generation,
                &fixture.scope.cleanup_lease_id,
                crate::sandbox::process::ProcessIdentity::current()
                    .expect("managed-process fixture supervisor"),
                direct_child,
            )
            .expect("mark managed-process fixture running");
        fixture
    }

    pub(crate) fn scope(&self) -> &crate::sandbox::process::ProcessScopeRecord {
        &self.scope
    }

    pub(crate) fn complete(mut self, outcome: &str) -> crate::sandbox::process::CleanupProof {
        let mut child = self
            .child
            .take()
            .expect("managed-process fixture child remains owned");
        match child.kill() {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {}
            Err(error) => panic!("kill managed-process fixture child: {error}"),
        }
        child.wait().expect("reap managed-process fixture child");
        // Waiting on the owned process handle proves this exact child
        // generation exited. A PID lookup can still find the exited
        // process object on Windows while another handle keeps it alive.
        drop(child);
        let direct_child = self
            .scope
            .direct_child
            .clone()
            .expect("managed-process fixture direct child");
        #[cfg(not(windows))]
        assert!(
            !direct_child.still_matches(),
            "cleanup proof requires the exact child generation to be gone"
        );

        let completed_at = chrono::Utc::now();
        let proof = crate::sandbox::process::CleanupProof {
            execution_generation: self.scope.execution_generation,
            cleanup_lease_id: self.scope.cleanup_lease_id.clone(),
            backend: self.scope.backend,
            direct_child,
            outcome: outcome.to_string(),
            descendants_reaped: true,
            completed_at,
        };
        let mut complete = self.scope.clone();
        complete.status = crate::sandbox::process::ProcessScopeStatus::Complete;
        complete.cleanup_reason = Some(outcome.to_string());
        complete.cleanup_proof = Some(proof.clone());
        complete.updated_at = completed_at;

        // The production supervisor owns this transition. The fixture
        // publishes the same CAS state only after reaping its controlled child.
        let directory_path = self
            .store
            .project_root()
            .join(".nib")
            .join("process-scopes");
        let directory = crate::daemons::state::StableDirectory::open(&directory_path)
            .expect("open managed-process fixture directory");
        let scope_path = directory_path.join(format!("{}.json", complete.scope_id));
        let opened = directory
            .open_read(&scope_path)
            .expect("open running managed-process fixture");
        directory
            .save_json_atomically_expected(
                &scope_path,
                &complete,
                crate::daemons::state::FileExpectation::Present(&opened),
            )
            .expect("publish completed managed-process fixture");

        self.cleanup_lease
            .take()
            .expect("managed-process fixture cleanup lease remains owned")
            .release_after_proof(&proof)
            .expect("release managed-process fixture cleanup lease after proof");
        assert_eq!(
            self.store
                .cleanup_lease_state(&complete)
                .expect("inspect released managed-process fixture cleanup lease"),
            crate::sandbox::process::CleanupLeaseState::Missing
        );
        assert_eq!(
            self.store
                .load(&complete.scope_id)
                .expect("reload completed managed-process fixture"),
            complete
        );
        proof
    }
}

impl Drop for ManagedProcessFixture {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub(crate) fn native_process_scope_backend() -> crate::sandbox::process::ProcessScopeBackend {
    #[cfg(target_os = "linux")]
    {
        crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace
    }
    #[cfg(windows)]
    {
        crate::sandbox::process::ProcessScopeBackend::WindowsJobObject
    }
    #[cfg(target_os = "macos")]
    {
        crate::sandbox::process::ProcessScopeBackend::MacosProcessGroup
    }
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    {
        panic!("managed-process fixtures are unsupported on this platform")
    }
}

pub(crate) struct PendingWriter {
    pub(crate) polled: Arc<AtomicBool>,
    pub(crate) dropped: Arc<AtomicBool>,
}

impl AsyncWrite for PendingWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        _buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.polled.store(true, Ordering::Release);
        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl Drop for PendingWriter {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

pub(crate) fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .expect("git starts");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(crate) fn initialize_git_repository(root: &Path) {
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "nib-tests@example.invalid"]);
    git(root, &["config", "user.name", "nib tests"]);
    std::fs::write(root.join(".gitignore"), ".nib/\n").expect("gitignore");
    std::fs::write(root.join("README.md"), "fixture\n").expect("fixture");
    git(root, &["add", ".gitignore", "README.md"]);
    git(root, &["commit", "-qm", "initial"]);
}

pub(crate) fn save_profile_config(root: &Path) -> NibConfig {
    std::fs::write(
        root.join(".profile.env"),
        "NIB_PROFILE_VALUE=profile-scoped\n",
    )
    .expect("profile env");
    let mut config = NibConfig::default();
    config.execution.plan_mode = false;
    config.profiles = ProfilesConfig {
        default: "workspace".to_string(),
        active: vec![ProfileConfig {
            id: "workspace".to_string(),
            root: PathBuf::from("."),
            env_file: Some(PathBuf::from(".profile.env")),
            ..ProfileConfig::default()
        }],
    };
    save_nib_config_full(root, &mut config).expect("save config");
    config
}

pub(crate) fn test_cancellation_audit(
    store: &SessionStore,
    session_id: &str,
    tool_name: &str,
) -> (McpCancellationAuditGuard, Arc<McpCancellationAuditState>) {
    let state = Arc::new(McpCancellationAuditState {
        session_store: store.clone(),
        session_id: session_id.to_string(),
        tool_name: tool_name.to_string(),
        cancellation_id: uuid::Uuid::new_v4().to_string(),
        status: StdMutex::new(McpCancellationAuditStatus::Pending),
        injected_failures: std::sync::atomic::AtomicUsize::new(0),
        injected_post_commit_failures: std::sync::atomic::AtomicUsize::new(0),
    });
    (
        McpCancellationAuditGuard {
            state: Arc::clone(&state),
            armed: true,
        },
        state,
    )
}

pub(crate) fn subagent_start_class(tool_name: &str) -> RequestCancellationClass {
    RequestCancellationClass::SubagentStart {
        tool_name: tool_name.to_string(),
    }
}

pub(crate) fn tool_request(name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": name,
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments}
    })
}

#[path = "test_part_0.rs"]
mod test_part_0;
#[path = "test_part_1.rs"]
mod test_part_1;
