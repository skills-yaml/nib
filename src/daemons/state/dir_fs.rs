//! T043 split.

use super::*;

impl StableDirectory {
    pub(crate) fn rename_child_directory_until_with_hooks(
        &self,
        source: &Path,
        expected: &Self,
        destination: &Path,
        deadline: Instant,
        mut before_publication: impl FnMut() -> Result<(), String>,
        after_capability_acquisition: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        ensure_directory_namespace_deadline(deadline, source)?;
        #[cfg(windows)]
        if !expected.delete_capable {
            return Err(format!(
                "state directory lacks the retained DELETE capability required for rename: {}",
                source.display()
            ));
        }
        if self.entry_kind(source)? != Some(StableEntryKind::Directory) {
            return Err(format!(
                "state directory does not exist or is not local: {}",
                source.display()
            ));
        }
        ensure_directory_namespace_deadline(deadline, source)?;
        if self.entry_kind(destination)?.is_some() {
            return Err(format!(
                "state directory quarantine already exists: {}",
                destination.display()
            ));
        }
        ensure_directory_namespace_deadline(deadline, destination)?;
        self.verify_visible()?;
        ensure_directory_namespace_deadline(deadline, source)?;
        expected.verify_visible_at(source)?;
        ensure_directory_namespace_deadline(deadline, source)?;
        let source_relative = self.relative_file(source)?;
        let destination_relative = self.relative_file(destination)?;
        ensure_directory_namespace_deadline(deadline, source)?;
        before_publication()?;
        ensure_directory_namespace_deadline(deadline, source)?;
        #[cfg(windows)]
        let mutation_directory = self.open_directory_for_mutation(source, expected)?;
        after_capability_acquisition()?;
        ensure_directory_namespace_deadline(deadline, source)?;

        // Publication and its parent-directory sync are one indivisible boundary:
        // there is intentionally no pausable/user callback after the rename. If
        // the syscall itself crosses the deadline, the post-sync check rejects
        // success while the exact, identity-bound directory remains recoverable.
        rename_open_directory_no_replace_platform(
            &self.directory,
            source_relative,
            #[cfg(not(windows))]
            &expected.directory,
            #[cfg(windows)]
            &mutation_directory,
            destination_relative,
        )
        .map_err(|error| {
            format!(
                "failed to publish state directory {}: {error}",
                source.display()
            )
        })?;
        self.sync_directory()?;
        ensure_directory_namespace_deadline(deadline, destination)?;
        expected.verify_visible_at(destination)?;
        ensure_directory_namespace_deadline(deadline, destination)?;
        self.verify_visible()?;
        ensure_directory_namespace_deadline(deadline, destination)
    }

    pub(crate) fn remove_empty_child_directory_if_matches(
        &self,
        path: &Path,
        expected: Self,
    ) -> Result<(), String> {
        self.remove_empty_child_directory_if_matches_with_guard(path, expected, || Ok(()))
    }

    pub(crate) fn remove_empty_child_directory_if_matches_with_guard(
        &self,
        path: &Path,
        expected: Self,
        guard: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.remove_empty_child_directory_if_matches_with_guard_and_hook(
            path,
            expected,
            guard,
            || Ok(()),
        )
    }

