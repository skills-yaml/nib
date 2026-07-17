use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const ACTIVATION_ENV: &str = "NIB_INTERNAL_MCP_LIFECYCLE_FIXTURE";
const ACTIVATION_TOKEN: &str = "nib-mcp-client-lifecycle-fixture-v1";
const MODE_ENV: &str = "NIB_INTERNAL_MCP_LIFECYCLE_MODE";
const HEARTBEAT_ENV: &str = "NIB_INTERNAL_MCP_LIFECYCLE_HEARTBEAT";
const GIT_WRAPPER_MARKER: &str = ".nib/mcp-git-lifecycle-fixture.json";
const GIT_WRAPPER_TOKEN: &str = "nib-mcp-git-lifecycle-fixture-v1";

pub(crate) fn run_if_requested() -> Option<i32> {
    let result = if std::env::var(ACTIVATION_ENV).ok().as_deref() == Some(ACTIVATION_TOKEN) {
        match std::env::var(MODE_ENV).as_deref() {
            Ok("initialization_failure") => run_server(ServerMode::InitializationFailure),
            Ok("initialization_stall") => run_server(ServerMode::InitializationStall),
            Ok("fatal_transport") => run_server(ServerMode::FatalTransport),
            Ok("healthy") => run_server(ServerMode::Healthy),
            Ok("root_exit_inherited_stdio") => run_server(ServerMode::RootExitInheritedStdio),
            Ok("process_tree") => run_process_tree(),
            Ok("heartbeat") => run_heartbeat(),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid internal MCP lifecycle fixture mode",
            )),
        }
    } else {
        run_git_wrapper_if_requested()?
    };
    match result {
        Ok(()) => Some(0),
        Err(error) => {
            eprintln!("internal MCP lifecycle fixture failed: {error}");
            Some(2)
        }
    }
}

#[derive(Clone, Copy)]
enum ServerMode {
    InitializationFailure,
    InitializationStall,
    FatalTransport,
    Healthy,
    RootExitInheritedStdio,
}

fn run_server(mode: ServerMode) -> io::Result<()> {
    spawn_heartbeat_descendant(matches!(mode, ServerMode::RootExitInheritedStdio))?;

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    let initialize = read_frame(&mut reader)?;
    let initialize_id = initialize
        .get("id")
        .cloned()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "initialize has no id"))?;
    if matches!(mode, ServerMode::InitializationStall) {
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }
    if matches!(mode, ServerMode::InitializationFailure) {
        write_frame(
            &mut writer,
            &json!({
                "jsonrpc": "2.0",
                "id": initialize_id,
                "error": {"code": -32000, "message": "fixture initialization failed"}
            }),
        )?;
        return Ok(());
    }

    write_frame(
        &mut writer,
        &json!({"jsonrpc": "2.0", "id": initialize_id, "result": {}}),
    )?;
    let initialized = read_frame(&mut reader)?;
    if initialized.get("method").and_then(Value::as_str) != Some("notifications/initialized") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected notifications/initialized",
        ));
    }

    let list = read_frame(&mut reader)?;
    let list_id = list
        .get("id")
        .cloned()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "tools/list has no id"))?;
    write_frame(
        &mut writer,
        &json!({
            "jsonrpc": "2.0",
            "id": list_id,
            "result": {
                "tools": [{
                    "name": "stall",
                    "description": "cross-platform lifecycle fixture",
                    "inputSchema": {"type": "object"}
                }]
            }
        }),
    )?;

    if matches!(mode, ServerMode::RootExitInheritedStdio) {
        wait_for_root_exit_trigger()?;
        return Ok(());
    }
    if matches!(mode, ServerMode::Healthy) {
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }

    let call = read_frame(&mut reader)?;
    if call.get("method").and_then(Value::as_str) != Some("tools/call") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected tools/call",
        ));
    }

    // Stop consuming stdin while callers fill the bounded transport queue. Exiting
    // closes stdout and turns the blocked write plus queued frames into one fatal path.
    thread::sleep(Duration::from_secs(1));
    Ok(())
}

