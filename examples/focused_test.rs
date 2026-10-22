//! Focused test runner exposed exclusively through repository Task gates (T087).
//!
//! `cargo run --quiet --example focused_test -- <cargo test arguments>` runs
//! `cargo test` with those arguments, streams its output unchanged, and fails when
//! the command fails or when its selection executed zero tests.
#[path = "../tests/support/focused_test.rs"]
mod focused;

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, ExitCode, Stdio};

fn main() -> ExitCode {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut child = match Command::new(cargo)
        .arg("test")
        .args(&arguments)
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!("focused test: failed to start cargo test: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut executed = 0;
    let mut stdout = std::io::stdout().lock();
    if let Some(output) = child.stdout.take() {
        // Lossy line reads keep arbitrary test output flowing while the summary
        // lines, which libtest always prints as ASCII, are counted.
        let mut reader = BufReader::new(output);
        let mut line = Vec::new();
        while matches!(reader.read_until(b'\n', &mut line), Ok(read) if read > 0) {
            let _ = stdout.write_all(&line);
            let _ = stdout.flush();
            executed += focused::executed_tests(&String::from_utf8_lossy(&line)).unwrap_or(0);
            line.clear();
        }
    }
    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            eprintln!("focused test: failed to wait for cargo test: {error}");
            return ExitCode::FAILURE;
        }
    };
    match focused::selection_outcome(status.success(), executed) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let selection = arguments
                .iter()
                .map(|argument| argument.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            eprintln!("focused test: {error}: cargo test {selection}");
            status
                .code()
                .and_then(|code| u8::try_from(code).ok())
                .filter(|code| *code != 0)
                .map_or(ExitCode::FAILURE, ExitCode::from)
        }
    }
}
