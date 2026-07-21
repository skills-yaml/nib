#[cfg(target_os = "linux")]
use nib::config::load_nib_config_full;
use nib::config::{save_nib_config_full, ExecutionConfig, McpServerEntry, NibConfig};
use nib::integrations::mcp::McpManager;
use nib::session::SessionStore;
use nib::tools::ToolExecutor;
use serde_json::json;
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::collections::HashSet;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::io::{AsyncWriteExt, BufReader};

#[cfg(debug_assertions)]
const MCP_FIXTURE_TOKEN: &str = "nib-mcp-client-lifecycle-fixture-v1";
#[cfg(debug_assertions)]
const MCP_FIXTURE_ACTIVATION_ENV: &str = "NIB_INTERNAL_MCP_LIFECYCLE_FIXTURE";
#[cfg(debug_assertions)]
const MCP_FIXTURE_MODE_ENV: &str = "NIB_INTERNAL_MCP_LIFECYCLE_MODE";
#[cfg(debug_assertions)]
const MCP_FIXTURE_HEARTBEAT_ENV: &str = "NIB_INTERNAL_MCP_LIFECYCLE_HEARTBEAT";
#[cfg(all(debug_assertions, target_os = "linux"))]
const MCP_GIT_FIXTURE_MARKER: &str = ".nib/mcp-git-lifecycle-fixture.json";
#[cfg(all(debug_assertions, target_os = "linux"))]
const MCP_GIT_FIXTURE_TOKEN: &str = "nib-mcp-git-lifecycle-fixture-v1";

#[cfg(debug_assertions)]
fn lifecycle_fixture_entry(mode: &str, heartbeat: &Path) -> McpServerEntry {
    McpServerEntry {
        command: env!("CARGO_BIN_EXE_nib").to_string(),
        env: HashMap::from([
            (
                MCP_FIXTURE_ACTIVATION_ENV.to_string(),
                MCP_FIXTURE_TOKEN.to_string(),
            ),
            (MCP_FIXTURE_MODE_ENV.to_string(), mode.to_string()),
            (
                MCP_FIXTURE_HEARTBEAT_ENV.to_string(),
                heartbeat.to_string_lossy().into_owned(),
            ),
        ]),
        request_timeout_secs: 10,
        ..McpServerEntry::default()
    }
}

#[cfg(debug_assertions)]
async fn wait_for_heartbeat(path: &Path) -> u64 {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let length = std::fs::metadata(path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            if length > 0 {
                return length;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("MCP fixture descendant heartbeat")
}

#[cfg(debug_assertions)]
async fn assert_heartbeat_stops(path: &Path) {
    let stopped = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let before = std::fs::metadata(path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            tokio::time::sleep(Duration::from_millis(150)).await;
            let after = std::fs::metadata(path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            if before == after {
                tokio::time::sleep(Duration::from_millis(150)).await;
                let confirmed = std::fs::metadata(path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0);
                if after == confirmed {
                    return;
                }
            }
        }
    })
    .await
    .is_ok();
    assert!(stopped, "managed MCP descendant heartbeat remained active");
}

#[cfg(debug_assertions)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(debug_assertions)]
fn shell_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    #[cfg(windows)]
    {
        path.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        path.into_owned()
    }
}

#[cfg(debug_assertions)]
fn terminal_process_tree_command(executable: &Path, heartbeat: &Path) -> String {
    format!(
        "{activation}={token} {mode}=process_tree {heartbeat_env}={heartbeat} exec {nib}",
        activation = MCP_FIXTURE_ACTIVATION_ENV,
        token = shell_quote(MCP_FIXTURE_TOKEN),
        mode = MCP_FIXTURE_MODE_ENV,
        heartbeat_env = MCP_FIXTURE_HEARTBEAT_ENV,
        heartbeat = shell_quote(&shell_path(heartbeat)),
        nib = shell_quote(&shell_path(executable)),
    )
}