    pub(crate) fn remove_empty_child_directory_if_matches_with_guard_and_hook(
        &self,
        path: &Path,
        expected: Self,
        mut guard: impl FnMut() -> Result<(), String>,
        after_capability_acquisition: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        guard()?;
        #[cfg(windows)]
        if !expected.delete_capable {
            return Err(format!(
                "state directory lacks the retained DELETE capability required for removal: {}",
                path.display()
            ));
        }
        self.verify_visible()?;
        guard()?;
        self.relative_file(path)?;
        expected.verify_visible_at(path)?;
        guard()?;
        #[cfg(windows)]
        {
            let path_bound = self.open_directory_for_mutation(path, &expected)?;
            after_capability_acquisition()?;
            drop(expected);
            let path_bound = path_bound.into_std_file();
            guard()?;
            delete_open_file_platform(&path_bound).map_err(|error| {
                format!(
                    "failed to remove the expected open state directory {}: {error}",
                    path.display()
                )
            })?;
            drop(path_bound);
        }
        #[cfg(not(windows))]
        {
            after_capability_acquisition()?;
            drop(expected);
            let relative = self.relative_file(path)?;
            guard()?;
            self.directory
                .remove_dir(relative)
                .map_err(|error| format!("failed to remove {}: {error}", path.display()))?;
        }
        self.sync_directory()?;
        guard()?;
        if self.entry_kind(path)?.is_some() {
            return Err(format!(
                "state directory reappeared during removal; replacement preserved: {}",
                path.display()
            ));
        }
        self.verify_visible()?;
        guard()
    }

    #[cfg(windows)]
    pub(crate) fn open_directory_for_mutation(
        &self,
        path: &Path,
        expected: &Self,
    ) -> Result<cap_std::fs::Dir, String> {
        let relative = self.relative_file(path)?;
        let file =
            crate::fs_security::open_directory_child_windows(&self.directory, relative, true)
                .map_err(|error| {
                    format!(
                        "failed to acquire directory mutation capability {}: {error}",
                        path.display()
                    )
                })?;
        let identity =
            crate::fs_security::FileIdentity::from_file(file.try_clone().map_err(|error| {
                format!("failed to clone directory mutation capability: {error}")
            })?)
            .map_err(|error| format!("failed to identify directory {}: {error}", path.display()))?;
        if identity != expected.identity {
            return Err(format!(
                "state directory identity changed before mutation: {}",
                path.display()
            ));
        }
        Ok(cap_std::fs::Dir::from_std_file(file))
    }

    pub(crate) fn rename_file_if_matches(
        &self,
        source: &Path,
        destination: &Path,
        expected: &File,
    ) -> Result<(), String> {
        if self.path_exists(destination)? {
            return Err(format!(
                "state destination already exists: {}",
                destination.display()
            ));
        }
        self.verify_visible()?;
        self.verify_file_identity(source, expected)?;
        self.move_open_file_no_replace(source, expected, destination)
    }

    pub(crate) fn move_open_file_no_replace(
        &self,
        source: &Path,
        expected: &File,
        destination: &Path,
    ) -> Result<(), String> {
        self.verify_visible()?;
        self.move_open_file_no_replace_bound(source, expected, destination)?;
        self.verify_visible()
    }

    pub(crate) fn move_open_file_no_replace_bound(
        &self,
        source: &Path,
        expected: &File,
        destination: &Path,
    ) -> Result<(), String> {
        self.move_open_file_no_replace_bound_with_hook(source, expected, destination, || Ok(()))
    }

