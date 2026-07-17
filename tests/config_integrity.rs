use nib::config::{load_nib_config_full, save_nib_config_full, update_nib_config, NibConfig};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CHILD_ROOT: &str = "NIB_CONFIG_CHILD_ROOT";
const CHILD_ID: &str = "NIB_CONFIG_CHILD_ID";
const CHILD_ITERATIONS: &str = "NIB_CONFIG_CHILD_ITERATIONS";
const CHILD_MODE: &str = "NIB_CONFIG_CHILD_MODE";
const CHILD_READY: &str = "NIB_CONFIG_CHILD_READY";
const CHILD_EXPECTATION: &str = "NIB_CONFIG_CHILD_EXPECTATION";

#[test]
fn config_process_worker() {
    let Ok(root) = std::env::var(CHILD_ROOT) else {
        return;
    };
    let root = PathBuf::from(root);
    if std::env::var(CHILD_MODE).as_deref() == Ok("load") {
        let ready = PathBuf::from(std::env::var_os(CHILD_READY).expect("child ready path"));
        std::fs::write(&ready, b"ready").expect("publish child readiness");
        let result = load_nib_config_full(&root);
        match std::env::var(CHILD_EXPECTATION).as_deref() {
            Ok("identity") => {
                let error = result.expect_err("replacement lock must fail closed");
                let message = error.to_string();
                assert!(
                    message.contains("different identities")
                        || message.contains("identity changed"),
                    "{message}"
                );
            }
            Ok("success") => {
                result.expect("config load");
            }
            expectation => panic!("unsupported child expectation: {expectation:?}"),
        }
        return;
    }
    let id = std::env::var(CHILD_ID).expect("child id");
    let iterations = std::env::var(CHILD_ITERATIONS)
        .expect("child iterations")
        .parse::<usize>()
        .expect("numeric child iterations");
    let barrier = root.join("config-process-barrier");
    std::fs::write(barrier.join(format!("ready-{id}")), b"ready").expect("ready marker");
    wait_for_path(&barrier.join("go"), Duration::from_secs(10));

    for iteration in 0..iterations {
        update_nib_config(&root, |config| {
            std::thread::sleep(Duration::from_millis(4));
            config
                .execution
                .boundaries
                .allow_write
                .push(format!("worker-{id}-{iteration}"));
            Ok(())
        })
        .expect("locked child update");
    }
}

#[test]
fn concurrent_process_updates_preserve_every_config_mutation() {
    const CHILDREN: usize = 6;
    const ITERATIONS: usize = 10;

    let root = tempfile::tempdir().expect("temporary project");
    let mut config = NibConfig::default();
    save_nib_config_full(root.path(), &mut config).expect("initial config");
    let barrier = root.path().join("config-process-barrier");
    std::fs::create_dir_all(&barrier).expect("barrier directory");
    let mut children = (0..CHILDREN)
        .map(|id| spawn_config_child(root.path(), id, ITERATIONS))
        .collect::<Vec<_>>();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ready = (0..CHILDREN)
            .filter(|id| barrier.join(format!("ready-{id}")).is_file())
            .count();
        if ready == CHILDREN {
            break;
        }
        assert!(Instant::now() < deadline, "children did not reach barrier");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::write(barrier.join("go"), b"go").expect("release children");

    for child in children.drain(..) {
        let output = child.wait_with_output().expect("wait for config child");
        assert!(
            output.status.success(),
            "config child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let config = load_nib_config_full(root.path()).expect("final config");
    let mut paths = config.execution.boundaries.allow_write;
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), CHILDREN * ITERATIONS);
    for id in 0..CHILDREN {
        for iteration in 0..ITERATIONS {
            assert!(paths.contains(&format!("worker-{id}-{iteration}")));
        }
    }
    assert!(root.path().join(".nib/config.toml.lock").is_file());
}

#[cfg(unix)]
#[test]
fn persistent_config_lock_survives_lock_and_nib_directory_replacement() {
    use std::sync::mpsc;

    let root = tempfile::tempdir().expect("temporary project");
    let mut config = NibConfig::default();
    save_nib_config_full(root.path(), &mut config).expect("initial config");
    let root_path = root.path().to_path_buf();
    let (owner_ready_tx, owner_ready_rx) = mpsc::channel();
    let (owner_release_tx, owner_release_rx) = mpsc::channel();
    let owner = std::thread::spawn(move || {
        update_nib_config(&root_path, |config| {
            owner_ready_tx.send(()).expect("owner readiness");
            owner_release_rx.recv().expect("owner release");
            config.agent.max_turns = 91;
            Ok(())
        })
    });
    owner_ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("owner acquired config lock");

    let ready = root.path().join("intact-contender.ready");
    let mut intact = spawn_load_child(root.path(), &ready, "success");
    assert_child_blocked(&mut intact, &ready);

    let lock_path = root.path().join(".nib/config.toml.lock");
    let displaced_lock = root.path().join(".nib/config.toml.lock.displaced");
    std::fs::rename(&lock_path, &displaced_lock).expect("displace visible config lock");
    std::fs::write(&lock_path, b"replacement").expect("replacement config lock");
    let identity_ready = root.path().join("identity-contender.ready");
    let output = spawn_load_child(root.path(), &identity_ready, "identity")
        .wait_with_output()
        .expect("identity child output");
    assert!(
        output.status.success(),
        "identity child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(identity_ready.exists(), "identity child did not run");
    std::fs::remove_file(&lock_path).expect("remove replacement lock");
    std::fs::rename(&displaced_lock, &lock_path).expect("restore visible config lock");

    let displaced_nib = root.path().join(".nib.displaced");
    std::fs::rename(root.path().join(".nib"), &displaced_nib).expect("displace .nib");
    std::fs::create_dir(root.path().join(".nib")).expect("replacement .nib");
    let directory_ready = root.path().join("directory-contender.ready");
    let mut directory_contender = spawn_load_child(root.path(), &directory_ready, "success");
    assert_child_blocked(&mut directory_contender, &directory_ready);
    std::fs::remove_dir_all(root.path().join(".nib")).expect("remove replacement .nib");
    std::fs::rename(&displaced_nib, root.path().join(".nib")).expect("restore .nib");

    owner_release_tx.send(()).expect("release owner");
    owner
        .join()
        .expect("join config owner")
        .expect("owner update");
    assert_eq!(
        load_nib_config_full(root.path())
            .expect("final config")
            .agent
            .max_turns,
        91
    );
}

fn spawn_config_child(root: &Path, id: usize, iterations: usize) -> Child {
    Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg("config_process_worker")
        .arg("--nocapture")
        .env(CHILD_ROOT, root)
        .env(CHILD_ID, id.to_string())
        .env(CHILD_ITERATIONS, iterations.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn config child")
}

#[cfg(unix)]
fn spawn_load_child(root: &Path, ready: &Path, expectation: &str) -> Child {
    let _ = std::fs::remove_file(ready);
    Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg("config_process_worker")
        .arg("--nocapture")
        .env(CHILD_ROOT, root)
        .env(CHILD_MODE, "load")
        .env(CHILD_READY, ready)
        .env(CHILD_EXPECTATION, expectation)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn config load child")
}

#[cfg(unix)]
fn assert_child_blocked(child: &mut Child, ready: &Path) {
    wait_for_path(ready, Duration::from_secs(5));
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        assert!(
            child.try_wait().expect("inspect config child").is_none(),
            "config contender exited while the anchored lock was held"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().expect("terminate blocked config child");
    child.wait().expect("reap blocked config child");
}

fn wait_for_path(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
