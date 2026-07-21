#![cfg(windows)]

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child as StdChild, Command as StdCommand};
use std::ptr;
use std::time::{Duration, Instant};

use tokio::process::{Child, Command};
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
    JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_PROCESS_ID_LIST,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, OpenThread, ResumeThread, WaitForSingleObject, CREATE_SUSPENDED,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, THREAD_SUSPEND_RESUME,
};

const TERMINATED_JOB_EXIT_CODE: u32 = 1;
const ERROR_CLEANUP_WAIT_MILLIS: u32 = 5_000;
const MAX_OBSERVED_JOB_PROCESSES: usize = 65_536;
const MAX_PROCESS_CAPTURE_ATTEMPTS: usize = 8;

struct JobProcessObservation {
    handles: Vec<OwnedHandle>,
    total_processes: Option<u32>,
    error: Option<io::Error>,
}

struct ProcessHandleCapture {
    handles: Vec<OwnedHandle>,
    error: Option<io::Error>,
    unstable: bool,
}

/// Owns the unnamed Job Object containing one managed child tree.
///
/// The handle is deliberately non-inheritable. `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
/// makes handle closure a second termination path if explicit job termination fails.
#[doc(hidden)]
pub struct WindowsJob {
    handle: Option<OwnedHandle>,
}

impl WindowsJob {
    fn create() -> io::Result<Self> {
        // A null SECURITY_ATTRIBUTES pointer creates a non-inheritable handle, and
        // a null name gives each managed child an independent Job Object.
        let raw_handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if raw_handle.is_null() {
            return Err(last_os_error("cannot create Windows Job Object"));
        }
        let handle = owned_handle(raw_handle);

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                raw_handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast::<c_void>(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            return Err(last_os_error(
                "cannot configure Windows Job Object kill-on-close containment",
            ));
        }

        Ok(Self {
            handle: Some(handle),
        })
    }

    fn attach_and_resume(&mut self, child: &mut Child) -> io::Result<()> {
        let result = self.try_attach_and_resume(child);
        if result.is_err() {
            self.fail_closed(child);
        }
        result
    }

    fn try_attach_and_resume(&self, child: &Child) -> io::Result<()> {
        let process_handle = child
            .raw_handle()
            .map(|handle| handle as HANDLE)
            .ok_or_else(|| io::Error::other("suspended Windows child has no process handle"))?;
        let process_id = child
            .id()
            .ok_or_else(|| io::Error::other("suspended Windows child has no process id"))?;
        let job_handle = self
            .handle
            .as_ref()
            .map(raw_handle)
            .ok_or_else(|| io::Error::other("Windows Job Object is already closed"))?;

        if unsafe { AssignProcessToJobObject(job_handle, process_handle) } == 0 {
            return Err(last_os_error(
                "cannot assign suspended child to Windows Job Object",
            ));
        }

        resume_only_thread(process_id)
    }

    fn fail_closed(&mut self, child: &mut Child) {
        self.terminate();

        // Assignment itself can fail, leaving the suspended child outside the job.
        // Kill that process directly as well and briefly wait for termination before
        // returning the spawn error. The child is never resumed on an error path.
        let process_handle = child.raw_handle().map(|handle| handle as HANDLE);
        let _ = child.start_kill();
        if let Some(process_handle) = process_handle {
            unsafe {
                let _ = WaitForSingleObject(process_handle, ERROR_CLEANUP_WAIT_MILLIS);
            }
        }
    }

    fn attach_std_and_resume(&mut self, child: &mut StdChild) -> io::Result<()> {
        let result = self.try_attach_std_and_resume(child);
        if result.is_err() {
            self.fail_closed_std(child);
        }
        result
    }

