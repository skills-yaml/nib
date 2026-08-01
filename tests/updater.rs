use std::process::Command;

fn nib() -> Command {
    Command::new(env!("CARGO_BIN_EXE_nib"))
}

#[test]
fn update_command_is_documented_in_cli_help() {
    let output = nib()
        .arg("--help")
        .env("NIB_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run nib help");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(stdout.contains("update"));
    assert!(stdout.contains("Update this installed release"));
}

#[test]
fn local_build_update_fails_before_network_or_mutation() {
    let output = nib()
        .arg("update")
        .env("NIB_UPDATE_BASE_URL", "http://127.0.0.1:1/")
        .env("NIB_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run nib update");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 update error");
    assert!(stderr.contains("not self-update managed"));
    assert!(!stderr.contains("release request failed"));
}