    pub(crate) fn move_open_file_no_replace_bound_with_hook(
        &self,
        source: &Path,
        expected: &File,
        destination: &Path,
        after_identity_check: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        #[cfg(unix)]
        {
            self.verify_file_identity(source, expected)?;
            after_identity_check()?;
            rename_file_no_replace_platform(
                &self.directory,
                self.relative_file(source)?,
                self.relative_file(destination)?,
            )
            .map_err(|error| {
                format!(
                    "failed to move state file {} without replacing {}: {error}",
                    source.display(),
                    destination.display()
                )
            })?;

            let moved = match self.open_read(destination) {
                Ok(moved) => moved,
                Err(error) => {
                    let rescue = self.rescue_moved_file(destination, source);
                    return Err(match rescue {
                        Ok(()) => format!(
                            "moved state file could not be verified and was restored to {}: {error}",
                            source.display()
                        ),
                        Err(rescue_error) => format!(
                            "moved state file could not be verified; all visible entries were preserved: {error}; rescue failed: {rescue_error}"
                        ),
                    });
                }
            };
            let identity_failure = match same_open_file_identity(&moved, expected) {
                Ok(true) => None,
                Ok(false) => Some("state source changed while it was moved".to_string()),
                Err(error) => Some(format!(
                    "moved state file identity could not be verified: {error}"
                )),
            };
            if let Some(identity_failure) = identity_failure {
                let rescue = self.rescue_moved_file(destination, source);
                return Err(match rescue {
                    Ok(()) => format!(
                        "{identity_failure} and the unverified file was restored to {}",
                        source.display()
                    ),
                    Err(rescue_error) => format!(
                        "{identity_failure}; all visible entries were preserved because rescue failed: {rescue_error}"
                    ),
                });
            }
            if self.path_exists(source)? {
                return Err(format!(
                    "state source path reappeared after its expected file was moved; both entries were preserved: {}",
                    source.display()
                ));
            }
            self.sync_directory()
        }

        #[cfg(windows)]
        {
            self.verify_file_identity(source, expected)?;
            let source_bound = self.open_read(source)?;
            if !same_open_file_identity(&source_bound, expected)? {
                return Err(format!(
                    "state source changed before its path-bound handle was retained: {}",
                    source.display()
                ));
            }
            after_identity_check()?;
            let publication =
                self.publish_open_file_no_replace(source, &source_bound, destination)?;
            self.verify_published_file(destination, &source_bound, publication)?;
            if self.path_exists(source)? {
                return Err(format!(
                    "state source path reappeared while its open file was moved: {}",
                    source.display()
                ));
            }
            self.sync_directory()
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = (source, expected, destination, after_identity_check);
            Err("this platform has no safe state-file relocation primitive".to_string())
        }
    }

    pub(crate) fn move_open_file_no_replace_bound_guarded(
        &self,
        source: &Path,
        expected: &File,
        destination: &Path,
        namespace_guard: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        #[cfg(unix)]
        {
            self.verify_file_identity(source, expected)?;
            namespace_guard()?;
            rename_file_no_replace_platform(
                &self.directory,
                self.relative_file(source)?,
                self.relative_file(destination)?,
            )
            .map_err(|error| {
                format!(
                    "failed to move state file {} without replacing {}: {error}",
                    source.display(),
                    destination.display()
                )
            })?;

            let moved = match self.open_read(destination) {
                Ok(moved) => moved,
                Err(error) => {
                    let rescue =
                        self.rescue_moved_file_guarded(destination, source, namespace_guard);
                    return Err(match rescue {
                        Ok(()) => format!(
                            "moved state file could not be verified and was restored to {}: {error}",
                            source.display()
                        ),
                        Err(rescue_error) => format!(
                            "moved state file could not be verified; all visible entries were preserved: {error}; rescue failed: {rescue_error}"
                        ),
                    });
                }
            };
            let identity_failure = match same_open_file_identity(&moved, expected) {
                Ok(true) => None,
                Ok(false) => Some("state source changed while it was moved".to_string()),
                Err(error) => Some(format!(
                    "moved state file identity could not be verified: {error}"
                )),
            };
            if let Some(identity_failure) = identity_failure {
                let rescue = self.rescue_moved_file_guarded(destination, source, namespace_guard);
                return Err(match rescue {
                    Ok(()) => format!(
                        "{identity_failure} and the unverified file was restored to {}",
                        source.display()
                    ),
                    Err(rescue_error) => format!(
                        "{identity_failure}; all visible entries were preserved because rescue failed: {rescue_error}"
                    ),
                });
            }
            if self.path_exists(source)? {
                return Err(format!(
                    "state source path reappeared after its expected file was moved; both entries were preserved: {}",
                    source.display()
                ));
            }
            namespace_guard()?;
            self.sync_directory()
        }

        #[cfg(windows)]
        {
            self.verify_file_identity(source, expected)?;
            let source_bound = self.open_read(source)?;
            if !same_open_file_identity(&source_bound, expected)? {
                return Err(format!(
                    "state source changed before its path-bound handle was retained: {}",
                    source.display()
                ));
            }
            namespace_guard()?;
            let publication = self.publish_open_file_no_replace_guarded(
                source,
                &source_bound,
                destination,
                namespace_guard,
            )?;
            self.verify_published_file(destination, &source_bound, publication)?;
            if self.path_exists(source)? {
                return Err(format!(
                    "state source path reappeared while its open file was moved: {}",
                    source.display()
                ));
            }
            namespace_guard()?;
            self.sync_directory()
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = (source, expected, destination, namespace_guard);
            Err("this platform has no safe state-file relocation primitive".to_string())
        }
    }

