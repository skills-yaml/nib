//! T043 split.

use super::*;

pub(crate) fn session_directory_identity_anchor(visible_marker: &Path) -> Result<PathBuf, String> {
    crate::daemons::state::daemon_lock_anchor_path(visible_marker)
}

pub(crate) fn session_lock_stripe(id: &str) -> usize {
    let hash = id
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    (hash as usize) % SESSION_LOCK_STRIPES
}

#[cfg(test)]
thread_local! {
    pub(crate) static PAUSE_RECORD_EVENT_ONCE_COMMIT: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn pause_record_event_once_commit(deadline: Instant) {
    if PAUSE_RECORD_EVENT_ONCE_COMMIT.get() {
        while Instant::now() < deadline {
            std::thread::yield_now();
        }
    }
}

#[cfg(not(test))]
pub(crate) fn pause_record_event_once_commit(_deadline: Instant) {}
