//! Protected cleanup authority for non-Linux managed-process owners (FT-020).
//!
//! Windows Job Objects used for supervision get a non-inheritable owner handle
//! and a DACL that denies WRITE_DAC/WRITE_OWNER to Everyone. macOS production
//! stays fail-closed unless a launchd bootstrap reaper is registered.
//! `ProcessScopeBackend::production()` remains fail-closed on both platforms
//! until native qualification.

/// Access rights a managed worker must not receive on the cleanup owner.
pub(crate) const DENY_WORKER_OWNER_RIGHTS: u32 = 0x0004_0000 | 0x0008_0000; // WRITE_DAC | WRITE_OWNER

/// Well-known macOS launchd label for a privileged cleanup reaper.
pub(crate) const MACOS_CLEANUP_REAPER_LABEL: &str = "com.skills-yaml.nib.cleanup-reaper";

pub(crate) fn deny_worker_owner_rights_include_write_dac() -> bool {
    DENY_WORKER_OWNER_RIGHTS & 0x0004_0000 != 0
}

pub(crate) fn deny_worker_owner_rights_include_write_owner() -> bool {
    DENY_WORKER_OWNER_RIGHTS & 0x0008_0000 != 0
}

pub(crate) fn protected_owner_policy_is_configured() -> bool {
    deny_worker_owner_rights_include_write_dac()
        && deny_worker_owner_rights_include_write_owner()
        && !MACOS_CLEANUP_REAPER_LABEL.is_empty()
}

/// Supervisor identity bound to one protected-owner generation.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProtectedOwnerGeneration {
    pub(crate) owner_pid: u32,
    pub(crate) generation: u64,
}