#[cfg(debug_assertions)]
fn install_terminal_tree_fixture(root: &Path) -> std::path::PathBuf {
    let executable = root.join(if cfg!(windows) {
        "mcp-lifecycle-fixture.exe"
    } else {
        "mcp-lifecycle-fixture"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_nib"), &executable)
        .expect("copy terminal lifecycle fixture executable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&executable)
            .expect("terminal lifecycle fixture metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions)
            .expect("terminal lifecycle fixture mode");
    }
    executable
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn install_git_stall_fixture(root: &Path, heartbeat: &Path) -> std::ffi::OsString {
    let bin = root.join("mcp-git-fixture-bin");
    std::fs::create_dir(&bin).expect("MCP Git fixture bin directory");
    let wrapper = bin.join(if cfg!(windows) { "git.exe" } else { "git" });
    std::fs::copy(env!("CARGO_BIN_EXE_nib"), &wrapper).expect("copy MCP Git fixture executable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&wrapper)
            .expect("MCP Git fixture metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&wrapper, permissions).expect("MCP Git fixture mode");
    }
    std::fs::write(
        root.join(MCP_GIT_FIXTURE_MARKER),
        serde_json::to_vec(&json!({
            "token": MCP_GIT_FIXTURE_TOKEN,
            "heartbeat": heartbeat.to_string_lossy(),
        }))
        .expect("MCP Git fixture marker JSON"),
    )
    .expect("MCP Git fixture marker");

    let inherited = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&inherited)))
        .expect("MCP Git fixture PATH")
}

fn git(root: &Path, arguments: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args(arguments)
        .output()
        .expect("git starts");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn initialize_server_fixture(root: &Path) {
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "nib-tests@example.invalid"]);
    git(root, &["config", "user.name", "nib tests"]);
    std::fs::write(root.join(".gitignore"), ".nib/\n").expect("gitignore");
    std::fs::write(root.join("README.md"), "fixture\n").expect("fixture readme");
    std::fs::write(root.join("AGENTS.md"), "- nib-policy: allow run_terminal\n")
        .expect("noninteractive MCP policy");
    git(root, &["add", ".gitignore", "README.md", "AGENTS.md"]);
    git(root, &["commit", "-qm", "initial"]);

    let mut config = NibConfig::default();
    config.execution.plan_mode = false;
    config.terminal.timeout = 60;
    save_nib_config_full(root, &mut config).expect("server config");
}

fn spawn_server(
    root: &Path,
) -> (
    tokio::process::Child,
    tokio::process::ChildStdin,
    BufReader<tokio::process::ChildStdout>,
) {
    spawn_server_with_path(root, None)
}

fn spawn_server_with_path(
    root: &Path,
    path: Option<&std::ffi::OsStr>,
) -> (
    tokio::process::Child,
    tokio::process::ChildStdin,
    BufReader<tokio::process::ChildStdout>,
) {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_nib"));
    command
        .arg("mcp-server")
        .current_dir(root)
        .env("TOKIO_WORKER_THREADS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let mut child = command.spawn().expect("MCP server starts");
    let stdin = child.stdin.take().expect("MCP server stdin");
    let stdout = BufReader::new(child.stdout.take().expect("MCP server stdout"));
    (child, stdin, stdout)
}

async fn write_request(stdin: &mut tokio::process::ChildStdin, request: serde_json::Value) {
    let mut frame = serde_json::to_vec(&request).expect("request JSON");
    frame.push(b'\n');
    stdin.write_all(&frame).await.expect("write MCP request");
    stdin.flush().await.expect("flush MCP request");
}

#[cfg(debug_assertions)]
async fn start_portable_terminal_tree(
    root: &Path,
    stdin: &mut tokio::process::ChildStdin,
    request_id: &str,
    executable: &Path,
    heartbeat_name: &str,
) -> std::path::PathBuf {
    let requested_heartbeat = if cfg!(windows) {
        executable
            .parent()
            .expect("terminal lifecycle fixture parent directory")
            .join(heartbeat_name)
    } else {
        std::path::PathBuf::from(heartbeat_name)
    };
    let fixture_heartbeat = if cfg!(windows) {
        std::path::PathBuf::from(heartbeat_name)
    } else {
        requested_heartbeat.clone()
    };
    let command = terminal_process_tree_command(executable, &fixture_heartbeat);
    write_request(
        stdin,
        json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {
                "name": "run_terminal",
                "arguments": {"command": command}
            }
        }),
    )
    .await;
    let started = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if requested_heartbeat.is_absolute() {
                if std::fs::metadata(&requested_heartbeat).is_ok_and(|metadata| metadata.len() > 0)
                {
                    return requested_heartbeat.clone();
                }
            } else {
                let sessions = root.join(".nib/worktrees/sessions");
                if let Ok(entries) = std::fs::read_dir(sessions) {
                    for entry in entries.flatten() {
                        let heartbeat = entry.path().join(heartbeat_name);
                        if std::fs::metadata(&heartbeat).is_ok_and(|metadata| metadata.len() > 0) {
                            return heartbeat;
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let heartbeat = started.unwrap_or_else(|_| {
        let store = SessionStore::for_project(root).expect("profile session store");
        let sessions = store
            .list()
            .into_iter()
            .filter_map(|id| store.load(&id))
            .collect::<Vec<_>>();
        panic!(
            "portable terminal fixture did not start; requested_heartbeat={requested_heartbeat:?}; command={command:?}; sessions={sessions:#?}"
        );
    });
    wait_for_audit_attempt(root, heartbeat_name).await;
    heartbeat
}

async fn read_response(stdout: &mut BufReader<tokio::process::ChildStdout>) -> serde_json::Value {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(10), stdout.read_line(&mut line))
        .await
        .expect("MCP response timeout")
        .expect("read MCP response");
    assert!(!line.is_empty(), "MCP server closed stdout");
    serde_json::from_str(&line).expect("MCP response JSON")
}

#[cfg(target_os = "linux")]
fn process_details(pid: i32) -> Option<(char, i32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    let mut fields = fields.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let parent = fields.next()?.parse().ok()?;
    Some((state, parent))
}

#[cfg(all(target_os = "linux", debug_assertions))]
fn test_session_lock_stripe(id: &str) -> usize {
    let hash = id
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    (hash as usize) % 64
}

#[cfg(all(target_os = "linux", debug_assertions))]
fn one_id_per_session_lock_stripe() -> Vec<String> {
    let mut ids = vec![None; 64];
    for candidate in 0_u64..100_000 {
        let id = format!("mcp-lock-stripe-{candidate}");
        let stripe = test_session_lock_stripe(&id);
        ids[stripe].get_or_insert(id);
        if ids.iter().all(Option::is_some) {
            break;
        }
    }
    ids.into_iter()
        .enumerate()
        .map(|(stripe, id)| id.unwrap_or_else(|| panic!("missing session lock stripe {stripe}")))
        .collect()
}

#[cfg(all(target_os = "linux", debug_assertions))]
fn hold_session_lock_stripes(
    store: &SessionStore,
    ids: &[String],
    index: usize,
    held_tx: &std::sync::mpsc::Sender<()>,
    release_rx: &std::sync::mpsc::Receiver<()>,
) -> Result<(), nib::session::SessionError> {
    if index == ids.len() {
        held_tx.send(()).expect("signal all session stripes held");
        release_rx
            .recv_timeout(Duration::from_secs(15))
            .expect("release all session stripes");
        return Ok(());
    }
    store.with_session_lock_for_testing(&ids[index], || {
        hold_session_lock_stripes(store, ids, index + 1, held_tx, release_rx)
    })
}

#[cfg(all(target_os = "linux", debug_assertions))]
fn start_skill_usage_lock_holder(
    project_root: &Path,
) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let store = SessionStore::for_project(project_root).expect("profile session store");
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        store
            .with_skill_usage_lock_for_testing(|| {
                held_tx.send(()).expect("signal skill usage lock held");
                release_rx
                    .recv_timeout(Duration::from_secs(15))
                    .expect("release skill usage lock");
                Ok(())
            })
            .expect("hold skill usage lock");
    });
    held_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("skill usage lock is held");
    (release_tx, holder)
}

#[cfg(target_os = "linux")]
fn process_group(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    fields.split_whitespace().nth(2)?.parse().ok()
}

#[cfg(target_os = "linux")]
async fn wait_for_relay_child(server_pid: i32) -> i32 {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let entries = std::fs::read_dir("/proc").expect("Linux procfs");
            for entry in entries.flatten() {
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|value| value.parse::<i32>().ok())
                else {
                    continue;
                };
                if process_details(pid).map(|(_, parent)| parent) != Some(server_pid) {
                    continue;
                }
                let command = std::fs::read(format!("/proc/{pid}/cmdline"))
                    .map(|bytes| String::from_utf8_lossy(&bytes).replace('\0', " "))
                    .unwrap_or_default();
                if command.contains("mcp-stdio-relay") {
                    return pid;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("MCP stdout relay child")
}

#[cfg(target_os = "linux")]
fn active_process_tree(token: &str) -> Vec<i32> {
    let mut processes = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Some((state, parent)) = process_details(pid) else {
            continue;
        };
        let command = std::fs::read(entry.path().join("cmdline"))
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        processes.push((pid, parent, state, command));
    }

    let mut selected = processes
        .iter()
        .filter(|(_, _, state, command)| *state != 'Z' && command.contains(token))
        .map(|(pid, _, _, _)| *pid)
        .collect::<HashSet<_>>();
    loop {
        let descendants = processes
            .iter()
            .filter(|(pid, parent, state, _)| {
                *state != 'Z' && !selected.contains(pid) && selected.contains(parent)
            })
            .map(|(pid, _, _, _)| *pid)
            .collect::<Vec<_>>();
        if descendants.is_empty() {
            break;
        }
        selected.extend(descendants);
    }
    let mut selected = selected.into_iter().collect::<Vec<_>>();
    selected.sort_unstable();
    selected
}

#[cfg(target_os = "linux")]
async fn wait_for_process_tree(token: &str) -> Vec<i32> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut previous = Vec::new();
        loop {
            let processes = active_process_tree(token);
            if processes.len() >= 2 && processes == previous {
                return processes;
            }
            previous = processes;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("long-running terminal process tree")
}

async fn wait_for_audit_attempt(root: &Path, token: &str) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let store = SessionStore::for_project(root).expect("profile session store");
            let audited = store.list().into_iter().find(|id| {
                store.load(id).is_some_and(|session| {
                    session.events.iter().any(|event| {
                        event.kind == "tool_attempted"
                            && event.details["tool_name"] == "run_terminal"
                            && event.details["arguments"]["command"]
                                .as_str()
                                .is_some_and(|command| command.contains(token))
                    })
                })
            });
            if let Some(id) = audited {
                return id;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("gated MCP audit attempt")
}