    fn try_attach_std_and_resume(&self, child: &StdChild) -> io::Result<()> {
        let process_handle = child.as_raw_handle() as HANDLE;
        let process_id = child.id();
        let job_handle = self
            .handle
            .as_ref()
            .map(raw_handle)
            .ok_or_else(|| io::Error::other("Windows Job Object is already closed"))?;

        if unsafe { AssignProcessToJobObject(job_handle, process_handle) } == 0 {
            return Err(last_os_error(
                "cannot assign suspended child to Windows Job Object",
            ));
        }

        resume_only_thread(process_id)
    }

    fn fail_closed_std(&mut self, child: &mut StdChild) {
        self.terminate();
        let process_handle = child.as_raw_handle() as HANDLE;
        let _ = child.kill();
        let wait_result = unsafe { WaitForSingleObject(process_handle, ERROR_CLEANUP_WAIT_MILLIS) };
        if wait_result == WAIT_OBJECT_0 {
            let _ = child.wait();
        }
    }

    /// Terminates every process associated with this job while retaining the handle
    /// long enough for callers to verify that the job is empty.
    pub(super) fn terminate(&mut self) {
        if let Some(handle) = &self.handle {
            unsafe {
                let _ = TerminateJobObject(raw_handle(handle), TERMINATED_JOB_EXIT_CODE);
            }
        }
    }

