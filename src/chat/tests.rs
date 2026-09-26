use super::*;
use nib::config::{load_nib_config_full, save_nib_config_full, NibConfig};
use serial_test::serial;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, Cursor, Read};
use std::path::PathBuf;
use tempfile::tempdir;

pub(crate) struct ScriptedLineReader {
    pub(crate) lines: VecDeque<(std::time::Duration, String)>,
    pub(crate) current: Vec<u8>,
    pub(crate) offset: usize,
}

#[derive(Clone, Copy)]
pub(crate) enum ModalInputStep {
    Immediate(&'static str),
    Delayed(u64, &'static str),
    AfterModal(u8, &'static str),
    AfterModalCycle(u8, &'static str),
}

pub(crate) struct ModalSynchronizedReader {
    pub(crate) steps: VecDeque<ModalInputStep>,
    pub(crate) modal_state: PlainModalState,
    pub(crate) current: Vec<u8>,
    pub(crate) offset: usize,
}

impl ModalSynchronizedReader {
    pub(crate) fn new(
        modal_state: PlainModalState,
        steps: impl IntoIterator<Item = ModalInputStep>,
    ) -> Self {
        Self {
            steps: steps.into_iter().collect(),
            modal_state,
            current: Vec::new(),
            offset: 0,
        }
    }
}

impl Read for ModalSynchronizedReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let count = available.len().min(output.len());
        output[..count].copy_from_slice(&available[..count]);
        self.consume(count);
        Ok(count)
    }
}

impl BufRead for ModalSynchronizedReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.offset >= self.current.len() {
            let Some(step) = self.steps.pop_front() else {
                return Ok(&[]);
            };
            let line = match step {
                ModalInputStep::Immediate(line) => line,
                ModalInputStep::Delayed(delay_ms, line) => {
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    line
                }
                ModalInputStep::AfterModal(expected, line)
                | ModalInputStep::AfterModalCycle(expected, line) => {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
                    if matches!(step, ModalInputStep::AfterModalCycle(_, _)) {
                        while self.modal_state.current() != PLAIN_MODAL_IDLE {
                            if std::time::Instant::now() >= deadline {
                                return Err(io::Error::new(
                                    io::ErrorKind::TimedOut,
                                    "plain modal did not release input before the test deadline",
                                ));
                            }
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                    }
                    while self.modal_state.current() != expected {
                        if std::time::Instant::now() >= deadline {
                            return Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "plain modal did not claim input before the test deadline",
                            ));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    line
                }
            };
            self.current = line.as_bytes().to_vec();
            self.offset = 0;
        }
        Ok(&self.current[self.offset..])
    }

    fn consume(&mut self, amount: usize) {
        self.offset = self.offset.saturating_add(amount).min(self.current.len());
    }
}

impl ScriptedLineReader {
    pub(crate) fn new(lines: impl IntoIterator<Item = (u64, &'static str)>) -> Self {
        Self {
            lines: lines
                .into_iter()
                .map(|(delay_ms, line)| {
                    (std::time::Duration::from_millis(delay_ms), line.to_string())
                })
                .collect(),
            current: Vec::new(),
            offset: 0,
        }
    }
}

impl Read for ScriptedLineReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let count = available.len().min(output.len());
        output[..count].copy_from_slice(&available[..count]);
        self.consume(count);
        Ok(count)
    }
}

impl BufRead for ScriptedLineReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.offset >= self.current.len() {
            let Some((delay, line)) = self.lines.pop_front() else {
                return Ok(&[]);
            };
            std::thread::sleep(delay);
            self.current = line.into_bytes();
            self.offset = 0;
        }
        Ok(&self.current[self.offset..])
    }

    fn consume(&mut self, amount: usize) {
        self.offset = self.offset.saturating_add(amount).min(self.current.len());
    }
}

pub(crate) struct CurrentDirGuard(pub(crate) PathBuf);

impl CurrentDirGuard {
    pub(crate) fn enter(path: &Path) -> Self {
        let original = std::env::current_dir().expect("current directory");
        std::env::set_current_dir(path).expect("enter project");
        Self(original)
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).expect("restore current directory");
    }
}

pub(crate) fn restore_env(name: &str, value: Option<OsString>) {
    match value {
        Some(value) => std::env::set_var(name, value),
        None => std::env::remove_var(name),
    }
}

pub(crate) struct EnvironmentGuard {
    pub(crate) name: &'static str,
    pub(crate) previous: Option<OsString>,
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
        restore_env(self.name, self.previous.take());
    }
}

pub(crate) fn save_mock_config(project: &Path) {
    let mut config = NibConfig::default();
    config
        .llm
        .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
    config.skills.enabled = false;
    config.daemons.cron_enabled = false;
    config.daemons.curator_enabled = false;
    save_nib_config_full(project, &mut config).expect("mock config");
}

pub(crate) fn terminal_capabilities(
    input_is_terminal: bool,
    output_is_terminal: bool,
    term: Option<&str>,
) -> TerminalCapabilities {
    TerminalCapabilities {
        input_is_terminal,
        output_is_terminal,
        term: term.map(str::to_string),
    }
}

#[path = "test_part_0.rs"]
mod test_part_0;