async fn wait_for_cancellation_audit(root: &Path, token: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let store = SessionStore::for_project(root).expect("profile session store");
            let reconciled = store.list().into_iter().any(|id| {
                store.load(&id).is_some_and(|session| {
                    let attempted = session.events.iter().any(|event| {
                        event.kind == "tool_attempted"
                            && event.details["arguments"]["command"]
                                .as_str()
                                .is_some_and(|command| command.contains(token))
                    });
                    let cancelled = session.events.iter().any(|event| {
                        event.kind == "mcp_request_cancelled"
                            && event.details["tool_name"] == "run_terminal"
                            && event.details["reconciled"] == true
                    });
                    attempted && cancelled
                })
            });
            if reconciled {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cancelled MCP audit reconciliation");
}

async fn wait_for_tool_completion(root: &Path, tool_name: &str) {
    let completed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let store = SessionStore::for_project(root).expect("profile session store");
            let completed = store.list().into_iter().any(|id| {
                store.load(&id).is_some_and(|session| {
                    session.tool_calls.iter().any(|call| {
                        call.tool_name.as_deref() == Some(tool_name)
                            && call
                                .result
                                .as_ref()
                                .is_some_and(|result| result["success"] == true)
                    })
                })
            });
            if completed {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    if completed.is_err() {
        let store = SessionStore::for_project(root).expect("profile session store");
        let matching = store
            .list()
            .into_iter()
            .filter_map(|id| store.load(&id))
            .flat_map(|session| session.tool_calls)
            .filter(|call| call.tool_name.as_deref() == Some(tool_name))
            .collect::<Vec<_>>();
        panic!("MCP tool completion audit timed out; matching records: {matching:?}");
    }
}

#[cfg(target_os = "linux")]
async fn wait_for_nib_run_cancellation_audit(root: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let store = SessionStore::for_project(root).expect("profile session store");
            let reconciled = store.list().into_iter().any(|id| {
                store.load(&id).is_some_and(|session| {
                    let tool_audited = session
                        .tool_calls
                        .iter()
                        .any(|call| call.tool_name.as_deref() == Some("spawn_subagent"));
                    let cancellation_audited = session.events.iter().any(|event| {
                        event.kind == "mcp_request_cancelled"
                            && event.details["tool_name"] == "nib_run"
                            && event.details["outcome"] == "cancelled"
                            && event.details["reconciled"] == true
                    });
                    tool_audited && cancellation_audited
                })
            });
            if reconciled {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("nib_run tool and cancellation audit");
}

#[cfg(target_os = "linux")]
async fn assert_process_tree_stops(processes: &[i32]) {
    let stopped = tokio::time::timeout(Duration::from_secs(5), async {
        while processes
            .iter()
            .any(|pid| process_details(*pid).is_some_and(|(state, _)| state != 'Z'))
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    let survivors = if stopped {
        Vec::new()
    } else {
        processes
            .iter()
            .filter_map(|pid| {
                let (state, _) = process_details(*pid)?;
                (state != 'Z').then(|| {
                    let command = std::fs::read(format!("/proc/{pid}/cmdline"))
                        .map(|bytes| String::from_utf8_lossy(&bytes).replace('\0', " "))
                        .unwrap_or_default();
                    format!("pid={pid} state={state} command={command}")
                })
            })
            .collect::<Vec<_>>()
    };
    if !stopped {
        for pid in processes.iter().rev() {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
    }
    assert!(
        stopped,
        "terminal process tree survived MCP cancellation: {survivors:?}"
    );
}

#[tokio::test]
async fn test_mcp_mock_server() {
    let project = tempfile::tempdir().expect("temporary MCP project");
    let model_context_secret = "model-context-\"credential\\line\nnext";
    let model_context_secret_json =
        serde_json::to_string(model_context_secret).expect("model context secret JSON");
    let model_context_secret_escaped =
        &model_context_secret_json[1..model_context_secret_json.len() - 1];
    let mut servers = HashMap::new();
    servers.insert(
        "nib_server".to_string(),
        McpServerEntry {
            command: env!("CARGO_BIN_EXE_nib").to_string(),
            args: vec!["mcp-server".to_string()],
            env: HashMap::new(),
            cwd: Some(project.path().to_path_buf()),
            ..McpServerEntry::default()
        },
    );

    let mcp = Arc::new(
        McpManager::new(&servers, &[model_context_secret.to_string()])
            .await
            .expect("Failed to initialize McpManager"),
    );

    let executor = ToolExecutor::new(project.path().to_path_buf(), ExecutionConfig::default())
        .with_mcp_manager(Arc::clone(&mcp));
    let model_tools = executor.get_tools_schema().await;
    let serialized_model_tools =
        serde_json::to_string(&model_tools).expect("serialize model tool context");
    assert!(model_tools
        .iter()
        .any(|tool| { tool["function"]["name"] == "nib_server::nib_get_status" }));
    for spelling in [
        model_context_secret,
        model_context_secret_json.as_str(),
        model_context_secret_escaped,
    ] {
        assert!(
            !serialized_model_tools.contains(spelling),
            "configured secret entered model tool context"
        );
    }

    // 1. Test tools/list
    let tools = mcp.list_tools().await.expect("Failed to list tools");
    assert!(tools.len() >= 2);
    let status_tool = tools
        .iter()
        .find(|tool| tool["name"] == "nib_server::nib_get_status")
        .expect("status tool advertised");
    assert_eq!(status_tool["parameters"]["required"][0], "session_id");
    assert!(status_tool.get("inputSchema").is_none());

    // 2. Test tools/call
    let res = mcp
        .call_tool(
            "nib_server::nib_get_status",
            json!({"session_id": "missing"}),
        )
        .await
        .expect("Failed to call tool");

    let text = res["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("not_found"));

    let error = mcp
        .call_tool("nib_server::not_advertised", json!({}))
        .await
        .expect_err("unknown tools must be rejected before invocation");
    assert!(error.to_string().contains("not advertised"));
}

#[tokio::test]
async fn outbound_manager_drop_is_bounded_on_every_platform() {
    let project = tempfile::tempdir().expect("temporary MCP lifecycle project");
    let servers = HashMap::from([(
        "nib_server".to_string(),
        McpServerEntry {
            command: env!("CARGO_BIN_EXE_nib").to_string(),
            args: vec!["mcp-server".to_string()],
            cwd: Some(project.path().to_path_buf()),
            ..McpServerEntry::default()
        },
    )]);
    let manager = McpManager::new(&servers, &[])
        .await
        .expect("outbound manager starts");
    manager
        .list_tools()
        .await
        .expect("outbound server is healthy");

    let started = std::time::Instant::now();
    drop(manager);

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "outbound manager drop did not synchronously join its transport"
    );
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn outbound_manager_drop_reaps_descendants_on_every_platform() {
    let directory = tempfile::tempdir().expect("MCP manager-drop lifecycle fixture");
    let heartbeat = directory.path().join("manager-drop-descendant.heartbeat");
    let servers = HashMap::from([(
        "fixture".to_string(),
        lifecycle_fixture_entry("healthy", &heartbeat),
    )]);
    let manager = McpManager::new(&servers, &[])
        .await
        .expect("healthy lifecycle fixture initializes");
    wait_for_heartbeat(&heartbeat).await;

    let started = std::time::Instant::now();
    drop(manager);

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "outbound manager drop did not synchronously join its transport"
    );
    assert_heartbeat_stops(&heartbeat).await;
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn outbound_initialization_timeout_reaps_descendants_on_every_platform() {
    let directory = tempfile::tempdir().expect("MCP initialization-timeout lifecycle fixture");
    let heartbeat = directory
        .path()
        .join("initialization-timeout-descendant.heartbeat");
    let mut entry = lifecycle_fixture_entry("initialization_stall", &heartbeat);
    entry.request_timeout_secs = 1;
    let servers = HashMap::from([("fixture".to_string(), entry)]);

    let error = match McpManager::new(&servers, &[]).await {
        Ok(_) => panic!("stalled initialization must time out"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("timed out"), "{error}");
    assert!(
        wait_for_heartbeat(&heartbeat).await > 0,
        "fixture descendant never ran"
    );
    assert_heartbeat_stops(&heartbeat).await;
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn cancelling_outbound_startup_reaps_descendants_on_every_platform() {
    let directory = tempfile::tempdir().expect("MCP startup-cancellation lifecycle fixture");
    let heartbeat = directory
        .path()
        .join("startup-cancellation-descendant.heartbeat");
    let servers = HashMap::from([(
        "fixture".to_string(),
        lifecycle_fixture_entry("initialization_stall", &heartbeat),
    )]);
    let startup = tokio::spawn(async move { McpManager::new(&servers, &[]).await });
    wait_for_heartbeat(&heartbeat).await;

    startup.abort();
    let cancellation = match startup.await {
        Err(error) => error,
        Ok(_) => panic!("outbound startup completed instead of being cancelled"),
    };
    assert!(
        cancellation.is_cancelled(),
        "outbound startup failed instead of being cancelled"
    );
    assert_heartbeat_stops(&heartbeat).await;
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn initialization_failure_reaps_server_descendants_on_every_platform() {
    let directory = tempfile::tempdir().expect("MCP initialization lifecycle fixture");
    let heartbeat = directory.path().join("initialization-descendant.heartbeat");
    let servers = HashMap::from([(
        "fixture".to_string(),
        lifecycle_fixture_entry("initialization_failure", &heartbeat),
    )]);

    let error = match McpManager::new(&servers, &[]).await {
        Ok(_) => panic!("fixture initialization must fail"),
        Err(error) => error,
    };

    assert!(
        error.to_string().contains("initialization failed"),
        "unexpected initialization failure: {error}"
    );
    assert!(
        wait_for_heartbeat(&heartbeat).await > 0,
        "fixture descendant never ran"
    );
    assert_heartbeat_stops(&heartbeat).await;
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fatal_transport_drains_queued_requests_and_reaps_descendants_on_every_platform() {
    const REQUESTS: usize = 32;

    let directory = tempfile::tempdir().expect("MCP fatal transport lifecycle fixture");
    let heartbeat = directory
        .path()
        .join("fatal-transport-descendant.heartbeat");
    let servers = HashMap::from([(
        "fixture".to_string(),
        lifecycle_fixture_entry("fatal_transport", &heartbeat),
    )]);
    let manager = Arc::new(
        McpManager::new(&servers, &[])
            .await
            .expect("fatal transport fixture initializes"),
    );
    wait_for_heartbeat(&heartbeat).await;

    let start = Arc::new(tokio::sync::Barrier::new(REQUESTS + 1));
    let payload = "x".repeat(768 * 1024);
    let mut calls = Vec::with_capacity(REQUESTS);
    for _ in 0..REQUESTS {
        let manager = Arc::clone(&manager);
        let start = Arc::clone(&start);
        let payload = payload.clone();
        calls.push(tokio::spawn(async move {
            start.wait().await;
            manager
                .call_tool("fixture::stall", json!({"payload": payload}))
                .await
        }));
    }
    start.wait().await;

    let results = tokio::time::timeout(Duration::from_secs(6), async {
        let mut results = Vec::with_capacity(REQUESTS);
        for call in calls {
            results.push(call.await.expect("MCP call task"));
        }
        results
    })
    .await
    .expect("fatal transport drains every queued request");
    assert_eq!(results.len(), REQUESTS);
    assert!(
        results.iter().all(Result::is_err),
        "fatal transport unexpectedly completed a queued request"
    );
    assert!(
        manager.list_tools().await.is_err(),
        "fatal transport accepted a late request"
    );

    drop(manager);
    assert_heartbeat_stops(&heartbeat).await;
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn root_exit_with_inherited_stdio_terminates_transport_and_descendant_on_every_platform() {
    let directory = tempfile::tempdir().expect("MCP root-exit lifecycle fixture");
    let heartbeat = directory.path().join("root-exit-descendant.heartbeat");
    let trigger = heartbeat.with_extension("root-exit");
    let servers = HashMap::from([(
        "fixture".to_string(),
        lifecycle_fixture_entry("root_exit_inherited_stdio", &heartbeat),
    )]);
    let manager = McpManager::new(&servers, &[])
        .await
        .expect("root-exit fixture initializes");
    wait_for_heartbeat(&heartbeat).await;

    std::fs::write(&trigger, b"exit").expect("trigger MCP root exit");
    let error = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Err(error) = manager.list_tools().await {
                break error;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("direct MCP server exit becomes terminal despite inherited stdio");
    assert!(
        error.to_string().contains("MCP server process exited"),
        "unexpected root-exit failure: {error}"
    );
    assert_heartbeat_stops(&heartbeat).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn client_startup_never_inherits_configured_secret_from_server_stderr() {
    let project = tempfile::tempdir().expect("temporary MCP project");
    initialize_server_fixture(project.path());
    let secret = "doctor-mcp-stderr-secret";
    let mut config = load_nib_config_full(project.path()).expect("current server config");
    config.mcp.servers.insert(
        "stderr_fixture".to_string(),
        McpServerEntry {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "printf '%s\\n' \"$MCP_API_TOKEN\" >&2; exec \"$NIB_FIXTURE_BIN\" mcp-server"
                    .to_string(),
            ],
            env: HashMap::from([
                ("MCP_API_TOKEN".to_string(), secret.to_string()),
                (
                    "NIB_FIXTURE_BIN".to_string(),
                    env!("CARGO_BIN_EXE_nib").to_string(),
                ),
            ]),
            cwd: Some(project.path().to_path_buf()),
            ..McpServerEntry::default()
        },
    );
    save_nib_config_full(project.path(), &mut config).expect("doctor MCP config");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_nib"))
        .arg("doctor")
        .current_dir(project.path())
        .output()
        .expect("nib doctor starts");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        combined.contains("Protocol initialize/list OK"),
        "{combined}"
    );
    assert!(
        !combined.contains(secret),
        "raw child stderr leaked: {combined}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancellation_targets_one_request_and_keeps_server_responsive_during_persistence() {
    let project = tempfile::tempdir().expect("temporary MCP project");
    initialize_server_fixture(project.path());
    let (mut server, mut stdin, mut stdout) = spawn_server(project.path());
    let token = format!("nib-mcp-cancel-{}", uuid::Uuid::new_v4());

    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": "long",
            "method": "tools/call",
            "params": {
                "name": "run_terminal",
                "arguments": {"command": format!("sleep 60 & wait # {token}")}
            }
        }),
    )
    .await;
    let processes = wait_for_process_tree(&token).await;
    let session_id = wait_for_audit_attempt(project.path(), &token).await;

    let lock_store = SessionStore::for_project(project.path()).expect("profile session store");
    let (lock_held_tx, lock_held_rx) = std::sync::mpsc::channel();
    let (release_lock_tx, release_lock_rx) = std::sync::mpsc::channel();
    let lock_thread = std::thread::spawn(move || {
        lock_store
            .update_session(&session_id, |_session| {
                lock_held_tx.send(()).expect("signal held session lock");
                release_lock_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("release held session lock");
                Ok(())
            })
            .expect("lock-holder session update");
    });
    lock_held_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("session persistence lock is held");

    write_request(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": "independent", "method": "ping"}),
    )
    .await;
    let independent = read_response(&mut stdout).await;
    assert_eq!(independent["id"], "independent", "{independent}");
    assert!(independent.get("result").is_some(), "{independent}");

    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": "long", "reason": "test cancellation"}
        }),
    )
    .await;

    write_request(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": "during-cancel", "method": "ping"}),
    )
    .await;
    let during_cancel = tokio::time::timeout(Duration::from_secs(2), read_response(&mut stdout))
        .await
        .expect("coordinator remains responsive while cancellation persistence waits");
    assert_eq!(during_cancel["id"], "during-cancel", "{during_cancel}");
    assert_process_tree_stops(&processes).await;

    release_lock_tx.send(()).expect("release session lock");
    lock_thread.join().expect("session lock holder");
    let cancelled = read_response(&mut stdout).await;
    assert_eq!(cancelled["id"], "long", "{cancelled}");
    assert_eq!(cancelled["error"]["code"], -32800, "{cancelled}");
    wait_for_cancellation_audit(project.path(), &token).await;

    write_request(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": "after", "method": "ping"}),
    )
    .await;
    let after = read_response(&mut stdout).await;
    assert_eq!(after["id"], "after", "{after}");

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(5), server.wait())
        .await
        .expect("MCP server exits after EOF")
        .expect("MCP server wait");
    assert!(status.success(), "MCP server failed after cancellation");
}

#[cfg(all(target_os = "linux", debug_assertions))]
#[tokio::test]
async fn preinitialization_session_lock_stall_is_responsive_and_bounded() {
    let project = tempfile::tempdir().expect("temporary MCP project");
    initialize_server_fixture(project.path());
    let lock_store = SessionStore::for_project(project.path()).expect("profile session store");
    let stripe_ids = one_id_per_session_lock_stripe();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let lock_thread = std::thread::spawn(move || {
        hold_session_lock_stripes(&lock_store, &stripe_ids, 0, &held_tx, &release_rx)
            .expect("hold every session lock stripe");
    });
    held_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("all session lock stripes are held");

    let (mut server, mut stdin, mut stdout) = spawn_server(project.path());
    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": "blocked-init",
            "method": "tools/call",
            "params": {
                "name": "read_file",
                "arguments": {"path": "README.md"}
            }
        }),
    )
    .await;
    write_request(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": "init-ping", "method": "ping"}),
    )
    .await;
    let ping = tokio::time::timeout(Duration::from_secs(2), read_response(&mut stdout))
        .await
        .expect("single-worker coordinator responds while audit session creation waits");
    assert_eq!(ping["id"], "init-ping", "{ping}");

    let shutdown_started = std::time::Instant::now();
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(8), server.wait())
        .await
        .expect("MCP server bounds preinitialization lock waits")
        .expect("MCP server wait");
    assert!(
        !status.success(),
        "MCP server must fail closed when the reserved audit session cannot be persisted"
    );
    assert!(shutdown_started.elapsed() < Duration::from_secs(8));

    release_tx.send(()).expect("release session lock stripes");
    lock_thread.join().expect("session stripe lock holder");
}

#[cfg(all(target_os = "linux", debug_assertions))]
#[tokio::test]
async fn attempted_audit_lock_stall_is_responsive_and_bounded() {
    let project = tempfile::tempdir().expect("temporary MCP project");
    initialize_server_fixture(project.path());
    let (release_lock_tx, lock_thread) = start_skill_usage_lock_holder(project.path());
    let (mut server, mut stdin, mut stdout) = spawn_server(project.path());

    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": "blocked-attempt",
            "method": "tools/call",
            "params": {
                "name": "read_file",
                "arguments": {"path": "README.md"}
            }
        }),
    )
    .await;
    write_request(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": "attempt-ping", "method": "ping"}),
    )
    .await;
    let ping = tokio::time::timeout(Duration::from_secs(2), read_response(&mut stdout))
        .await
        .expect("single-worker coordinator responds while attempted audit waits");
    assert_eq!(ping["id"], "attempt-ping", "{ping}");

    let shutdown_started = std::time::Instant::now();
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(8), server.wait())
        .await
        .expect("MCP server bounds attempted audit lock waits")
        .expect("MCP server wait");
    assert!(
        !status.success(),
        "MCP server must fail closed when attempted and cancellation audits cannot persist"
    );
    assert!(shutdown_started.elapsed() < Duration::from_secs(8));

    release_lock_tx.send(()).expect("release skill usage lock");
    lock_thread.join().expect("skill usage lock holder");
}

