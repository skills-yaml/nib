//! T043 split.

use super::*;

impl StableDirectory {
    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
    pub(crate) fn save_bytes_atomically_expected_with_all_hooks_guarded(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expectation: AtomicSaveExpectation<'_>,
        hooks: AtomicSaveHooks<impl FnOnce() -> Result<(), String>, impl FnOnce(), impl FnOnce()>,
        namespace_guard: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<FilePublicationReceipt, FilePublicationError> {
        let AtomicSaveExpectation {
            require_attached_before_commit,
            file: expected,
            retain_publication_lock,
        } = expectation;
        let AtomicSaveHooks {
            before_commit,
            after_evacuation,
            before_receipt,
        } = hooks;
        let destination = self.relative_file(path)?.to_path_buf();
        let temporary = deterministic_artifact_name(
            temporary_prefix,
            destination.as_os_str().as_encoded_bytes(),
            ".tmp",
        );
        let temporary_path = self.path.join(&temporary);
        let previous =
            deterministic_previous_artifact_name(temporary_prefix, destination.as_os_str())?;
        let previous_path = self.path.join(&previous);
        namespace_guard()?;
        self.recover_atomic_transaction_guarded(
            path,
            &temporary_path,
            &previous_path,
            false,
            false,
            namespace_guard,
        )?;

        #[cfg(test)]
        let any_expected_file = match expected {
            FileExpectation::Any if self.path_exists(path)? => Some(self.open_read(path)?),
            _ => None,
        };
        let expected = match expected {
            #[cfg(test)]
            FileExpectation::Any if any_expected_file.is_some() => {
                FileExpectation::Present(any_expected_file.as_ref().expect("opened expected file"))
            }
            #[cfg(test)]
            FileExpectation::Any => FileExpectation::Missing,
            expectation => expectation,
        };
        self.verify_file_expectation(path, expected)?;

        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        configure_capability_owner_only(&mut options);
        configure_capability_no_follow(&mut options);
        configure_capability_delete_access(&mut options, true, true);
        namespace_guard()?;
        pause_atomic_publication_phase(path, encoded, "temporary_create")?;
        let mut file = self
            .directory
            .open_with(&temporary, &options)
            .map(cap_std::fs::File::into_std)
            .map_err(|error| format!("failed to create temporary state file: {error}"))?;
        file.lock()
            .map_err(|error| format!("failed to lock temporary state file: {error}"))?;
        let mut evacuated_previous = None;
        let mut exact_publication_committed = false;
        let write_result = (|| {
            namespace_guard()?;
            file.write_all(encoded).map_err(|error| error.to_string())?;
            namespace_guard()?;
            file.sync_all().map_err(|error| error.to_string())?;
            let before_commit_attachment_error = self.verify_visible().err();
            if require_attached_before_commit {
                if let Some(error) = before_commit_attachment_error.as_ref() {
                    return Err(error.clone());
                }
            }
            self.verify_file_expectation(path, expected)?;
            namespace_guard()?;
            before_commit()?;
            namespace_guard()?;
            self.verify_file_identity(&temporary_path, &file)?;

            if let FileExpectation::Present(previous_file) = expected {
                namespace_guard()?;
                if let Err(error) = self.move_open_file_no_replace_bound_guarded(
                    path,
                    previous_file,
                    &previous_path,
                    namespace_guard,
                ) {
                    if let Ok(opened_previous) = self.open_read(&previous_path) {
                        let _ = self.rollback_previous_file_guarded(
                            path,
                            &previous_path,
                            &opened_previous,
                            namespace_guard,
                        );
                    }
                    return Err(error);
                }
                evacuated_previous = Some(self.open_read(&previous_path)?);
            } else {
                self.verify_file_expectation(path, FileExpectation::Missing)?;
            }
            after_evacuation();
            pause_atomic_publication_phase(path, encoded, "after_evacuation")?;

            namespace_guard()?;
            let publication = match self.publish_open_file_no_replace_guarded(
                &temporary_path,
                &file,
                path,
                namespace_guard,
            ) {
                Ok(publication) => publication,
                Err(error) => {
                    if let Some(previous_file) = evacuated_previous.as_ref() {
                        let rollback = self.rollback_previous_file_guarded(
                            path,
                            &previous_path,
                            previous_file,
                            namespace_guard,
                        );
                        return match rollback {
                            Ok(()) => Err(error),
                            Err(rollback_error) => Err(format!(
                                "{error}; failed to restore the prior state: {rollback_error}"
                            )),
                        };
                    }
                    return Err(error);
                }
            };
            exact_publication_committed = true;
            pause_atomic_publication_phase(path, encoded, "canonical_publish")?;
            self.verify_published_file(path, &file, publication)?;
            let published = read_open_file_prefix(&file, encoded.len().saturating_add(1))
                .map_err(|error| format!("failed to verify published state: {error}"))?;
            if published != encoded {
                return Err(format!(
                    "published state did not retain the requested bytes: {}",
                    path.display()
                ));
            }
            let post_publication = (|| {
                namespace_guard()?;
                pause_atomic_publication_phase(path, encoded, "directory_sync")?;
                self.sync_directory()?;
                if let Some(previous_file) = evacuated_previous.as_ref() {
                    self.remove_visible_file_if_matches_guarded(
                        &previous_path,
                        previous_file,
                        namespace_guard,
                        || self.verify_publication_bytes(path, &file, encoded),
                    )?;
                }
                self.verify_publication_bytes(path, &file, encoded)?;
                let after_commit_attachment_error = self.verify_visible().err();
                match (
                    before_commit_attachment_error,
                    after_commit_attachment_error,
                ) {
                    (Some(error), _) | (None, Some(error)) => Err(error),
                    (None, None) => Ok(()),
                }
            })();
            match post_publication {
                Ok(()) => Ok(()),
                Err(error) => Err(error),
            }
        })();
        let temporary_cleanup = self
            .cleanup_open_temporary_file_guarded(&temporary_path, &file, namespace_guard, || {
                if exact_publication_committed {
                    self.verify_publication_bytes(path, &file, encoded)
                } else {
                    Ok(())
                }
            })
            .and_then(|()| {
                if exact_publication_committed {
                    self.verify_publication_bytes(path, &file, encoded)
                } else {
                    Ok(())
                }
            });
        let unlock_result = if retain_publication_lock && exact_publication_committed {
            Ok(())
        } else {
            file.unlock()
                .map_err(|error| format!("failed to unlock published state file: {error}"))
        };
        before_receipt();
        let receipt_guard = pause_atomic_publication_phase(path, encoded, "receipt_return")
            .and_then(|()| namespace_guard());
        let temporary_cleanup = match (temporary_cleanup, unlock_result, receipt_guard) {
            (Ok(()), Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(()), Ok(()))
            | (Ok(()), Err(error), Ok(()))
            | (Ok(()), Ok(()), Err(error)) => Err(error),
            (cleanup, unlock, guard) => Err([cleanup.err(), unlock.err(), guard.err()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("; ")),
        };
        match (write_result, temporary_cleanup) {
            (Ok(()), Ok(())) => Ok(FilePublicationReceipt {
                file,
                exact_identity: true,
            }),
            (Err(message), Ok(())) => {
                let receipt = exact_publication_committed.then_some(FilePublicationReceipt {
                    file,
                    exact_identity: true,
                });
                Err(FilePublicationError { message, receipt })
            }
            (Ok(()), Err(message)) => Err(FilePublicationError {
                message,
                receipt: Some(FilePublicationReceipt {
                    file,
                    exact_identity: true,
                }),
            }),
            (Err(message), Err(cleanup_error)) => {
                let receipt = exact_publication_committed.then_some(FilePublicationReceipt {
                    file,
                    exact_identity: true,
                });
                Err(FilePublicationError {
                    message: format!(
                        "{message}; temporary state cleanup also failed: {cleanup_error}"
                    ),
                    receipt,
                })
            }
        }
    }

    pub(crate) fn recover_stale_temporary_files(
        &self,
        temporary_prefix: &str,
        max_entries: usize,
        max_name_bytes: usize,
    ) -> Result<usize, String> {
        self.recover_stale_temporary_files_with_policy(
            temporary_prefix,
            max_entries,
            max_name_bytes,
            false,
        )
    }

    pub(crate) fn recover_stale_temporary_files_strict(
        &self,
        temporary_prefix: &str,
        max_entries: usize,
        max_name_bytes: usize,
    ) -> Result<usize, String> {
        self.recover_stale_temporary_files_with_policy(
            temporary_prefix,
            max_entries,
            max_name_bytes,
            true,
        )
    }

    pub(crate) fn recover_stale_temporary_files_strict_with_guard(
        &self,
        temporary_prefix: &str,
        max_entries: usize,
        max_name_bytes: usize,
        mut namespace_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<usize, String> {
        namespace_guard()?;
        let mut previous_targets = Vec::new();
        let mut temporary_names = Vec::new();
        self.for_each_entry_bounded(max_entries, max_name_bytes, |name| {
            if let Some(target) = parse_previous_artifact_name(&name, temporary_prefix) {
                previous_targets.push(target);
            } else if is_deterministic_artifact_name(&name, temporary_prefix, ".tmp") {
                temporary_names.push(name);
            }
            Ok(())
        })?;
        let mut removed = 0_usize;
        for target_name in previous_targets {
            namespace_guard()?;
            let target = self.path.join(&target_name);
            let temporary = self.path.join(deterministic_artifact_name(
                temporary_prefix,
                target_name.as_encoded_bytes(),
                ".tmp",
            ));
            let previous = self.path.join(deterministic_previous_artifact_name(
                temporary_prefix,
                &target_name,
            )?);
            if self.recover_atomic_transaction_guarded(
                &target,
                &temporary,
                &previous,
                true,
                true,
                &mut namespace_guard,
            )? {
                removed = removed.saturating_add(1);
            }
        }
        for name in temporary_names {
            namespace_guard()?;
            let path = self.path.join(&name);
            if !self.path_exists(&path)? {
                continue;
            }
            let file = self.open_read_write(&path)?;
            match file.try_lock() {
                Ok(()) => {
                    self.remove_visible_file_if_matches_guarded(
                        &path,
                        &file,
                        &mut namespace_guard,
                        || Ok(()),
                    )?;
                    removed = removed.saturating_add(1);
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(format!(
                        "live atomic transaction artifact was preserved: {}",
                        path.display()
                    ));
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.to_string()),
            }
        }
        namespace_guard()?;
        Ok(removed)
    }

    pub(crate) fn recover_stale_temporary_files_with_policy(
        &self,
        temporary_prefix: &str,
        max_entries: usize,
        max_name_bytes: usize,
        reject_obscured_live_writer: bool,
    ) -> Result<usize, String> {
        let mut previous_targets = Vec::new();
        let mut temporary_names = Vec::new();
        self.for_each_entry_bounded(max_entries, max_name_bytes, |name| {
            if let Some(target) = parse_previous_artifact_name(&name, temporary_prefix) {
                previous_targets.push(target);
            } else if is_deterministic_artifact_name(&name, temporary_prefix, ".tmp") {
                temporary_names.push(name);
            }
            Ok(())
        })?;

        let mut removed = 0_usize;
        for target_name in previous_targets {
            let target = self.path.join(&target_name);
            let temporary = self.path.join(deterministic_artifact_name(
                temporary_prefix,
                target_name.as_encoded_bytes(),
                ".tmp",
            ));
            let previous = self.path.join(deterministic_previous_artifact_name(
                temporary_prefix,
                &target_name,
            )?);
            if self.recover_atomic_transaction(
                &target,
                &temporary,
                &previous,
                true,
                reject_obscured_live_writer,
            )? {
                removed = removed.saturating_add(1);
            }
        }

        for name in temporary_names {
            let path = self.path.join(&name);
            if !self.path_exists(&path)? {
                continue;
            }
            let file = match self.open_read_write(&path) {
                Ok(file) => file,
                Err(_) if !self.path_exists(&path)? => continue,
                Err(error) => return Err(error),
            };
            match file.try_lock() {
                Ok(()) => {
                    self.remove_visible_file_if_matches(&path, &file, || Ok(()))?;
                    removed = removed.saturating_add(1);
                }
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(format!(
                        "failed to inspect temporary state ownership {}: {error}",
                        path.display()
                    ))
                }
            }
        }
        Ok(removed)
    }

    pub(crate) fn is_atomic_transaction_artifact_name(
        name: &std::ffi::OsStr,
        temporary_prefix: &str,
    ) -> bool {
        parse_previous_artifact_name(name, temporary_prefix).is_some()
            || is_deterministic_artifact_name(name, temporary_prefix, ".tmp")
    }

    pub(crate) fn atomic_previous_target_name(
        name: &std::ffi::OsStr,
        temporary_prefix: &str,
    ) -> Option<OsString> {
        parse_previous_artifact_name(name, temporary_prefix)
    }

    pub(crate) fn restore_exact_previous_artifact_with_guard(
        &self,
        target: &Path,
        previous: &Path,
        expected_bytes: &[u8],
        mut namespace_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        namespace_guard()?;
        if self.path_exists(target)? {
            return Err(format!(
                "cannot restore prior atomic state over an existing target: {}",
                target.display()
            ));
        }
        let previous_file = self.open_read_write(previous)?;
        let observed = read_open_file_prefix(&previous_file, expected_bytes.len() + 1)
            .map_err(|error| error.to_string())?;
        if observed != expected_bytes {
            return Err(format!(
                "prior atomic state bytes changed and were preserved: {}",
                previous.display()
            ));
        }
        self.move_open_file_no_replace_bound_guarded(
            previous,
            &previous_file,
            target,
            &mut namespace_guard,
        )?;
        namespace_guard()?;
        self.sync_directory()?;
        namespace_guard()
    }

    /// Recovers artifacts from an atomic publication whose destination was
    /// required to be missing. The caller supplies the exact planned bytes and
    /// retains the surrounding transaction authority. Any prior artifact,
    /// live writer, byte mismatch, or canonical/temporary ambiguity is
    /// preserved and rejected.
    pub(crate) fn recover_exact_missing_publication_with_guard(
        &self,
        target: &Path,
        temporary_prefix: &str,
        expected_bytes: &[u8],
        mut namespace_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let relative = self.relative_file(target)?;
        let temporary = self.path.join(deterministic_artifact_name(
            temporary_prefix,
            relative.as_os_str().as_encoded_bytes(),
            ".tmp",
        ));
        let previous = self.path.join(deterministic_previous_artifact_name(
            temporary_prefix,
            relative.as_os_str(),
        )?);
        namespace_guard()?;
        if self.path_exists(&previous)? {
            return Err(format!(
                "missing-only atomic publication has an ambiguous prior artifact; it was preserved: {}",
                previous.display()
            ));
        }
        let target_exists = self.path_exists(target)?;
        let temporary_exists = self.path_exists(&temporary)?;
        if temporary_exists {
            let temporary_file = self.open_read_write(&temporary)?;
            match temporary_file.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(format!(
                        "temporary state file is still owned by a live writer: {}",
                        temporary.display()
                    ));
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(format!(
                        "failed to inspect temporary state ownership {}: {error}",
                        temporary.display()
                    ));
                }
            }
            let observed = read_open_file_prefix(&temporary_file, expected_bytes.len() + 1)
                .map_err(|error| error.to_string())?;
            if observed != expected_bytes {
                return Err(format!(
                    "temporary atomic publication bytes are ambiguous and were preserved: {}",
                    temporary.display()
                ));
            }
            if target_exists {
                let target_file = self.open_read_write(target)?;
                let target_bytes = read_open_file_prefix(&target_file, expected_bytes.len() + 1)
                    .map_err(|error| error.to_string())?;
                if target_bytes != expected_bytes
                    || !same_open_file_identity(&target_file, &temporary_file)?
                {
                    return Err(format!(
                        "atomic publication target and temporary artifact do not identify the same exact publication; both were preserved: {}",
                        target.display()
                    ));
                }
                self.verify_file_identity(target, &target_file)?;
            }
            namespace_guard()?;
            self.remove_visible_file_if_matches_direct_with_guard(
                &temporary,
                &temporary_file,
                &mut namespace_guard,
            )?;
        }
        namespace_guard()?;
        if self.path_exists(target)? {
            let target_file = self.open_read_write(target)?;
            let observed = read_open_file_prefix(&target_file, expected_bytes.len() + 1)
                .map_err(|error| error.to_string())?;
            if observed != expected_bytes {
                return Err(format!(
                    "atomic publication target bytes are ambiguous and were preserved: {}",
                    target.display()
                ));
            }
            self.verify_file_identity(target, &target_file)?;
        }
        namespace_guard()
    }