    #[cfg(unix)]
    pub(crate) fn rescue_moved_file(
        &self,
        source: &Path,
        destination: &Path,
    ) -> Result<(), String> {
        rename_file_no_replace_platform(
            &self.directory,
            self.relative_file(source)?,
            self.relative_file(destination)?,
        )
        .map_err(|error| {
            format!(
                "failed to restore moved state file {} to {}: {error}",
                source.display(),
                destination.display()
            )
        })?;
        self.sync_directory()
    }

    #[cfg(unix)]
    pub(crate) fn rescue_moved_file_guarded(
        &self,
        source: &Path,
        destination: &Path,
        namespace_guard: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        namespace_guard()?;
        rename_file_no_replace_platform(
            &self.directory,
            self.relative_file(source)?,
            self.relative_file(destination)?,
        )
        .map_err(|error| {
            format!(
                "failed to restore moved state file {} to {}: {error}",
                source.display(),
                destination.display()
            )
        })?;
        namespace_guard()?;
        self.sync_directory()
    }

    pub(crate) fn verify_published_file(
        &self,
        destination: &Path,
        expected: &File,
        publication: HandlePublication,
    ) -> Result<(), String> {
        let _ = publication;
        self.verify_file_identity(destination, expected)
    }

    #[cfg(windows)]
    pub(crate) fn publish_open_file_no_replace(
        &self,
        source: &Path,
        expected: &File,
        destination: &Path,
    ) -> Result<HandlePublication, String> {
        let destination_relative = self.relative_file(destination)?;
        let source_relative = self.relative_file(source)?;
        let publication = publish_open_file_no_replace_platform(
            &self.directory,
            source_relative,
            expected,
            destination_relative,
        )
        .map_err(|error| {
            format!(
                "failed to publish the open state file without replacing {}: {error}",
                destination.display()
            )
        })?;

        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if let Err(identity_error) = self.verify_file_identity(destination, expected) {
            let rescue = self.open_read(destination).and_then(|published| {
                self.move_open_file_no_replace_bound(destination, &published, source)
            });
            return Err(match rescue {
                Ok(()) => format!(
                    "published state source changed before pathname-bound publication and the unverified file was restored to {}: {identity_error}",
                    source.display()
                ),
                Err(rescue_error) => format!(
                    "published state source changed before pathname-bound publication; all visible entries were preserved: {identity_error}; rescue failed: {rescue_error}"
                ),
            });
        }

        Ok(publication)
    }