#[cfg(all(target_os = "linux", debug_assertions))]
#[tokio::test]
async fn final_audit_lock_stall_is_responsive_and_bounded() {
    let project = tempfile::tempdir().expect("temporary MCP project");
    initialize_server_fixture(project.path());
    let (mut server, mut stdin, mut stdout) = spawn_server(project.path());
    let token = format!("nib-mcp-final-audit-{}", uuid::Uuid::new_v4());
    let marker = project.path().join("release-terminal");

    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": "blocked-final",
            "method": "tools/call",
            "params": {
                "name": "run_terminal",
                "arguments": {
                    "command": format!(
                        "while [ ! -f '{}' ]; do sleep 1; done # {token}",
                        marker.display()
                    )
                }
            }
        }),
    )
    .await;
    let processes = wait_for_process_tree(&token).await;
    wait_for_audit_attempt(project.path(), &token).await;
    let (release_lock_tx, lock_thread) = start_skill_usage_lock_holder(project.path());
    std::fs::write(&marker, b"release\n").expect("release terminal command");
    assert_process_tree_stops(&processes).await;

    write_request(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": "final-ping", "method": "ping"}),
    )
    .await;
    let ping = tokio::time::timeout(Duration::from_secs(2), read_response(&mut stdout))
        .await
        .expect("single-worker coordinator responds while final audit waits");
    assert_eq!(ping["id"], "final-ping", "{ping}");

    let shutdown_started = std::time::Instant::now();
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(8), server.wait())
        .await
        .expect("MCP server bounds final audit lock waits")
        .expect("MCP server wait");
    assert!(
        !status.success(),
        "MCP server must fail closed when final and cancellation audits cannot persist"
    );
    assert!(shutdown_started.elapsed() < Duration::from_secs(8));

    release_lock_tx.send(()).expect("release skill usage lock");
    lock_thread.join().expect("skill usage lock holder");
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn targeted_cancellation_reaps_terminal_descendants_on_every_platform() {
    let project = tempfile::tempdir().expect("portable MCP cancellation fixture");
    initialize_server_fixture(project.path());
    let fixture = install_terminal_tree_fixture(project.path());
    let (mut server, mut stdin, mut stdout) = spawn_server(project.path());
    let heartbeat = start_portable_terminal_tree(
        project.path(),
        &mut stdin,
        "portable-cancel",
        &fixture,
        "cancelled-terminal.heartbeat",
    )
    .await;

    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": "portable-cancel", "reason": "portable lifecycle test"}
        }),
    )
    .await;
    let cancelled = read_response(&mut stdout).await;
    assert_eq!(cancelled["id"], "portable-cancel", "{cancelled}");
    assert_eq!(cancelled["error"]["code"], -32800, "{cancelled}");
    assert_heartbeat_stops(&heartbeat).await;
    wait_for_cancellation_audit(project.path(), "cancelled-terminal.heartbeat").await;

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(5), server.wait())
        .await
        .expect("portable cancellation server exit")
        .expect("portable cancellation server wait");
    assert!(status.success(), "MCP server failed after cancellation");
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn stdin_disconnect_reaps_terminal_descendants_on_every_platform() {
    let project = tempfile::tempdir().expect("portable MCP disconnect fixture");
    initialize_server_fixture(project.path());
    let fixture = install_terminal_tree_fixture(project.path());
    let (mut server, mut stdin, _stdout) = spawn_server(project.path());
    let heartbeat = start_portable_terminal_tree(
        project.path(),
        &mut stdin,
        "portable-disconnect",
        &fixture,
        "disconnected-terminal.heartbeat",
    )
    .await;

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(5), server.wait())
        .await
        .expect("portable disconnect server exit")
        .expect("portable disconnect server wait");
    assert!(status.success(), "MCP server failed after disconnect");
    assert_heartbeat_stops(&heartbeat).await;
    wait_for_cancellation_audit(project.path(), "disconnected-terminal.heartbeat").await;
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn fatal_input_reaps_terminal_descendants_on_every_platform() {
    let project = tempfile::tempdir().expect("portable MCP fatal-input fixture");
    initialize_server_fixture(project.path());
    let fixture = install_terminal_tree_fixture(project.path());
    let (mut server, mut stdin, _stdout) = spawn_server(project.path());
    let heartbeat = start_portable_terminal_tree(
        project.path(),
        &mut stdin,
        "portable-fatal",
        &fixture,
        "fatal-input-terminal.heartbeat",
    )
    .await;

    let oversized = vec![b'x'; 1024 * 1024 + 1];
    let _ = tokio::time::timeout(Duration::from_secs(5), stdin.write_all(&oversized))
        .await
        .expect("portable fatal frame write is bounded");
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(5), server.wait())
        .await
        .expect("portable fatal-input server exit")
        .expect("portable fatal-input server wait");
    assert!(
        !status.success(),
        "fatal input unexpectedly returned success"
    );
    assert_heartbeat_stops(&heartbeat).await;
    wait_for_cancellation_audit(project.path(), "fatal-input-terminal.heartbeat").await;
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn blocked_stdout_disconnect_reaps_terminal_descendants_on_every_platform() {
    let project = tempfile::tempdir().expect("portable MCP backpressure fixture");
    initialize_server_fixture(project.path());
    let fixture = install_terminal_tree_fixture(project.path());
    std::fs::write(project.path().join("large.txt"), vec![b'x'; 220_000])
        .expect("large MCP response fixture");
    let (mut server, mut stdin, _unread_stdout) = spawn_server(project.path());
    let heartbeat = start_portable_terminal_tree(
        project.path(),
        &mut stdin,
        "portable-backpressure",
        &fixture,
        "backpressured-terminal.heartbeat",
    )
    .await;
    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": "portable-large",
            "method": "tools/call",
            "params": {
                "name": "read_file",
                "arguments": {
                    "path": "large.txt",
                    "max_bytes": 220000,
                    "max_lines": 10000
                }
            }
        }),
    )
    .await;
    wait_for_tool_completion(project.path(), "read_file").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(5), server.wait())
        .await
        .expect("portable backpressure server exit")
        .expect("portable backpressure server wait");
    assert!(
        status.success(),
        "MCP server failed under stdout backpressure"
    );
    assert_heartbeat_stops(&heartbeat).await;
    wait_for_cancellation_audit(project.path(), "backpressured-terminal.heartbeat").await;
}

