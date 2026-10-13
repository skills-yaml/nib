//! Explicit local-editor handoff with private drafts and bounded recovery.
use super::*;
use std::io::Read;

const MAX_EDITOR_DRAFT: u64 = MAX_COMPOSER_BYTES as u64;

pub(crate) fn edit_draft_with(
    draft: &str,
    editor: &str,
    invoke: impl FnOnce(&str, &Path) -> io::Result<bool>,
) -> io::Result<String> {
    if editor.trim().is_empty() {
        return Err(io::Error::other("set VISUAL or EDITOR to edit the prompt"));
    }
    let builder = tempfile::Builder::new();
    #[cfg(unix)]
    let builder = {
        let mut builder = builder;
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
        builder
    };
    let directory = builder.tempdir()?;
    #[cfg(windows)]
    make_directory_private(directory.path())?;
    let directory_handle =
        cap_std::fs::Dir::open_ambient_dir(directory.path(), cap_std::ambient_authority())?;
    let mut file = tempfile::NamedTempFile::new_in(directory.path())?;
    file.write_all(draft.as_bytes())?;
    file.flush()?;
    if !invoke(editor, file.path())? {
        return Err(io::Error::other("editor failed; original draft retained"));
    }
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let opened = directory_handle.open_with(
        file.path().file_name().expect("temporary filename"),
        &options,
    )?;
    let metadata = opened.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_EDITOR_DRAFT {
        return Err(io::Error::other(
            "editor draft must be a regular file within the composer byte limit",
        ));
    }
    let mut text = String::new();
    opened
        .take(MAX_EDITOR_DRAFT + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_EDITOR_DRAFT {
        return Err(io::Error::other("editor draft exceeds 1 MiB"));
    }
    // Treat returned content as pasted text, never as terminal commands.
    let mut composer = Composer::default();
    composer.insert_paste(&text);
    Ok(composer.input)
}

pub(crate) fn editor_handoff(
    suspend: impl FnOnce() -> io::Result<()>,
    invoke: impl FnOnce() -> io::Result<bool>,
    resume: impl FnOnce() -> io::Result<()>,
) -> io::Result<io::Result<bool>> {
    let result = suspend().and_then(|()| invoke());
    resume()?;
    Ok(result)
}

pub(crate) fn open_draft_editor(
    terminal: &mut DefaultTerminal,
    draft: &str,
) -> io::Result<Result<String, String>> {
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("EDITOR").ok())
        .unwrap_or_default();
    let mut restoration_error = None;
    let result = edit_draft_with(draft, &editor, |editor, path| {
        let handoff = editor_handoff(
            restore_terminal,
            || {
                let status = if cfg!(windows) {
                    Command::new("cmd").args(["/C", editor]).arg(path).status()
                } else {
                    // Only the user-owned editor setting is executable shell text.
                    // The private draft path is a separate positional argument.
                    Command::new("sh")
                        .args(["-c", &format!("exec {editor} \"$1\""), "nib-editor"])
                        .arg(path)
                        .status()
                };
                status.map(|s| s.success())
            },
            || {
                crossterm::terminal::enable_raw_mode()?;
                execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
                enable_bracketed_paste()?;
                enable_mouse_capture()?;
                terminal.clear()
            },
        );
        match handoff {
            Ok(result) => result,
            Err(error) => {
                restoration_error = Some(io::Error::new(error.kind(), error.to_string()));
                Err(error)
            }
        }
    });
    if let Some(error) = restoration_error {
        return Err(error);
    }
    Ok(result.map_err(|e| e.to_string()))
}

/// Protect editor replacement files and backups even with a shared TEMP override.
#[cfg(windows)]
fn make_directory_private(path: &Path) -> io::Result<()> {
    let script = r#"$ErrorActionPreference = 'Stop';
$acl = New-Object System.Security.AccessControl.DirectorySecurity;
$acl.SetAccessRuleProtection($true, $false);
$sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User;
$rule = New-Object System.Security.AccessControl.FileSystemAccessRule($sid, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow');
$acl.AddAccessRule($rule);
Set-Acl -LiteralPath $env:NIB_EDITOR_PRIVATE_DIR -AclObject $acl;"#;
    let status = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("NIB_EDITOR_PRIVATE_DIR", path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(io::Error::other(
            "cannot create a private editor directory; original draft retained",
        ));
    }
    Ok(())
}