fn run_process_tree() -> io::Result<()> {
    spawn_heartbeat_descendant(false)?;
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

fn run_git_wrapper_if_requested() -> Option<io::Result<()>> {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => return Some(Err(error)),
    };
    let is_git = executable
        .file_stem()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("git"));
    if !is_git {
        return None;
    }

    let marker = match std::env::current_dir() {
        Ok(directory) => directory.join(GIT_WRAPPER_MARKER),
        Err(error) => return Some(Err(error)),
    };
    let encoded = match std::fs::read(&marker) {
        Ok(encoded) => encoded,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => return Some(Err(error)),
    };
    let configuration = match serde_json::from_slice::<Value>(&encoded) {
        Ok(configuration) => configuration,
        Err(error) => return Some(Err(io::Error::new(io::ErrorKind::InvalidData, error))),
    };
    if configuration.get("token").and_then(Value::as_str) != Some(GIT_WRAPPER_TOKEN) {
        return Some(Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid internal MCP Git lifecycle fixture token",
        )));
    }
    let Some(heartbeat) = configuration.get("heartbeat").and_then(Value::as_str) else {
        return Some(Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "internal MCP Git lifecycle fixture heartbeat path is missing",
        )));
    };

    Some(run_git_wrapper(heartbeat))
}

fn run_git_wrapper(heartbeat: &str) -> io::Result<()> {
    let mut descendant = Command::new(std::env::current_exe()?);
    descendant
        .env_clear()
        .env(ACTIVATION_ENV, ACTIVATION_TOKEN)
        .env(MODE_ENV, "heartbeat")
        .env(HEARTBEAT_ENV, heartbeat)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    descendant.spawn()?;
    wait_for_heartbeat_path(std::path::Path::new(heartbeat))?;
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

fn spawn_heartbeat_descendant(inherit_stdio: bool) -> io::Result<()> {
    let heartbeat = std::env::var_os(HEARTBEAT_ENV).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "internal MCP lifecycle heartbeat path is missing",
        )
    })?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .env_clear()
        .env(ACTIVATION_ENV, ACTIVATION_TOKEN)
        .env(MODE_ENV, "heartbeat")
        .env(HEARTBEAT_ENV, heartbeat);
    if inherit_stdio {
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
    } else {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    }
    command.spawn()?;
    wait_for_heartbeat_start()
}

fn wait_for_root_exit_trigger() -> io::Result<()> {
    let heartbeat = std::env::var_os(HEARTBEAT_ENV).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "internal MCP lifecycle heartbeat path is missing",
        )
    })?;
    let trigger = std::path::PathBuf::from(heartbeat).with_extension("root-exit");
    for _ in 0..1_000 {
        if trigger.is_file() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "internal MCP lifecycle root-exit trigger did not arrive",
    ))
}

fn wait_for_heartbeat_start() -> io::Result<()> {
    let heartbeat = std::env::var_os(HEARTBEAT_ENV).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "internal MCP lifecycle heartbeat path is missing",
        )
    })?;
    wait_for_heartbeat_path(std::path::Path::new(&heartbeat))
}

fn wait_for_heartbeat_path(heartbeat: &std::path::Path) -> io::Result<()> {
    for _ in 0..200 {
        if std::fs::metadata(heartbeat).is_ok_and(|metadata| metadata.len() > 0) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "internal MCP lifecycle heartbeat did not start",
    ))
}

fn run_heartbeat() -> io::Result<()> {
    let path = std::env::var_os(HEARTBEAT_ENV).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "internal MCP lifecycle heartbeat path is missing",
        )
    })?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    for _ in 0..2_400 {
        file.write_all(b".")?;
        file.flush()?;
        thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}

fn read_frame(reader: &mut impl BufRead) -> io::Result<Value> {
    let mut frame = String::new();
    let bytes = reader.read_line(&mut frame)?;
    if bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "MCP fixture stdin closed",
        ));
    }
    serde_json::from_str(&frame).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn write_frame(writer: &mut impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, value).map_err(io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
}