#[cfg(all(debug_assertions, target_os = "linux"))]
#[tokio::test]
async fn linux_nib_run_cancellation_reaps_git_descendants() {
    let project = tempfile::tempdir().expect("portable nib_run cancellation fixture");
    initialize_server_fixture(project.path());
    let heartbeat = project.path().join("cancelled-nib-run-git.heartbeat");
    let path = install_git_stall_fixture(project.path(), &heartbeat);
    let (mut server, mut stdin, mut stdout) =
        spawn_server_with_path(project.path(), Some(path.as_os_str()));

    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": "portable-nib-run",
            "method": "tools/call",
            "params": {
                "name": "nib_run",
                "arguments": {"goal": "remain cancelled", "max_steps": 100}
            }
        }),
    )
    .await;
    wait_for_heartbeat(&heartbeat).await;
    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": "portable-nib-run"}
        }),
    )
    .await;

    let cancelled = read_response(&mut stdout).await;
    assert_eq!(cancelled["id"], "portable-nib-run", "{cancelled}");
    assert_eq!(cancelled["error"]["code"], -32800, "{cancelled}");
    assert_heartbeat_stops(&heartbeat).await;
    assert!(
        nib::tools::delegation::list_subagents(project.path())
            .expect("reconciled subagent records")
            .is_empty(),
        "precommit cancellation persisted a subagent record"
    );
    wait_for_nib_run_cancellation_audit(project.path()).await;

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(5), server.wait())
        .await
        .expect("portable nib_run cancellation server exit")
        .expect("portable nib_run cancellation server wait");
    assert!(
        status.success(),
        "MCP server failed after nib_run cancellation"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn stdin_disconnect_joins_active_work_and_kills_terminal_descendants() {
    let project = tempfile::tempdir().expect("temporary MCP project");
    initialize_server_fixture(project.path());
    let (mut server, mut stdin, _stdout) = spawn_server(project.path());
    let token = format!("nib-mcp-disconnect-{}", uuid::Uuid::new_v4());

    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": "disconnect",
            "method": "tools/call",
            "params": {
                "name": "run_terminal",
                "arguments": {"command": format!("sleep 60 & wait # {token}")}
            }
        }),
    )
    .await;
    let processes = wait_for_process_tree(&token).await;
    wait_for_audit_attempt(project.path(), &token).await;

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(5), server.wait())
        .await
        .expect("MCP server exits after disconnect")
        .expect("MCP server wait");
    assert!(status.success(), "MCP server failed after disconnect");
    assert_process_tree_stops(&processes).await;
    wait_for_cancellation_audit(project.path(), &token).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn stdin_disconnect_stops_every_request_and_bounds_stuck_audit_persistence() {
    let project = tempfile::tempdir().expect("temporary MCP project");
    initialize_server_fixture(project.path());
    let (mut server, mut stdin, _stdout) = spawn_server(project.path());
    let first_token = format!("nib-mcp-disconnect-first-{}", uuid::Uuid::new_v4());
    let second_token = format!("nib-mcp-disconnect-second-{}", uuid::Uuid::new_v4());

    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": "first",
            "method": "tools/call",
            "params": {
                "name": "run_terminal",
                "arguments": {"command": format!("sleep 60 & wait # {first_token}")}
            }
        }),
    )
    .await;
    let first_processes = wait_for_process_tree(&first_token).await;
    let first_session_id = wait_for_audit_attempt(project.path(), &first_token).await;

    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": "second",
            "method": "tools/call",
            "params": {
                "name": "run_terminal",
                "arguments": {"command": format!("sleep 60 & wait # {second_token}")}
            }
        }),
    )
    .await;
    let second_processes = wait_for_process_tree(&second_token).await;
    wait_for_audit_attempt(project.path(), &second_token).await;

    let lock_store = SessionStore::for_project(project.path()).expect("profile session store");
    let (lock_held_tx, lock_held_rx) = std::sync::mpsc::channel();
    let (release_lock_tx, release_lock_rx) = std::sync::mpsc::channel();
    let lock_thread = std::thread::spawn(move || {
        lock_store
            .update_session(&first_session_id, |_session| {
                lock_held_tx.send(()).expect("signal held session lock");
                release_lock_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("release held session lock");
                Ok(())
            })
            .expect("lock-holder session update");
    });
    lock_held_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("session persistence lock is held");

    drop(stdin);
    assert_process_tree_stops(&first_processes).await;
    assert_process_tree_stops(&second_processes).await;

    let shutdown_started = std::time::Instant::now();
    let status = tokio::time::timeout(Duration::from_secs(8), server.wait())
        .await
        .expect("MCP server bounds stuck cancellation audit persistence")
        .expect("MCP server wait");
    assert!(
        !status.success(),
        "MCP server must fail closed when cancellation audit persistence times out"
    );
    assert!(
        shutdown_started.elapsed() < Duration::from_secs(8),
        "MCP server shutdown exceeded its cancellation audit bound"
    );

    release_lock_tx.send(()).expect("release session lock");
    lock_thread.join().expect("session lock holder");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn fatal_oversized_stdin_frame_joins_active_work_and_kills_terminal_descendants() {
    let project = tempfile::tempdir().expect("temporary MCP project");
    initialize_server_fixture(project.path());
    let (mut server, mut stdin, _stdout) = spawn_server(project.path());
    let token = format!("nib-mcp-fatal-input-{}", uuid::Uuid::new_v4());

    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": "fatal-input",
            "method": "tools/call",
            "params": {
                "name": "run_terminal",
                "arguments": {"command": format!("sleep 60 & wait # {token}")}
            }
        }),
    )
    .await;
    let processes = wait_for_process_tree(&token).await;
    wait_for_audit_attempt(project.path(), &token).await;

    let oversized = vec![b'x'; 1024 * 1024 + 1];
    let _ = tokio::time::timeout(Duration::from_secs(5), stdin.write_all(&oversized))
        .await
        .expect("fatal frame write is bounded");
    drop(stdin);

    let status = tokio::time::timeout(Duration::from_secs(5), server.wait())
        .await
        .expect("MCP server exits after fatal input")
        .expect("MCP server wait");
    assert!(
        !status.success(),
        "fatal input unexpectedly returned success"
    );
    assert_process_tree_stops(&processes).await;
    wait_for_cancellation_audit(project.path(), &token).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn stdout_backpressure_does_not_block_eof_descendant_cleanup() {
    let project = tempfile::tempdir().expect("temporary MCP project");
    initialize_server_fixture(project.path());
    std::fs::write(project.path().join("large.txt"), vec![b'x'; 220_000])
        .expect("large MCP response fixture");
    let (mut server, mut stdin, _unread_stdout) = spawn_server(project.path());
    let token = format!("nib-mcp-backpressure-{}", uuid::Uuid::new_v4());

    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": "long",
            "method": "tools/call",
            "params": {
                "name": "run_terminal",
                "arguments": {"command": format!("sleep 60 & wait # {token}")}
            }
        }),
    )
    .await;
    let processes = wait_for_process_tree(&token).await;
    wait_for_audit_attempt(project.path(), &token).await;

    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": "large",
            "method": "tools/call",
            "params": {
                "name": "read_file",
                "arguments": {
                    "path": "large.txt",
                    "max_bytes": 220000,
                    "max_lines": 10000
                }
            }
        }),
    )
    .await;
    wait_for_tool_completion(project.path(), "read_file").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(5), server.wait())
        .await
        .expect("MCP server exits despite blocked stdout")
        .expect("MCP server wait");
    assert!(
        status.success(),
        "MCP server failed under stdout backpressure"
    );
    assert_process_tree_stops(&processes).await;
    wait_for_cancellation_audit(project.path(), &token).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn killing_server_process_group_reaps_blocked_stdout_relay() {
    use std::os::unix::process::CommandExt;

    let project = tempfile::tempdir().expect("temporary MCP project");
    initialize_server_fixture(project.path());
    std::fs::write(project.path().join("large.txt"), vec![b'x'; 220_000])
        .expect("large MCP response fixture");
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_nib"));
    command
        .arg("mcp-server")
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command.as_std_mut().process_group(0);
    let mut server = command.spawn().expect("group-isolated MCP server starts");
    let server_pid = server.id().expect("MCP server pid") as i32;
    let mut stdin = server.stdin.take().expect("MCP server stdin");
    let _unread_stdout = server.stdout.take().expect("MCP server stdout");

    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": "large",
            "method": "tools/call",
            "params": {
                "name": "read_file",
                "arguments": {
                    "path": "large.txt",
                    "max_bytes": 220000,
                    "max_lines": 10000
                }
            }
        }),
    )
    .await;
    wait_for_tool_completion(project.path(), "read_file").await;
    let relay_pid = wait_for_relay_child(server_pid).await;
    assert_eq!(
        process_group(relay_pid),
        Some(server_pid),
        "stdout relay escaped the MCP server process group"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;

    // SAFETY: a negative PID targets the isolated process group created above.
    let kill_result = unsafe { libc::kill(-server_pid, libc::SIGKILL) };
    assert_eq!(
        kill_result,
        0,
        "failed to kill MCP server process group: {}",
        std::io::Error::last_os_error()
    );
    let _status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = server.try_wait().expect("MCP server wait") {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("MCP server process reaped");
    let relay_stopped = tokio::time::timeout(Duration::from_secs(5), async {
        while process_details(relay_pid).is_some_and(|(state, _)| state != 'Z') {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    assert!(relay_stopped, "stdout relay survived MCP server tree kill");
}

#[cfg(windows)]
#[tokio::test]
async fn stdout_backpressure_does_not_block_eof_cleanup_on_windows() {
    let project = tempfile::tempdir().expect("temporary MCP project");
    initialize_server_fixture(project.path());
    std::fs::write(project.path().join("large.txt"), vec![b'x'; 220_000])
        .expect("large MCP response fixture");
    let (mut server, mut stdin, _unread_stdout) = spawn_server(project.path());

    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": "large",
            "method": "tools/call",
            "params": {
                "name": "read_file",
                "arguments": {
                    "path": "large.txt",
                    "max_bytes": 220000,
                    "max_lines": 10000
                }
            }
        }),
    )
    .await;
    wait_for_tool_completion(project.path(), "read_file").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(5), server.wait())
        .await
        .expect("MCP server exits despite blocked stdout")
        .expect("MCP server wait");
    assert!(
        status.success(),
        "MCP server failed under stdout backpressure"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn nib_run_precommit_cancellation_kills_git_and_leaves_no_spawn_state() {
    use std::os::unix::fs::PermissionsExt;

    let project = tempfile::tempdir().expect("temporary MCP project");
    initialize_server_fixture(project.path());
    let bin_dir = project.path().join("test-bin");
    std::fs::create_dir(&bin_dir).expect("test bin directory");
    let marker = project.path().join("worktree-add-started");
    let release = project.path().join("release-worktree-add");
    let pid_file = project.path().join("worktree-add-pids");
    let real_git = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("resolve git");
    assert!(real_git.status.success(), "git is required for MCP fixture");
    let real_git = String::from_utf8(real_git.stdout)
        .expect("git path UTF-8")
        .trim()
        .to_string();
    let git_wrapper = bin_dir.join("git");
    std::fs::write(
        &git_wrapper,
        format!(
            concat!(
                "#!/bin/sh\n",
                "case \" $* \" in\n",
                "  *\" worktree add \"*)\n",
                "    sleep 60 & child=$!\n",
                "    printf '%s %s' \"$$\" \"$child\" > '{pid_file}'\n",
                "    : > '{marker}'\n",
                "    while [ ! -e '{release}' ]; do sleep 0.01; done\n",
                "    ;;\n",
                "esac\n",
                "exec '{real_git}' \"$@\"\n"
            ),
            pid_file = pid_file.display(),
            marker = marker.display(),
            release = release.display(),
            real_git = real_git,
        ),
    )
    .expect("git wrapper");
    std::fs::set_permissions(&git_wrapper, std::fs::Permissions::from_mode(0o755))
        .expect("git wrapper mode");
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let mut server = tokio::process::Command::new(env!("CARGO_BIN_EXE_nib"))
        .arg("mcp-server")
        .current_dir(project.path())
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("MCP server starts");
    let mut stdin = server.stdin.take().expect("MCP server stdin");
    let mut stdout = BufReader::new(server.stdout.take().expect("MCP server stdout"));

    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": "nib-run",
            "method": "tools/call",
            "params": {
                "name": "nib_run",
                "arguments": {"goal": "remain cancelled", "max_steps": 100}
            }
        }),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("spawn commit barrier");
    let git_pids = std::fs::read_to_string(&pid_file)
        .expect("git wrapper pids")
        .split_whitespace()
        .map(|pid| pid.parse::<i32>().expect("numeric git wrapper pid"))
        .collect::<Vec<_>>();

    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": "nib-run"}
        }),
    )
    .await;

    write_request(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": "during-nib-run-cancel", "method": "ping"}),
    )
    .await;
    let independent =
        tokio::time::timeout(Duration::from_secs(2), read_response(&mut stdout)).await;
    let independent = independent.expect("independent request remains responsive");
    assert_eq!(independent["id"], "during-nib-run-cancel", "{independent}");

    let cancelled =
        match tokio::time::timeout(Duration::from_secs(5), read_response(&mut stdout)).await {
            Ok(cancelled) => cancelled,
            Err(error) => {
                let _ = std::fs::write(&release, b"test cleanup release");
                panic!("nib_run cancellation did not finish while git remained blocked: {error}");
            }
        };
    assert_eq!(cancelled["id"], "nib-run", "{cancelled}");
    assert_eq!(cancelled["error"]["code"], -32800, "{cancelled}");
    let records = nib::tools::delegation::list_subagents(project.path())
        .expect("reconciled subagent records");
    assert!(
        records.is_empty(),
        "precommit cancellation persisted {records:?}"
    );
    assert_process_tree_stops(&git_pids).await;
    let worktree_root = project.path().join(".nib/worktrees/subagents");
    assert!(
        !worktree_root.exists()
            || std::fs::read_dir(&worktree_root)
                .expect("worktree root")
                .next()
                .is_none(),
        "precommit cancellation left a worktree path"
    );
    let branches = std::process::Command::new("git")
        .current_dir(project.path())
        .args([
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads/nib/subagent/",
        ])
        .output()
        .expect("list subagent branches");
    assert!(branches.status.success());
    assert!(branches.stdout.is_empty(), "precommit branch survived");
    let worktrees = std::process::Command::new("git")
        .current_dir(project.path())
        .args(["worktree", "list", "--porcelain"])
        .output()
        .expect("list worktrees");
    assert!(worktrees.status.success());
    assert!(
        !String::from_utf8_lossy(&worktrees.stdout).contains(".nib/worktrees/subagents"),
        "precommit worktree remained registered"
    );
    wait_for_nib_run_cancellation_audit(project.path()).await;

    write_request(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": "after-nib-run", "method": "ping"}),
    )
    .await;
    let after = read_response(&mut stdout).await;
    assert_eq!(after["id"], "after-nib-run", "{after}");

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(5), server.wait())
        .await
        .expect("MCP server exits after nib_run cancellation")
        .expect("MCP server wait");
    assert!(
        status.success(),
        "MCP server failed after nib_run cancellation"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn nib_run_worktree_add_failure_preserves_unproven_registration() {
    use std::os::unix::fs::PermissionsExt;

    let project = tempfile::tempdir().expect("temporary MCP project");
    initialize_server_fixture(project.path());
    let bin_dir = project.path().join("test-bin");
    std::fs::create_dir(&bin_dir).expect("test bin directory");
    let marker = project.path().join("forced-worktree-add-failure");
    let real_git = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("resolve git");
    assert!(real_git.status.success(), "git is required for MCP fixture");
    let real_git = String::from_utf8(real_git.stdout)
        .expect("git path UTF-8")
        .trim()
        .to_string();
    let git_wrapper = bin_dir.join("git");
    std::fs::write(
        &git_wrapper,
        format!(
            concat!(
                "#!/bin/sh\n",
                "case \" $* \" in\n",
                "  *\" worktree add \"*)\n",
                "    '{real_git}' \"$@\" || exit $?\n",
                "    : > '{marker}'\n",
                "    printf '%s' 'forced failure after successful worktree add' >&2\n",
                "    exit 73\n",
                "    ;;\n",
                "esac\n",
                "exec '{real_git}' \"$@\"\n"
            ),
            marker = marker.display(),
            real_git = real_git,
        ),
    )
    .expect("git wrapper");
    std::fs::set_permissions(&git_wrapper, std::fs::Permissions::from_mode(0o755))
        .expect("git wrapper mode");
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut server = tokio::process::Command::new(env!("CARGO_BIN_EXE_nib"))
        .arg("mcp-server")
        .current_dir(project.path())
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("MCP server starts");
    let mut stdin = server.stdin.take().expect("MCP server stdin");
    let mut stdout = BufReader::new(server.stdout.take().expect("MCP server stdout"));

    write_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": "forced-failure",
            "method": "tools/call",
            "params": {
                "name": "nib_run",
                "arguments": {"goal": "must not start", "max_steps": 1}
            }
        }),
    )
    .await;
    let response = read_response(&mut stdout).await;
    assert_eq!(response["id"], "forced-failure", "{response}");
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert!(marker.exists(), "worktree add failure was not injected");
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("forced failure after successful worktree add")),
        "{response}"
    );
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("registrations were preserved")),
        "{response}"
    );
    let records = nib::tools::delegation::list_subagents(project.path())
        .expect("subagent records after compensation");
    assert!(records.is_empty(), "failed spawn persisted {records:?}");
    let worktree_root = project.path().join(".nib/worktrees/subagents");
    assert!(
        !worktree_root.exists()
            || std::fs::read_dir(&worktree_root)
                .expect("worktree root")
                .next()
                .is_none(),
        "failed spawn left a worktree path"
    );
    let branches = std::process::Command::new("git")
        .current_dir(project.path())
        .args([
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads/nib/subagent/",
        ])
        .output()
        .expect("list subagent branches");
    assert!(branches.status.success());
    assert!(branches.stdout.is_empty(), "failed spawn left a branch");
    let worktrees = std::process::Command::new("git")
        .current_dir(project.path())
        .args(["worktree", "list", "--porcelain"])
        .output()
        .expect("list worktrees");
    assert!(worktrees.status.success());
    assert!(
        String::from_utf8_lossy(&worktrees.stdout).contains(".nib/worktrees/subagents"),
        "unproven post-error registration was deleted"
    );
    let store = SessionStore::for_project(project.path()).expect("profile session store");
    let audit_sessions = store
        .list()
        .into_iter()
        .filter_map(|id| store.load(&id))
        .collect::<Vec<_>>();
    assert!(audit_sessions.iter().any(|session| {
        session
            .tool_calls
            .iter()
            .any(|call| call.tool_name.as_deref() == Some("spawn_subagent"))
    }));
    assert!(!audit_sessions.iter().any(|session| {
        session
            .events
            .iter()
            .any(|event| event.kind == "mcp_request_cancelled")
    }));

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(5), server.wait())
        .await
        .expect("MCP server exits after forced failure")
        .expect("MCP server wait");
    assert!(status.success(), "MCP server failed after compensation");
}