    pub(super) fn wait_until_empty(&self, timeout: Duration) -> io::Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.accounting()?.ActiveProcesses == 0 {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Terminates the complete job and waits until no assigned process remains.
    ///
    /// This is public for the `nib` binary's synchronous bounded-command launcher;
    /// the library supervisor uses the same proof internally. Callers must retain
    /// this handle until the result is known.
    #[doc(hidden)]
    pub fn terminate_and_wait(&mut self, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| io::Error::other("Windows Job Object is already closed"))?;
        let observed = self.capture_process_handles_until_stable(deadline);
        let termination =
            if unsafe { TerminateJobObject(raw_handle(handle), TERMINATED_JOB_EXIT_CODE) } == 0 {
                Err(last_os_error("cannot terminate Windows Job Object"))
            } else {
                Ok(())
            };
        let cleanup = match self.wait_until_observed_processes_exit(&observed.handles, deadline) {
            Ok(true) => Ok(()),
            Ok(false) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Windows Job Object processes did not signal before the cleanup deadline",
            )),
            Err(error) => Err(error),
        };
        let cleanup = cleanup.and_then(|()| {
            let Some(expected_total) = observed.total_processes else {
                return Ok(());
            };
            let final_accounting = self.accounting()?;
            if !process_generation_is_unchanged(expected_total, &final_accounting) {
                return Err(io::Error::other(format!(
                    "Windows Job Object process generation changed during termination: expected {expected_total}, observed {}",
                    final_accounting.TotalProcesses
                )));
            }
            Ok(())
        });
        let cleanup = match (observed.error, cleanup) {
            (None, result) => result,
            (Some(error), Ok(())) => Err(error),
            (Some(observation), Err(cleanup)) => Err(io::Error::other(format!(
                "{observation}; cleanup verification failed: {cleanup}"
            ))),
        };
        match (termination, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(termination), Err(cleanup)) => Err(io::Error::other(format!(
                "{termination}; cleanup verification failed: {cleanup}"
            ))),
        }
    }

    fn accounting(&self) -> io::Result<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| io::Error::other("Windows Job Object is already closed"))?;
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        let queried = unsafe {
            QueryInformationJobObject(
                raw_handle(handle),
                JobObjectBasicAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast::<c_void>(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                ptr::null_mut(),
            )
        };
        if queried == 0 {
            return Err(last_os_error("cannot query Windows Job Object cleanup"));
        }
        Ok(accounting)
    }

    fn capture_process_handles_until_stable(&self, deadline: Instant) -> JobProcessObservation {
        let mut observed = Vec::new();
        for attempt in 0..MAX_PROCESS_CAPTURE_ATTEMPTS {
            if Instant::now() >= deadline {
                return JobProcessObservation {
                    handles: observed,
                    total_processes: None,
                    error: Some(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Windows Job Object process capture exceeded the cleanup deadline",
                    )),
                };
            }
            let before = match self.accounting() {
                Ok(accounting) => accounting,
                Err(error) => {
                    return JobProcessObservation {
                        handles: observed,
                        total_processes: None,
                        error: Some(error),
                    };
                }
            };
            let capture = self.capture_process_handles_once(deadline);
            let captured_count = capture.handles.len();
            let capture_stable = !capture.unstable;
            observed.extend(capture.handles);
            if let Some(error) = capture.error {
                return JobProcessObservation {
                    handles: observed,
                    total_processes: None,
                    error: Some(error),
                };
            }
            let after = match self.accounting() {
                Ok(accounting) => accounting,
                Err(error) => {
                    return JobProcessObservation {
                        handles: observed,
                        total_processes: None,
                        error: Some(error),
                    };
                }
            };
            if process_capture_is_stable(&before, &after, captured_count, capture_stable) {
                return JobProcessObservation {
                    handles: observed,
                    total_processes: Some(after.TotalProcesses),
                    error: None,
                };
            }
            if attempt + 1 == MAX_PROCESS_CAPTURE_ATTEMPTS {
                return JobProcessObservation {
                    handles: observed,
                    total_processes: None,
                    error: Some(io::Error::other(
                        "Windows Job Object process membership did not stabilize before termination",
                    )),
                };
            }
            let now = Instant::now();
            if now < deadline {
                std::thread::sleep(Duration::from_millis(10).min(deadline - now));
            }
        }
        unreachable!("bounded Windows process capture loop always returns")
    }

    fn capture_process_handles_once(&self, deadline: Instant) -> ProcessHandleCapture {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| io::Error::other("Windows Job Object is already closed"));
        let handle = match handle {
            Ok(handle) => handle,
            Err(error) => {
                return ProcessHandleCapture {
                    handles: Vec::new(),
                    error: Some(error),
                    unstable: false,
                };
            }
        };
        let process_ids = match query_job_process_ids(raw_handle(handle), deadline) {
            Ok(process_ids) => process_ids,
            Err(error) => {
                return ProcessHandleCapture {
                    handles: Vec::new(),
                    error: Some(error),
                    unstable: false,
                };
            }
        };
        let mut processes = Vec::with_capacity(process_ids.len());
        let mut unstable = false;
        for process_id in process_ids {
            if Instant::now() >= deadline {
                return ProcessHandleCapture {
                    handles: processes,
                    error: Some(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Windows Job Object process capture exceeded the cleanup deadline",
                    )),
                    unstable,
                };
            }
            let process = unsafe {
                OpenProcess(
                    PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                    0,
                    process_id,
                )
            };
            if process.is_null() {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                    unstable = true;
                    continue;
                }
                return ProcessHandleCapture {
                    handles: processes,
                    error: Some(io::Error::new(
                        error.kind(),
                        format!("cannot retain Windows Job Object process {process_id}: {error}"),
                    )),
                    unstable,
                };
            }
            let process = owned_handle(process);
            let mut belongs_to_job = 0;
            if unsafe {
                IsProcessInJob(
                    raw_handle(&process),
                    raw_handle(handle),
                    &mut belongs_to_job,
                )
            } == 0
            {
                let error = last_os_error("cannot verify Windows Job Object process membership");
                processes.push(process);
                return ProcessHandleCapture {
                    handles: processes,
                    error: Some(error),
                    unstable,
                };
            }
            if belongs_to_job == 0 {
                unstable = true;
                continue;
            }
            processes.push(process);
        }
        ProcessHandleCapture {
            handles: processes,
            error: None,
            unstable,
        }
    }

    fn wait_until_observed_processes_exit(
        &self,
        observed: &[OwnedHandle],
        deadline: Instant,
    ) -> io::Result<bool> {
        loop {
            let mut all_signaled = true;
            for process in observed {
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                match unsafe { WaitForSingleObject(raw_handle(process), 0) } {
                    WAIT_OBJECT_0 => {}
                    WAIT_TIMEOUT => all_signaled = false,
                    WAIT_FAILED => {
                        return Err(last_os_error(
                            "cannot wait for Windows Job Object process termination",
                        ));
                    }
                    result => {
                        return Err(io::Error::other(format!(
                            "unexpected Windows process wait result {result}"
                        )));
                    }
                }
            }
            if all_signaled && self.accounting()?.ActiveProcesses == 0 {
                return Ok(true);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(10).min(deadline - now));
        }
    }
}