impl ProtectedOwnerGeneration {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn current(generation: u64) -> Result<Self, String> {
        if generation == 0 {
            return Err("protected cleanup owner generation must be non-zero".to_string());
        }
        Ok(Self {
            owner_pid: std::process::id(),
            generation,
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn matches_live_owner(&self, live: &Self) -> bool {
        self.owner_pid == live.owner_pid && self.generation == live.generation
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn reject_stale_protected_owner(
    live: &ProtectedOwnerGeneration,
    candidate: &ProtectedOwnerGeneration,
) -> Result<(), String> {
    if candidate.matches_live_owner(live) {
        Ok(())
    } else {
        Err(format!(
            "stale protected cleanup owner generation {} (pid {}) cannot replace live generation {} (pid {})",
            candidate.generation, candidate.owner_pid, live.generation, live.owner_pid
        ))
    }
}

#[cfg(windows)]
pub(crate) fn require_windows_protected_owner() -> Result<(), String> {
    let job = create_protected_job_object()
        .map_err(|error| format!("Windows protected cleanup owner preflight failed: {error}"))?;
    drop(job);
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn macos_bootstrap_reaper_registered() -> bool {
    std::path::Path::new("/Library/LaunchDaemons")
        .join(format!("{MACOS_CLEANUP_REAPER_LABEL}.plist"))
        .is_file()
}

#[cfg(target_os = "macos")]
pub(crate) fn require_macos_protected_reaper() -> Result<(), String> {
    if macos_bootstrap_reaper_registered() {
        Ok(())
    } else {
        Err(
            "production subagent supervision is unavailable on macOS until cleanup authority is isolated from the managed worker"
                .to_string(),
        )
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::DENY_WORKER_OWNER_RIGHTS;
    use std::ffi::c_void;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::ptr;
    use windows_sys::Win32::Foundation::{
        DuplicateHandle, GetHandleInformation, DUPLICATE_SAME_ACCESS, GENERIC_ALL, HANDLE,
        HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::{
        AddAccessAllowedAce, AddAccessDeniedAce, AllocateAndInitializeSid, FreeSid,
        GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor,
        SetSecurityDescriptorDacl, SetSecurityDescriptorOwner, TokenUser, ACL, ACL_REVISION, PSID,
        SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SECURITY_WORLD_SID_AUTHORITY, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows_sys::Win32::System::JobObjects::CreateJobObjectW;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
    const SECURITY_WORLD_RID: u32 = 0;
    const ACL_BYTES: usize = 1024;

    pub(crate) struct ProtectedJob {
        pub(crate) handle: OwnedHandle,
    }

    struct JobSecurity {
        descriptor: SECURITY_DESCRIPTOR,
        acl: Vec<u8>,
        world_sid: PSID,
        token: OwnedHandle,
        user_buffer: Vec<u8>,
        attrs: SECURITY_ATTRIBUTES,
    }

    impl Drop for JobSecurity {
        fn drop(&mut self) {
            if !self.world_sid.is_null() {
                unsafe {
                    let _ = FreeSid(self.world_sid);
                }
            }
        }
    }

    impl JobSecurity {
        fn owner_sid(&self) -> PSID {
            let user = unsafe { &*(self.user_buffer.as_ptr() as *const TOKEN_USER) };
            user.User.Sid
        }

        fn prepare() -> io::Result<Self> {
            let token = open_query_token()?;
            let user_buffer = read_token_user(&token)?;
            let world_sid = allocate_world_sid()?;
            let mut security = Self {
                descriptor: unsafe { std::mem::zeroed() },
                acl: vec![0u8; ACL_BYTES],
                world_sid,
                token,
                user_buffer,
                attrs: SECURITY_ATTRIBUTES {
                    nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: ptr::null_mut(),
                    bInheritHandle: 0,
                },
            };
            security.populate_descriptor()?;
            Ok(security)
        }

        fn populate_descriptor(&mut self) -> io::Result<()> {
            let descriptor = (&raw mut self.descriptor).cast::<c_void>();
            if unsafe { InitializeSecurityDescriptor(descriptor, SECURITY_DESCRIPTOR_REVISION) }
                == 0
            {
                return Err(last_os_error(
                    "cannot initialize Windows protected-owner security descriptor",
                ));
            }
            let acl = self.acl.as_mut_ptr().cast::<ACL>();
            if unsafe { InitializeAcl(acl, ACL_BYTES as u32, ACL_REVISION) } == 0 {
                return Err(last_os_error(
                    "cannot initialize Windows protected-owner ACL",
                ));
            }
            if unsafe {
                AddAccessDeniedAce(acl, ACL_REVISION, DENY_WORKER_OWNER_RIGHTS, self.world_sid)
            } == 0
            {
                return Err(last_os_error(
                    "cannot deny WRITE_DAC/WRITE_OWNER on Windows protected-owner job",
                ));
            }
            if unsafe { AddAccessAllowedAce(acl, ACL_REVISION, GENERIC_ALL, self.owner_sid()) } == 0
            {
                return Err(last_os_error(
                    "cannot grant owner access on Windows protected-owner job",
                ));
            }
            if unsafe { SetSecurityDescriptorDacl(descriptor, 1, acl, 0) } == 0 {
                return Err(last_os_error(
                    "cannot attach DACL to Windows protected-owner job",
                ));
            }
            if unsafe { SetSecurityDescriptorOwner(descriptor, self.owner_sid(), 0) } == 0 {
                return Err(last_os_error(
                    "cannot set owner on Windows protected-owner job",
                ));
            }
            self.attrs.lpSecurityDescriptor = descriptor;
            Ok(())
        }
    }

    fn open_query_token() -> io::Result<OwnedHandle> {
        let mut token = INVALID_HANDLE_VALUE;
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(last_os_error(
                "cannot open process token for protected owner",
            ));
        }
        Ok(owned(token))
    }

    fn read_token_user(token: &OwnedHandle) -> io::Result<Vec<u8>> {
        let mut needed = 0u32;
        unsafe {
            GetTokenInformation(raw(token), TokenUser, ptr::null_mut(), 0, &mut needed);
        }
        if needed == 0 {
            return Err(last_os_error(
                "cannot size Windows protected-owner token user",
            ));
        }
        let mut buffer = vec![0u8; needed as usize];
        if unsafe {
            GetTokenInformation(
                raw(token),
                TokenUser,
                buffer.as_mut_ptr().cast::<c_void>(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(last_os_error(
                "cannot read Windows protected-owner token user",
            ));
        }
        Ok(buffer)
    }

    fn allocate_world_sid() -> io::Result<PSID> {
        let mut sid: PSID = ptr::null_mut();
        let authority = SECURITY_WORLD_SID_AUTHORITY;
        if unsafe {
            AllocateAndInitializeSid(
                &raw const authority,
                1,
                SECURITY_WORLD_RID,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                &mut sid,
            )
        } == 0
        {
            return Err(last_os_error(
                "cannot allocate Everyone SID for protected owner",
            ));
        }
        Ok(sid)
    }

    pub(crate) fn create_protected_job_object() -> io::Result<ProtectedJob> {
        let security = JobSecurity::prepare()?;
        let created = unsafe { CreateJobObjectW(&raw const security.attrs, ptr::null()) };
        if created.is_null() {
            return Err(last_os_error(
                "cannot create Windows Job Object with protected-owner DACL",
            ));
        }
        let created = owned(created);
        let owner = duplicate_non_inheritable(&created)?;
        drop(created);
        if handle_is_inheritable(&owner)? {
            return Err(io::Error::other(
                "Windows protected-owner job handle must not be inheritable",
            ));
        }
        Ok(ProtectedJob { handle: owner })
    }

    fn duplicate_non_inheritable(source: &OwnedHandle) -> io::Result<OwnedHandle> {
        let mut duplicated = INVALID_HANDLE_VALUE;
        if unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                raw(source),
                GetCurrentProcess(),
                &mut duplicated,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            return Err(last_os_error(
                "cannot duplicate Windows protected-owner job handle",
            ));
        }
        Ok(owned(duplicated))
    }

    pub(crate) fn handle_is_inheritable(handle: &OwnedHandle) -> io::Result<bool> {
        let mut flags = 0u32;
        if unsafe { GetHandleInformation(raw(handle), &mut flags) } == 0 {
            return Err(last_os_error(
                "cannot inspect Windows protected-owner handle inheritance",
            ));
        }
        Ok(flags & HANDLE_FLAG_INHERIT != 0)
    }

    fn owned(handle: HANDLE) -> OwnedHandle {
        unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) }
    }

    fn raw(handle: &OwnedHandle) -> HANDLE {
        handle.as_raw_handle() as HANDLE
    }

    fn last_os_error(context: &str) -> io::Error {
        let error = io::Error::last_os_error();
        io::Error::new(error.kind(), format!("{context}: {error}"))
    }
}

#[cfg(windows)]
pub(crate) use windows_impl::{create_protected_job_object, handle_is_inheritable, ProtectedJob};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_deny_mask_includes_write_dac_and_write_owner() {
        assert!(deny_worker_owner_rights_include_write_dac());
        assert!(deny_worker_owner_rights_include_write_owner());
        assert_eq!(DENY_WORKER_OWNER_RIGHTS, 0x000C_0000);
    }

    #[test]
    fn stale_generation_cannot_replace_live_owner() {
        let live = ProtectedOwnerGeneration::current(7).expect("live generation");
        let stale = ProtectedOwnerGeneration {
            owner_pid: live.owner_pid,
            generation: 6,
        };
        let error = reject_stale_protected_owner(&live, &stale).expect_err("stale owner");
        assert!(error.contains("stale protected cleanup owner"), "{error}");
        reject_stale_protected_owner(&live, &live).expect("live owner");
    }

    #[test]
    fn zero_generation_is_rejected() {
        let error = ProtectedOwnerGeneration::current(0).expect_err("zero generation");
        assert!(error.contains("non-zero"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_production_backend_is_never_windows_or_macos() {
        match crate::sandbox::process::ProcessScopeBackend::production() {
            Ok(crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace) => {}
            Ok(other) => panic!("Linux production backend must stay PID-namespace, got {other:?}"),
            Err(_) => {}
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_protected_job_handle_is_not_inheritable() {
        let job = create_protected_job_object().expect("protected job");
        assert!(
            !handle_is_inheritable(&job.handle).expect("inheritance probe"),
            "protected owner handle must not be inheritable"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_production_stays_fail_closed_after_protected_owner_preflight() {
        require_windows_protected_owner().expect("protected owner preflight");
        let error = crate::sandbox::process::ProcessScopeBackend::production()
            .expect_err("Windows production remains fail-closed pending qualification");
        assert!(error.contains("unavailable on Windows"), "{error}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_production_stays_fail_closed_without_or_after_reaper_probe() {
        let error = crate::sandbox::process::ProcessScopeBackend::production()
            .expect_err("macOS production remains fail-closed");
        assert!(error.contains("unavailable on macOS"), "{error}");
    }
}
