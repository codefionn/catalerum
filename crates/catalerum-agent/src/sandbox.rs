//! OS-level confinement for executed commands (SOUL §19/§20).
//!
//! File *tool* ops are already path-scoped by the daemon itself; this module adds a
//! second wall around `computer_exec`'s child process so a command can't reach the
//! filesystem outside the granted directories:
//!
//! - **Linux** — a Landlock LSM ruleset applied in the child (via `pre_exec`):
//!   **writes** only under the granted read-write paths; read+execute on the
//!   granted paths plus the standard system prefixes (`/usr`, `/etc`, …) so the
//!   shell, binaries, and libraries still work (and `/tmp`/`/dev` stay writable —
//!   `> /dev/null`, scratch files). Best-effort: on a kernel without Landlock the
//!   ruleset build fails and the command runs unconfined (the daemon logs it).
//! - **macOS** — the command is wrapped in `sandbox-exec` with a generated profile
//!   that denies file writes outside the granted read-write paths.
//! - **Windows / other** — no OS sandbox (the daemon's own path scoping still gates
//!   the file *tools*).
//!
//! Every platform returns a ready-to-spawn [`tokio::process::Command`]; the caller
//! sets stdio, timeout, and output caps.

use std::path::Path;

use catalerum_core::computer::{DirGrant, SandboxKind};
use tokio::process::Command;

/// Which sandbox this build/host will actually apply (advertised in `Hello`).
pub fn active_kind(enabled: bool) -> SandboxKind {
    if !enabled {
        return SandboxKind::None;
    }
    if cfg!(target_os = "linux") {
        SandboxKind::Landlock
    } else if cfg!(target_os = "macos") {
        SandboxKind::SandboxExec
    } else {
        SandboxKind::None
    }
}

/// Build the shell command to run `command` in `cwd`, applying the OS sandbox over
/// `dirs` when `sandbox` is set.
pub fn build_command(command: &str, cwd: &Path, dirs: &[DirGrant], sandbox: bool) -> Command {
    #[cfg(target_os = "macos")]
    {
        if sandbox {
            return macos_sandboxed(command, cwd, dirs);
        }
    }

    let mut cmd = shell_command(command);
    cmd.current_dir(cwd);

    #[cfg(target_os = "linux")]
    {
        if sandbox {
            apply_landlock(&mut cmd, dirs);
        }
    }
    let _ = (dirs, sandbox); // silence unused on non-linux/macos
    cmd
}

/// A `/bin/sh -c <command>` (unix) or `cmd /C <command>` (windows) Command.
fn shell_command(command: &str) -> Command {
    #[cfg(target_family = "unix")]
    {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(command);
        cmd
    }
    #[cfg(target_family = "windows")]
    {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    }
}