fn process_capture_is_stable(
    before: &JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    after: &JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    captured_count: usize,
    capture_stable: bool,
) -> bool {
    capture_stable
        && before.TotalProcesses == after.TotalProcesses
        && before.ActiveProcesses == after.ActiveProcesses
        && after.ActiveProcesses as usize == captured_count
}

fn process_generation_is_unchanged(
    expected_total: u32,
    final_accounting: &JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
) -> bool {
    final_accounting.TotalProcesses == expected_total
}

fn query_job_process_ids(handle: HANDLE, deadline: Instant) -> io::Result<Vec<u32>> {
    let mut capacity = 16_usize;
    loop {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Windows Job Object process list exceeded the cleanup deadline",
            ));
        }
        let extra_entries = capacity.saturating_sub(1);
        let buffer_bytes = size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>()
            .checked_add(extra_entries.saturating_mul(size_of::<usize>()))
            .ok_or_else(|| io::Error::other("Windows Job Object process list size overflowed"))?;
        let word_count = buffer_bytes.div_ceil(size_of::<usize>());
        let mut buffer = vec![0_usize; word_count];
        let information = buffer
            .as_mut_ptr()
            .cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
        let queried = unsafe {
            QueryInformationJobObject(
                handle,
                JobObjectBasicProcessIdList,
                information.cast::<c_void>(),
                u32::try_from(buffer_bytes).map_err(|_| {
                    io::Error::other("Windows Job Object process list exceeded Win32 limits")
                })?,
                ptr::null_mut(),
            )
        };
        let assigned = unsafe { (*information).NumberOfAssignedProcesses as usize };
        let returned = unsafe { (*information).NumberOfProcessIdsInList as usize };
        if assigned > MAX_OBSERVED_JOB_PROCESSES {
            return Err(io::Error::other(format!(
                "Windows Job Object contains more than {MAX_OBSERVED_JOB_PROCESSES} processes"
            )));
        }
        if returned > capacity {
            return Err(io::Error::other(
                "Windows Job Object returned more process ids than the supplied buffer",
            ));
        }
        if returned > assigned {
            return Err(io::Error::other(
                "Windows Job Object returned an inconsistent process count",
            ));
        }
        if queried != 0 && returned >= assigned {
            let process_ids = unsafe {
                std::slice::from_raw_parts(
                    std::ptr::addr_of!((*information).ProcessIdList).cast::<usize>(),
                    returned,
                )
            };
            return process_ids
                .iter()
                .copied()
                .map(|process_id| {
                    u32::try_from(process_id).map_err(|_| {
                        io::Error::other(format!(
                            "Windows Job Object returned invalid process id {process_id}"
                        ))
                    })
                })
                .collect();
        }

        let error = io::Error::last_os_error();
        if queried == 0 && error.raw_os_error() != Some(ERROR_MORE_DATA as i32) {
            return Err(io::Error::new(
                error.kind(),
                format!("cannot query Windows Job Object process list: {error}"),
            ));
        }
        capacity = assigned.max(capacity.saturating_mul(2));
        if capacity == 0 || capacity > MAX_OBSERVED_JOB_PROCESSES {
            return Err(io::Error::other(
                "Windows Job Object process list did not converge",
            ));
        }
    }
}