    pub(crate) fn recover_atomic_transaction(
        &self,
        target: &Path,
        temporary: &Path,
        previous: &Path,
        skip_live_writer: bool,
        reject_obscured_live_writer: bool,
    ) -> Result<bool, String> {
        self.recover_atomic_transaction_guarded(
            target,
            temporary,
            previous,
            skip_live_writer,
            reject_obscured_live_writer,
            &mut || Ok(()),
        )
    }

    pub(crate) fn recover_atomic_transaction_guarded(
        &self,
        target: &Path,
        temporary: &Path,
        previous: &Path,
        skip_live_writer: bool,
        reject_obscured_live_writer: bool,
        namespace_guard: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<bool, String> {
        let mut previous_open_hook = || Ok(());
        let mut live_target_hook = || {};
        self.recover_atomic_transaction_with_hooks_guarded(
            target,
            temporary,
            previous,
            skip_live_writer,
            reject_obscured_live_writer,
            AtomicRecoveryHooks {
                previous_open: &mut previous_open_hook,
                live_target: &mut live_target_hook,
            },
            namespace_guard,
        )
    }

    #[cfg(test)]
    pub(crate) fn recover_atomic_transaction_with_live_target_hook(
        &self,
        target: &Path,
        temporary: &Path,
        previous: &Path,
        skip_live_writer: bool,
        reject_obscured_live_writer: bool,
        live_target_hook: &mut impl FnMut(),
    ) -> Result<bool, String> {
        let mut previous_open_hook = || Ok(());
        self.recover_atomic_transaction_with_hooks(
            target,
            temporary,
            previous,
            skip_live_writer,
            reject_obscured_live_writer,
            AtomicRecoveryHooks {
                previous_open: &mut previous_open_hook,
                live_target: live_target_hook,
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn recover_atomic_transaction_with_hooks(
        &self,
        target: &Path,
        temporary: &Path,
        previous: &Path,
        skip_live_writer: bool,
        reject_obscured_live_writer: bool,
        hooks: AtomicRecoveryHooks<'_>,
    ) -> Result<bool, String> {
        self.recover_atomic_transaction_with_hooks_guarded(
            target,
            temporary,
            previous,
            skip_live_writer,
            reject_obscured_live_writer,
            hooks,
            &mut || Ok(()),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn recover_atomic_transaction_with_hooks_guarded(
        &self,
        target: &Path,
        temporary: &Path,
        previous: &Path,
        skip_live_writer: bool,
        reject_obscured_live_writer: bool,
        mut hooks: AtomicRecoveryHooks<'_>,
        namespace_guard: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<bool, String> {
        const MAX_NAMESPACE_RETRIES: usize = 8;

        let policy = AtomicRecoveryPolicy {
            skip_live_writer,
            reject_obscured_live_writer,
            strict_deadline: reject_obscured_live_writer
                .then(|| Instant::now() + STRICT_RECOVERY_LIVE_WRITER_WAIT),
        };
        for attempt in 0..MAX_NAMESPACE_RETRIES {
            let observed = (
                self.path_exists(target)?,
                self.path_exists(temporary)?,
                self.path_exists(previous)?,
            );
            match self.recover_atomic_transaction_once(
                target,
                temporary,
                previous,
                policy,
                &mut hooks,
                namespace_guard,
            ) {
                Ok(recovered) => return Ok(recovered),
                Err(error) => {
                    let current = (
                        self.path_exists(target)?,
                        self.path_exists(temporary)?,
                        self.path_exists(previous)?,
                    );
                    if current == observed {
                        return Err(error);
                    }
                    if attempt + 1 == MAX_NAMESPACE_RETRIES {
                        return Err(format!(
                            "atomic state namespace changed repeatedly while recovering {}: {error}",
                            target.display()
                        ));
                    }
                }
            }
        }
        unreachable!("bounded atomic recovery loop always returns")
    }

    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
    pub(crate) fn recover_atomic_transaction_once(
        &self,
        target: &Path,
        temporary: &Path,
        previous: &Path,
        policy: AtomicRecoveryPolicy,
        hooks: &mut AtomicRecoveryHooks<'_>,
        namespace_guard: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<bool, String> {
        namespace_guard()?;
        let AtomicRecoveryPolicy {
            skip_live_writer,
            reject_obscured_live_writer,
            strict_deadline,
        } = policy;
        let temporary_file = if self.path_exists(temporary)? {
            let file = self.open_read_write(temporary)?;
            match file.try_lock() {
                Ok(()) => Some(file),
                Err(std::fs::TryLockError::WouldBlock) if skip_live_writer => {
                    if !reject_obscured_live_writer
                        || (!self.path_exists(target)? && !self.path_exists(previous)?)
                    {
                        return Ok(false);
                    }

                    let deadline = strict_deadline
                        .expect("strict live-writer rejection carries a recovery deadline");
                    loop {
                        let now = Instant::now();
                        if now >= deadline {
                            return Err(format!(
                                "atomic state transaction is still owned by a live writer and may obscure its target: {}",
                                target.display()
                            ));
                        }
                        thread::sleep(STRICT_RECOVERY_POLL_INTERVAL.min(deadline - now));
                        match file.try_lock() {
                            Ok(()) => break Some(file),
                            Err(std::fs::TryLockError::WouldBlock) => {}
                            Err(std::fs::TryLockError::Error(error)) => {
                                return Err(format!(
                                    "failed while waiting for temporary state ownership {}: {error}",
                                    temporary.display()
                                ));
                            }
                        }
                    }
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(format!(
                        "temporary state file is still owned by a live writer: {}",
                        temporary.display()
                    ))
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(format!(
                        "failed to inspect temporary state ownership {}: {error}",
                        temporary.display()
                    ))
                }
            }
        } else {
            None
        };

        let mut recovered = false;
        if self.path_exists(previous)? {
            (hooks.previous_open)()?;
            let previous_file = self.open_read_write(previous)?;
            if self.path_exists(target)? {
                let target_file = self.open_read_write(target)?;
                if same_open_file_identity(&target_file, &previous_file)? {
                    self.remove_visible_file_if_matches_guarded(
                        previous,
                        &previous_file,
                        namespace_guard,
                        || Ok(()),
                    )?;
                    recovered = true;
                } else {
                    match target_file.try_lock() {
                        Ok(()) => {
                            self.verify_file_identity(target, &target_file)?;
                            if !self.path_exists(previous)? {
                                return Ok(false);
                            }
                            self.verify_file_identity(previous, &previous_file)?;
                        }
                        Err(std::fs::TryLockError::WouldBlock) if skip_live_writer => {
                            (hooks.live_target)();
                            if !reject_obscured_live_writer {
                                return Ok(false);
                            }

                            let deadline = strict_deadline
                                .expect("strict live-writer rejection carries a recovery deadline");
                            loop {
                                let now = Instant::now();
                                if now >= deadline {
                                    return Err(format!(
                                        "atomic state target is still owned by a live writer: {}",
                                        target.display()
                                    ));
                                }
                                thread::sleep(STRICT_RECOVERY_POLL_INTERVAL.min(deadline - now));
                                match target_file.try_lock() {
                                    Ok(()) => break,
                                    Err(std::fs::TryLockError::WouldBlock) => {}
                                    Err(std::fs::TryLockError::Error(error)) => {
                                        return Err(format!(
                                            "failed while waiting for published state ownership {}: {error}",
                                            target.display()
                                        ));
                                    }
                                }
                            }

                            self.verify_file_identity(target, &target_file)?;
                            if !self.path_exists(previous)? {
                                return Ok(false);
                            }
                            self.verify_file_identity(previous, &previous_file)?;
                        }
                        Err(std::fs::TryLockError::WouldBlock) => {
                            return Err(format!(
                                "atomic state target is still owned by a live writer: {}",
                                target.display()
                            ));
                        }
                        Err(std::fs::TryLockError::Error(error)) => {
                            return Err(format!(
                                "failed to inspect published state ownership {}: {error}",
                                target.display()
                            ));
                        }
                    }
                    return Err(format!(
                        "atomic state target and an ambiguous prior artifact both exist; both were preserved: {}",
                        target.display()
                    ));
                }
            } else {
                return Err(format!(
                    "atomic state target is missing while an unproven prior artifact exists; the artifact was preserved: {}",
                    target.display()
                ));
            }
        }

        if let Some(file) = temporary_file {
            if self.path_exists(temporary)? {
                match self.verify_file_identity(temporary, &file) {
                    Ok(()) => {
                        self.remove_visible_file_if_matches_guarded(
                            temporary,
                            &file,
                            namespace_guard,
                            || Ok(()),
                        )?;
                        recovered = true;
                    }
                    Err(error) => {
                        return Err(format!(
                            "temporary state path was substituted during recovery and was preserved: {error}"
                        ))
                    }
                }
            }
        }
        Ok(recovered)
    }

    pub(crate) fn rollback_previous_file_guarded(
        &self,
        target: &Path,
        previous: &Path,
        expected: &File,
        namespace_guard: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if !self.path_exists(previous)? {
            return Ok(());
        }
        self.verify_file_identity(previous, expected)?;
        if self.path_exists(target)? {
            let visible_target = self.open_read(target)?;
            match same_open_file_identity(&visible_target, expected) {
                Ok(true) => self.remove_bound_file_if_matches_guarded(
                    previous,
                    expected,
                    namespace_guard,
                    || Ok(()),
                ),
                Ok(false) => Err(format!(
                    "atomic state target and evacuated prior state are identity-distinct; both were preserved as ambiguous: {} and {}",
                    target.display(),
                    previous.display()
                )),
                Err(error) => Err(format!(
                    "atomic state target identity could not be compared with the evacuated prior state; both were preserved: {error}"
                )),
            }
        } else {
            namespace_guard()?;
            self.move_open_file_no_replace_bound_guarded(
                previous,
                expected,
                target,
                namespace_guard,
            )
        }
    }

    #[cfg(test)]
    pub(crate) fn cleanup_open_temporary_file(
        &self,
        path: &Path,
        expected: &File,
        before_delete: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        if !self.path_exists(path)? {
            return Ok(());
        }
        match self.verify_file_identity(path, expected) {
            Ok(()) => self.remove_bound_file_if_matches(path, expected, before_delete),
            Err(error) => Err(format!(
                "temporary state path was substituted and was preserved: {error}"
            )),
        }
    }

    pub(crate) fn cleanup_open_temporary_file_guarded(
        &self,
        path: &Path,
        expected: &File,
        namespace_guard: &mut impl FnMut() -> Result<(), String>,
        before_delete: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        if !self.path_exists(path)? {
            return Ok(());
        }
        match self.verify_file_identity(path, expected) {
            Ok(()) => self.remove_bound_file_if_matches_guarded(
                path,
                expected,
                namespace_guard,
                before_delete,
            ),
            Err(error) => Err(format!(
                "temporary state path was substituted and was preserved: {error}"
            )),
        }
    }

    pub(crate) fn sync_directory(&self) -> Result<(), String> {
        #[cfg(unix)]
        {
            let mut options = cap_std::fs::OpenOptions::new();
            options.read(true);
            self.directory
                .open_with(".", &options)
                .map(cap_std::fs::File::into_std)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| format!("failed to sync {}: {error}", self.path.display()))
        }
        #[cfg(not(unix))]
        {
            Ok(())
        }
    }

    pub(crate) fn finalize_failed_exact_publication(
        &self,
        path: &Path,
        previous_expected: Option<&File>,
        receipt: &FilePublicationReceipt,
        temporary_prefix: &str,
        expected_bytes: &[u8],
    ) -> Result<(), String> {
        self.finalize_failed_exact_publication_with_guard(
            path,
            previous_expected,
            receipt,
            temporary_prefix,
            expected_bytes,
            &mut || Ok(()),
        )
    }

    pub(crate) fn finalize_failed_exact_publication_with_guard(
        &self,
        path: &Path,
        previous_expected: Option<&File>,
        receipt: &FilePublicationReceipt,
        temporary_prefix: &str,
        expected_bytes: &[u8],
        namespace_guard: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        namespace_guard()?;
        if !receipt.exact_identity {
            return Err("state publication does not carry an exact identity receipt".to_string());
        }
        let destination = self.relative_file(path)?;
        let temporary = self.path.join(deterministic_artifact_name(
            temporary_prefix,
            destination.as_os_str().as_encoded_bytes(),
            ".tmp",
        ));
        let previous = self.path.join(deterministic_previous_artifact_name(
            temporary_prefix,
            destination.as_os_str(),
        )?);

        self.verify_visible()?;
        namespace_guard()?;
        self.verify_exact_publication_bytes(path, receipt, expected_bytes)?;
        namespace_guard()?;
        self.sync_directory()?;
        if self.path_exists(&previous)? {
            let previous_expected = previous_expected.ok_or_else(|| {
                format!(
                    "unexpected prior-state artifact was preserved while finalizing {}",
                    path.display()
                )
            })?;
            self.verify_file_identity(&previous, previous_expected)?;
            self.remove_bound_file_if_matches_guarded(
                &previous,
                previous_expected,
                namespace_guard,
                || self.verify_exact_publication_bytes(path, receipt, expected_bytes),
            )?;
        }
        if self.path_exists(&temporary)? {
            self.verify_file_identity(&temporary, &receipt.file)?;
            self.remove_bound_file_if_matches_guarded(
                &temporary,
                &receipt.file,
                namespace_guard,
                || self.verify_exact_publication_bytes(path, receipt, expected_bytes),
            )?;
        }
        namespace_guard()?;
        self.sync_directory()?;
        namespace_guard()?;
        self.verify_exact_publication_bytes(path, receipt, expected_bytes)?;
        namespace_guard()?;
        self.verify_visible()
    }

    pub(crate) fn verify_exact_publication_bytes(
        &self,
        path: &Path,
        receipt: &FilePublicationReceipt,
        expected_bytes: &[u8],
    ) -> Result<(), String> {
        self.verify_publication_bytes(path, &receipt.file, expected_bytes)
    }

    pub(crate) fn verify_publication_bytes(
        &self,
        path: &Path,
        expected_file: &File,
        expected_bytes: &[u8],
    ) -> Result<(), String> {
        self.verify_file_identity(path, expected_file)?;
        let actual = read_open_file_prefix(expected_file, expected_bytes.len().saturating_add(1))
            .map_err(|error| {
            format!(
                "failed to read state publication while finalizing {}: {error}",
                path.display()
            )
        })?;
        if actual != expected_bytes {
            return Err(format!(
                "state publication bytes changed while finalizing {}",
                path.display()
            ));
        }
        self.verify_file_identity(path, expected_file)
    }

    pub(crate) fn verify_file_identity(&self, path: &Path, file: &File) -> Result<(), String> {
        let expected = crate::fs_security::FileIdentity::from_file(
            file.try_clone()
                .map_err(|error| format!("failed to clone {}: {error}", path.display()))?,
        )
        .map_err(|error| format!("failed to identify {}: {error}", path.display()))?;
        let reopened = self.open_read_unchecked(path)?;
        let actual = crate::fs_security::FileIdentity::from_file(reopened)
            .map_err(|error| format!("failed to identify {}: {error}", path.display()))?;
        if actual != expected {
            return Err(format!(
                "state file identity changed while it was in use: {}",
                path.display()
            ));
        }
        Ok(())
    }

    pub(crate) fn open_read_unchecked(&self, path: &Path) -> Result<File, String> {
        let relative = self.relative_file(path)?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true);
        configure_capability_no_follow(&mut options);
        let file = self
            .directory
            .open_with(relative, &options)
            .map(cap_std::fs::File::into_std)
            .map_err(|error| format!("failed to re-open {}: {error}", path.display()))?;
        validate_stable_file_metadata(path, &file.metadata().map_err(|error| error.to_string())?)?;
        Ok(file)
    }

    pub(crate) fn deterministic_artifact_path(
        &self,
        path: &Path,
        prefix: &str,
        suffix: &str,
    ) -> Result<PathBuf, String> {
        let relative = self.relative_file(path)?;
        Ok(self.path.join(deterministic_artifact_name(
            prefix,
            relative.as_os_str().as_encoded_bytes(),
            suffix,
        )))
    }

    pub(crate) fn deterministic_previous_artifact_path(
        &self,
        path: &Path,
        prefix: &str,
    ) -> Result<PathBuf, String> {
        let relative = self.relative_file(path)?;
        Ok(self.path.join(deterministic_previous_artifact_name(
            prefix,
            relative.as_os_str(),
        )?))
    }

    pub(crate) fn restore_visible_file_no_replace_if_matches(
        &self,
        source: &Path,
        expected: &File,
        destination: &Path,
    ) -> Result<(), String> {
        self.move_open_file_no_replace_bound(source, expected, destination)
    }

    pub(crate) fn restore_visible_file_no_replace_if_matches_with_guard(
        &self,
        source: &Path,
        expected: &File,
        destination: &Path,
        mut namespace_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.move_open_file_no_replace_bound_guarded(
            source,
            expected,
            destination,
            &mut namespace_guard,
        )
    }

    pub(crate) fn relative_file<'a>(&self, path: &'a Path) -> Result<&'a Path, String> {
        let Some(parent) = path.parent() else {
            return Err(format!(
                "state path is not a direct child of the opened directory: {}",
                path.display()
            ));
        };
        let Some(file_name) = path.file_name() else {
            return Err(format!(
                "state path is not a direct child of the opened directory: {}",
                path.display()
            ));
        };
        if parent != self.path {
            #[cfg(not(windows))]
            return Err(format!(
                "state path is not a direct child of the opened directory: {}",
                path.display()
            ));
            #[cfg(windows)]
            {
                let requested =
                    crate::fs_security::canonicalize_existing_directory_without_symlinks(parent)
                        .map_err(|_| {
                            format!(
                                "state path is not a direct child of the opened directory: {}",
                                path.display()
                            )
                        })?;
                let retained =
                    crate::fs_security::canonicalize_existing_directory_without_symlinks(
                        &self.path,
                    )
                    .map_err(|_| {
                        format!(
                            "state path is not a direct child of the opened directory: {}",
                            path.display()
                        )
                    })?;
                if requested != retained {
                    return Err(format!(
                        "state path is not a direct child of the opened directory: {}",
                        path.display()
                    ));
                }
                self.verify_visible().map_err(|_| {
                    format!(
                        "state path is not a direct child of the opened directory: {}",
                        path.display()
                    )
                })?;
            }
        }
        Ok(Path::new(file_name))
    }
}