#[cfg(target_os = "linux")]
fn apply_landlock(cmd: &mut Command, dirs: &[DirGrant]) {
    use landlock::{
        Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI,
    };

    let abi = ABI::V2;
    // Build the ruleset BEFORE fork (opening the path fds), then call
    // `restrict_self()` in the child's pre-exec. Any failure here → run unconfined.
    let build = || -> Result<landlock::RulesetCreated, String> {
        let mut ruleset = Ruleset::default()
            .handle_access(AccessFs::from_all(abi))
            .map_err(|e| e.to_string())?
            .create()
            .map_err(|e| e.to_string())?;
        // The OS itself stays readable + executable — without this the child can't
        // even exec `/bin/sh` (every spawn fails EACCES). Writes remain denied, so
        // confinement is write-scoped, mirroring the macOS profile. Distro-specific
        // paths that don't exist are skipped like missing grants below.
        const SYSTEM_READ_EXEC: &[&str] = &[
            "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/etc", "/opt", "/nix", "/proc",
            "/sys", "/run", "/var",
        ];
        for path in SYSTEM_READ_EXEC {
            if let Ok(fd) = PathFd::new(path) {
                ruleset = ruleset
                    .add_rule(PathBeneath::new(fd, AccessFs::from_read(abi)))
                    .map_err(|e| e.to_string())?;
            }
        }
        // Scratch space and device files commands legitimately write to
        // (`> /dev/null`, mkstemp); device nodes are still guarded by plain DAC.
        for path in ["/tmp", "/dev"] {
            if let Ok(fd) = PathFd::new(path) {
                ruleset = ruleset
                    .add_rule(PathBeneath::new(fd, AccessFs::from_all(abi)))
                    .map_err(|e| e.to_string())?;
            }
        }
        for d in dirs {
            let access = if d.mode.can_write() {
                AccessFs::from_all(abi)
            } else {
                AccessFs::from_read(abi)
            };
            let fd = match PathFd::new(&d.path) {
                Ok(fd) => fd,
                Err(_) => continue, // a missing granted dir is skipped, not fatal
            };
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, access))
                .map_err(|e| e.to_string())?;
        }
        Ok(ruleset)
    };

    match build() {
        Ok(ruleset) => {
            let mut ruleset = Some(ruleset);
            // SAFETY: the closure only calls `restrict_self` (Landlock syscalls) in
            // the forked child before exec; it allocates nothing after fork beyond
            // what Landlock itself does, and never touches shared parent state.
            unsafe {
                cmd.pre_exec(move || {
                    if let Some(rs) = ruleset.take() {
                        rs.restrict_self()
                            .map_err(|e| std::io::Error::other(e.to_string()))?;
                    }
                    Ok(())
                });
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Landlock unavailable — running command unconfined");
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use catalerum_core::computer::DirMode;

    fn landlock_supported() -> bool {
        use landlock::{Access, AccessFs, Ruleset, RulesetAttr, ABI};
        match Ruleset::default().handle_access(AccessFs::from_all(ABI::V2)) {
            Ok(r) => r.create().is_ok(),
            Err(_) => false,
        }
    }

    fn rw_grant(path: &Path) -> [DirGrant; 1] {
        [DirGrant {
            path: path.to_string_lossy().into_owned(),
            mode: DirMode::ReadWrite,
        }]
    }

    async fn run_sandboxed(command: &str, cwd: &Path, grants: &[DirGrant]) -> std::process::Output {
        let mut cmd = build_command(command, cwd, grants, true);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        cmd.spawn()
            .expect("spawn under sandbox")
            .wait_with_output()
            .await
            .expect("wait for sandboxed command")
    }

    /// REGRESSION: the ruleset once granted access to the served dirs ONLY, so the
    /// child couldn't read/exec `/bin/sh` or the system libraries and every
    /// sandboxed spawn died with EACCES ("spawn failed: Permission denied").
    #[tokio::test]
    async fn sandboxed_exec_spawns_and_runs() {
        let tmp = std::env::temp_dir().join(format!("ca-sbx-run-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let out = run_sandboxed("echo sandboxed-ok", &tmp, &rw_grant(&tmp)).await;
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("sandboxed-ok"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Writes outside every grant stay denied — the confinement the sandbox is for.
    #[tokio::test]
    async fn sandbox_denies_writes_outside_grants() {
        if !landlock_supported() {
            eprintln!("kernel without Landlock — skipping the deny assertion");
            return;
        }
        // `/tmp` is deliberately writable inside the sandbox, so the outside
        // target lives under the workspace `target/` dir instead.
        let outside = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("ca-sbx-deny-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        let granted = std::env::temp_dir().join(format!("ca-sbx-grant-{}", std::process::id()));
        std::fs::create_dir_all(&granted).unwrap();

        let leak = outside.join("leak.txt");
        let out = run_sandboxed(
            &format!("echo x > '{}'", leak.display()),
            &granted,
            &rw_grant(&granted),
        )
        .await;
        assert!(
            !out.status.success(),
            "write outside the grants unexpectedly succeeded"
        );
        assert!(!leak.exists(), "file leaked outside the grants");

        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&granted);
    }
}

#[cfg(target_os = "macos")]
fn macos_sandboxed(command: &str, cwd: &Path, dirs: &[DirGrant]) -> Command {
    // A minimal sandbox-exec profile: allow everything by default EXCEPT
    // file-write, which is allowed only under the granted read-write subpaths.
    let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    for d in dirs {
        if d.mode.can_write() {
            // Escape any double-quotes in the path for the profile literal.
            let escaped = d.path.replace('"', "\\\"");
            profile.push_str(&format!("(allow file-write* (subpath \"{escaped}\"))\n"));
        }
    }
    let mut cmd = Command::new("sandbox-exec");
    cmd.arg("-p")
        .arg(profile)
        .arg("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd);
    cmd
}