impl Drop for WindowsJob {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Spawns a Tokio child that cannot run before it belongs to its Job Object.
///
/// `CREATE_SUSPENDED` lets the standard/Tokio command path retain responsibility
/// for Windows argument quoting, environment blocks, stdio handles, and async wait.
/// The primary thread is resumed only after successful job assignment.
pub(super) fn spawn_contained(command: &mut Command) -> io::Result<(Child, WindowsJob)> {
    let mut job = WindowsJob::create()?;
    configure_suspended_spawn(command);

    let mut child = command.spawn()?;
    job.attach_and_resume(&mut child)?;
    Ok((child, job))
}

/// Synchronous counterpart to [`spawn_contained`], used by bounded command paths.
#[doc(hidden)]
pub fn spawn_contained_std(command: &mut StdCommand) -> io::Result<(StdChild, WindowsJob)> {
    let mut job = WindowsJob::create()?;
    command.creation_flags(CREATE_SUSPENDED);

    let mut child = command.spawn()?;
    job.attach_std_and_resume(&mut child)?;
    Ok((child, job))
}

fn configure_suspended_spawn(command: &mut Command) {
    command.kill_on_drop(true).creation_flags(CREATE_SUSPENDED);
}

fn resume_only_thread(process_id: u32) -> io::Result<()> {
    let thread_id = only_thread_id(process_id)?;
    let thread_handle = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
    if thread_handle.is_null() {
        return Err(last_os_error(
            "cannot open suspended Windows child's primary thread",
        ));
    }
    let thread_handle = owned_handle(thread_handle);

    let previous_suspend_count = unsafe { ResumeThread(raw_handle(&thread_handle)) };
    if previous_suspend_count == u32::MAX {
        return Err(last_os_error(
            "cannot resume suspended Windows child's primary thread",
        ));
    }
    if previous_suspend_count != 1 {
        return Err(io::Error::other(format!(
            "suspended Windows child had unexpected primary-thread suspend count {previous_suspend_count}"
        )));
    }

    Ok(())
}

fn only_thread_id(process_id: u32) -> io::Result<u32> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(last_os_error(
            "cannot snapshot threads for suspended Windows child",
        ));
    }
    let snapshot = owned_handle(snapshot);
    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        ..THREADENTRY32::default()
    };

    if unsafe { Thread32First(raw_handle(&snapshot), &mut entry) } == 0 {
        return Err(last_os_error(
            "cannot enumerate threads for suspended Windows child",
        ));
    }

    let mut matching_thread = None;
    loop {
        if entry.th32OwnerProcessID == process_id {
            if matching_thread.replace(entry.th32ThreadID).is_some() {
                return Err(io::Error::other(
                    "suspended Windows child unexpectedly has more than one thread",
                ));
            }
        }

        if unsafe { Thread32Next(raw_handle(&snapshot), &mut entry) } != 0 {
            continue;
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
            break;
        }
        return Err(io::Error::new(
            error.kind(),
            format!("cannot continue suspended Windows child thread enumeration: {error}"),
        ));
    }

    matching_thread
        .ok_or_else(|| io::Error::other("suspended Windows child's primary thread was not found"))
}

fn owned_handle(handle: HANDLE) -> OwnedHandle {
    // SAFETY: every caller transfers a newly opened, non-null Win32 handle.
    unsafe { OwnedHandle::from_raw_handle(handle.cast()) }
}

fn raw_handle(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle() as HANDLE
}