    pub(crate) fn publish_open_file_no_replace_guarded(
        &self,
        source: &Path,
        expected: &File,
        destination: &Path,
        namespace_guard: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<HandlePublication, String> {
        let destination_relative = self.relative_file(destination)?;
        let source_relative = self.relative_file(source)?;
        namespace_guard()?;
        let publication = publish_open_file_no_replace_platform(
            &self.directory,
            source_relative,
            expected,
            destination_relative,
        )
        .map_err(|error| {
            format!(
                "failed to publish the open state file without replacing {}: {error}",
                destination.display()
            )
        })?;

        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if let Err(identity_error) = self.verify_file_identity(destination, expected) {
            let rescue = self.open_read(destination).and_then(|published| {
                self.move_open_file_no_replace_bound_guarded(
                    destination,
                    &published,
                    source,
                    namespace_guard,
                )
            });
            return Err(match rescue {
                Ok(()) => format!(
                    "published state source changed before pathname-bound publication and the unverified file was restored to {}: {identity_error}",
                    source.display()
                ),
                Err(rescue_error) => format!(
                    "published state source changed before pathname-bound publication; all visible entries were preserved: {identity_error}; rescue failed: {rescue_error}"
                ),
            });
        }

        Ok(publication)
    }

    pub(crate) fn remove_visible_file_if_matches(
        &self,
        path: &Path,
        expected: &File,
        before_delete: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.verify_visible()?;
        self.remove_bound_file_if_matches(path, expected, before_delete)?;
        self.verify_visible()
    }

    pub(crate) fn remove_bound_file_if_matches(
        &self,
        path: &Path,
        expected: &File,
        before_delete: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.remove_bound_file_if_matches_with_hook_after_identity(
            path,
            expected,
            before_delete,
            || Ok(()),
        )
    }

    pub(crate) fn remove_bound_file_if_matches_with_hook_after_identity(
        &self,
        path: &Path,
        expected: &File,
        before_delete: impl FnOnce() -> Result<(), String>,
        after_identity_check: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.verify_file_identity(path, expected)?;
        before_delete()?;
        #[cfg(windows)]
        let path_bound = {
            let path_bound = self.open_read(path)?;
            if !same_open_file_identity(&path_bound, expected)? {
                return Err(format!(
                    "state file changed before its path-bound deletion handle was retained: {}",
                    path.display()
                ));
            }
            path_bound
        };
        #[cfg(not(windows))]
        self.verify_file_identity(path, expected)?;
        after_identity_check()?;
        #[cfg(windows)]
        delete_open_file_platform(&path_bound).map_err(|error| {
            format!(
                "failed to remove the expected open state file {}: {error}",
                path.display()
            )
        })?;
        #[cfg(not(windows))]
        self.remove_file_bound_without_sync(path)?;
        self.sync_directory()
    }

    pub(crate) fn remove_bound_file_if_matches_guarded(
        &self,
        path: &Path,
        expected: &File,
        namespace_guard: &mut impl FnMut() -> Result<(), String>,
        before_delete: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.verify_file_identity(path, expected)?;
        before_delete()?;
        #[cfg(windows)]
        let path_bound = {
            let path_bound = self.open_read(path)?;
            if !same_open_file_identity(&path_bound, expected)? {
                return Err(format!(
                    "state file changed before its path-bound deletion handle was retained: {}",
                    path.display()
                ));
            }
            path_bound
        };
        #[cfg(not(windows))]
        self.verify_file_identity(path, expected)?;
        namespace_guard()?;
        #[cfg(windows)]
        delete_open_file_platform(&path_bound).map_err(|error| {
            format!(
                "failed to remove the expected open state file {}: {error}",
                path.display()
            )
        })?;
        #[cfg(not(windows))]
        self.remove_file_bound_without_sync(path)?;
        namespace_guard()?;
        self.sync_directory()
    }

    pub(crate) fn remove_visible_file_if_matches_guarded(
        &self,
        path: &Path,
        expected: &File,
        namespace_guard: &mut impl FnMut() -> Result<(), String>,
        before_delete: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.verify_visible()?;
        self.remove_bound_file_if_matches_guarded(path, expected, namespace_guard, before_delete)?;
        self.verify_visible()
    }

    pub(crate) fn for_each_entry_bounded(
        &self,
        max_entries: usize,
        max_name_bytes: usize,
        mut visit: impl FnMut(OsString) -> Result<(), String>,
    ) -> Result<(), String> {
        self.verify_visible()?;
        let mut entries = 0_usize;
        let mut name_bytes = 0_usize;
        for entry in self
            .directory
            .entries()
            .map_err(|error| format!("failed to list {}: {error}", self.path.display()))?
        {
            let name = entry
                .map_err(|error| format!("failed to list {}: {error}", self.path.display()))?
                .file_name();
            entries = entries
                .checked_add(1)
                .ok_or_else(|| "state directory entry count overflowed".to_string())?;
            name_bytes = name_bytes
                .checked_add(name.as_encoded_bytes().len())
                .ok_or_else(|| "state directory name byte count overflowed".to_string())?;
            if entries > max_entries || name_bytes > max_name_bytes {
                return Err(format!(
                    "state directory {} exceeds the bounded scan limit ({max_entries} entries, {max_name_bytes} filename bytes)",
                    self.path.display()
                ));
            }
            visit(name)?;
        }
        self.verify_visible()?;
        Ok(())
    }

    pub(crate) fn save_json_atomically_expected<T: Serialize>(
        &self,
        path: &Path,
        value: &T,
        expected: FileExpectation<'_>,
    ) -> Result<(), String> {
        let encoded = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
        self.save_bytes_atomically_expected(path, &encoded, ".nib-daemon-", expected)
    }

    #[cfg(test)]
    pub(crate) fn save_bytes_atomically(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
    ) -> Result<(), String> {
        self.save_bytes_atomically_expected(path, encoded, temporary_prefix, FileExpectation::Any)
    }

    pub(crate) fn save_bytes_atomically_expected(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expected: FileExpectation<'_>,
    ) -> Result<(), String> {
        self.save_bytes_atomically_expected_with_receipt(path, encoded, temporary_prefix, expected)
            .map(drop)
            .map_err(|error| error.message)
    }

    pub(crate) fn save_bytes_atomically_expected_with_receipt(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expected: FileExpectation<'_>,
    ) -> Result<FilePublicationReceipt, FilePublicationError> {
        self.save_bytes_atomically_expected_with_receipt_and_guard(
            path,
            encoded,
            temporary_prefix,
            expected,
            || Ok(()),
        )
    }

    pub(crate) fn save_bytes_atomically_expected_with_receipt_and_guard(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expected: FileExpectation<'_>,
        mut namespace_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<FilePublicationReceipt, FilePublicationError> {
        self.save_bytes_atomically_expected_with_all_hooks_guarded(
            path,
            encoded,
            temporary_prefix,
            AtomicSaveExpectation {
                require_attached_before_commit: true,
                file: expected,
                retain_publication_lock: false,
            },
            AtomicSaveHooks {
                before_commit: || Ok(()),
                after_evacuation: || {},
                before_receipt: || {},
            },
            &mut namespace_guard,
        )
    }

    pub(crate) fn save_bytes_atomically_expected_with_locked_receipt(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expected: FileExpectation<'_>,
    ) -> Result<FilePublicationReceipt, FilePublicationError> {
        self.save_bytes_atomically_expected_with_hooks(
            path,
            encoded,
            temporary_prefix,
            AtomicSaveExpectation {
                require_attached_before_commit: true,
                file: expected,
                retain_publication_lock: true,
            },
            || Ok(()),
            || {},
        )
    }

    #[cfg(test)]
    pub(crate) fn save_bytes_atomically_expected_with_locked_receipt_before_return(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expected: FileExpectation<'_>,
        before_receipt: impl FnOnce(),
    ) -> Result<FilePublicationReceipt, FilePublicationError> {
        self.save_bytes_atomically_expected_with_all_hooks(
            path,
            encoded,
            temporary_prefix,
            AtomicSaveExpectation {
                require_attached_before_commit: true,
                file: expected,
                retain_publication_lock: true,
            },
            AtomicSaveHooks {
                before_commit: || Ok(()),
                after_evacuation: || {},
                before_receipt,
            },
        )
    }

    #[cfg(all(test, unix))]
    pub(crate) fn save_bytes_atomically_after_effects(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
    ) -> Result<(), String> {
        self.save_bytes_atomically_after_effects_expected(
            path,
            encoded,
            temporary_prefix,
            FileExpectation::Any,
        )
    }

    #[cfg(all(test, unix))]
    pub(crate) fn save_bytes_atomically_after_effects_expected(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expected: FileExpectation<'_>,
    ) -> Result<(), String> {
        self.save_bytes_atomically_expected_with_hook(
            path,
            encoded,
            temporary_prefix,
            false,
            expected,
            || Ok(()),
        )
    }

    #[cfg(test)]
    pub(crate) fn save_bytes_atomically_with_hook(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        require_attached_before_commit: bool,
        before_commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.save_bytes_atomically_expected_with_hook(
            path,
            encoded,
            temporary_prefix,
            require_attached_before_commit,
            FileExpectation::Any,
            before_commit,
        )
    }

    pub(crate) fn save_bytes_atomically_expected_with_hook(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        require_attached_before_commit: bool,
        expected: FileExpectation<'_>,
        before_commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.save_bytes_atomically_expected_with_hooks(
            path,
            encoded,
            temporary_prefix,
            AtomicSaveExpectation {
                require_attached_before_commit,
                file: expected,
                retain_publication_lock: false,
            },
            before_commit,
            || {},
        )
        .map(drop)
        .map_err(|error| error.message)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn save_bytes_atomically_expected_with_guard_and_hook(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        require_attached_before_commit: bool,
        expected: FileExpectation<'_>,
        mut namespace_guard: impl FnMut() -> Result<(), String>,
        before_commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.save_bytes_atomically_expected_with_all_hooks_guarded(
            path,
            encoded,
            temporary_prefix,
            AtomicSaveExpectation {
                require_attached_before_commit,
                file: expected,
                retain_publication_lock: false,
            },
            AtomicSaveHooks {
                before_commit,
                after_evacuation: || {},
                before_receipt: || {},
            },
            &mut namespace_guard,
        )
        .map(drop)
        .map_err(|error| error.message)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn save_bytes_atomically_expected_with_recovery_hooks(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expectation: AtomicSaveExpectation<'_>,
        before_commit: impl FnOnce() -> Result<(), String>,
        after_evacuation: impl FnOnce(),
    ) -> Result<(), String> {
        self.save_bytes_atomically_expected_with_hooks(
            path,
            encoded,
            temporary_prefix,
            expectation,
            before_commit,
            after_evacuation,
        )
        .map(drop)
        .map_err(|error| error.message)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn save_bytes_atomically_expected_with_after_evacuation_hook(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expected: FileExpectation<'_>,
        after_evacuation: impl FnOnce(),
    ) -> Result<(), String> {
        self.save_bytes_atomically_expected_with_hooks(
            path,
            encoded,
            temporary_prefix,
            AtomicSaveExpectation {
                require_attached_before_commit: true,
                file: expected,
                retain_publication_lock: false,
            },
            || Ok(()),
            after_evacuation,
        )
        .map(drop)
        .map_err(|error| error.message)
    }

    pub(crate) fn save_bytes_atomically_expected_with_hooks(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expectation: AtomicSaveExpectation<'_>,
        before_commit: impl FnOnce() -> Result<(), String>,
        after_evacuation: impl FnOnce(),
    ) -> Result<FilePublicationReceipt, FilePublicationError> {
        self.save_bytes_atomically_expected_with_all_hooks(
            path,
            encoded,
            temporary_prefix,
            expectation,
            AtomicSaveHooks {
                before_commit,
                after_evacuation,
                before_receipt: || {},
            },
        )
    }

    pub(crate) fn save_bytes_atomically_expected_with_all_hooks(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expectation: AtomicSaveExpectation<'_>,
        hooks: AtomicSaveHooks<impl FnOnce() -> Result<(), String>, impl FnOnce(), impl FnOnce()>,
    ) -> Result<FilePublicationReceipt, FilePublicationError> {
        self.save_bytes_atomically_expected_with_all_hooks_guarded(
            path,
            encoded,
            temporary_prefix,
            expectation,
            hooks,
            &mut || Ok(()),
        )
    }
}