fn last_os_error(context: &str) -> io::Error {
    let error = io::Error::last_os_error();
    io::Error::new(error.kind(), format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };

    use super::*;

    const ROLE_ENV: &str = "NIB_WINDOWS_JOB_TEST_ROLE";
    const PID_PATH_ENV: &str = "NIB_WINDOWS_JOB_TEST_PID_PATH";
    const READY_PATH_ENV: &str = "NIB_WINDOWS_JOB_TEST_READY_PATH";
    const FIXTURE_TEST: &str =
        "sandbox::windows_job::tests::nested_job_drop_terminates_descendant_tree";

    #[test]
    fn cleanup_accounting_fence_rejects_unstable_or_new_processes() {
        let before = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION {
            TotalProcesses: 2,
            ActiveProcesses: 1,
            ..JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default()
        };
        let mut after = before;

        assert!(process_capture_is_stable(&before, &after, 1, true));
        assert!(!process_capture_is_stable(&before, &after, 1, false));
        after.TotalProcesses += 1;
        assert!(!process_capture_is_stable(&before, &after, 1, true));
        assert!(!process_generation_is_unchanged(2, &after));
    }

    #[tokio::test]
    async fn nested_job_drop_terminates_descendant_tree() {
        match std::env::var(ROLE_ENV).as_deref() {
            Ok("nested-parent") => {
                exercise_descendant_termination().await;
                return;
            }
            Ok("leader") => {
                run_leader_fixture().await;
                return;
            }
            Ok("descendant") => {
                let ready_path = std::env::var_os(READY_PATH_ENV).expect("ready path");
                fs::write(ready_path, b"ready").expect("write descendant readiness");
                loop {
                    std::thread::sleep(Duration::from_secs(60));
                }
            }
            _ => {}
        }

        // The helper is assigned to this outer job. It creates another contained
        // child tree itself, proving that the inner assignment works as a nested job.
        let mut command = fixture_command("nested-parent");
        let (mut child, outer_job) = spawn_contained(&mut command).expect("outer job child");
        let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
            .await
            .expect("nested parent timeout")
            .expect("wait for nested parent");
        drop(outer_job);
        assert!(status.success(), "nested parent failed with {status}");
    }

    async fn exercise_descendant_termination() {
        let directory = FixtureDirectory::new();
        let pid_path = directory.path().join("descendant.pid");
        let ready_path = directory.path().join("descendant.ready");
        let mut command = fixture_command("leader");
        command
            .env(PID_PATH_ENV, &pid_path)
            .env(READY_PATH_ENV, &ready_path);

        let (mut leader, inner_job) = spawn_contained(&mut command).expect("inner job child");
        wait_for_path(&ready_path).await;
        let descendant_id: u32 = fs::read_to_string(&pid_path)
            .expect("read descendant pid")
            .parse()
            .expect("numeric descendant pid");
        let descendant = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, descendant_id) };
        assert!(!descendant.is_null(), "open descendant process handle");
        let descendant = owned_handle(descendant);

        drop(inner_job);
        let wait_result = unsafe { WaitForSingleObject(raw_handle(&descendant), 5_000) };
        assert_eq!(
            wait_result, WAIT_OBJECT_0,
            "descendant survived Windows Job Object closure"
        );
        tokio::time::timeout(Duration::from_secs(5), leader.wait())
            .await
            .expect("leader termination timeout")
            .expect("wait for terminated leader");
    }

    fn fixture_command(role: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .args(["--exact", FIXTURE_TEST, "--nocapture"])
            .env(ROLE_ENV, role)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    async fn run_leader_fixture() {
        let pid_path = std::env::var_os(PID_PATH_ENV).expect("pid path");
        let ready_path = std::env::var_os(READY_PATH_ENV).expect("ready path");
        let mut descendant = fixture_command("descendant");
        descendant.env(READY_PATH_ENV, ready_path);
        let mut descendant = descendant.spawn().expect("spawn descendant fixture");
        fs::write(
            pid_path,
            descendant.id().expect("descendant id").to_string(),
        )
        .expect("write descendant pid");
        let _ = descendant.wait().await;
    }

    async fn wait_for_path(path: &Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !path.is_file() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fixture readiness timeout");
    }

    struct FixtureDirectory(PathBuf);

    impl FixtureDirectory {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "nib-windows-job-test-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create fixture directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for FixtureDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
