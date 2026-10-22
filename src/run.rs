//! `jarsuite run <tool>`: launches a tool of the active release with the shared cache,
//! holding its `.inuse` lock so that no activation happens while it runs.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use crate::install::Root;

/// Lines of standard error that [`run_captured`] keeps.
pub const STDERR_TAIL: usize = 20;

/// Runs `tool` and returns its exit code. Ctrl+C handling is up to the caller.
pub fn run(root: &Root, tool: &str, args: &[OsString]) -> Result<i32> {
    launch(root, tool, args, |mut command| {
        Ok(command.status()?.code().unwrap_or(1))
    })
}

/// Runs `tool` in the installation folder, for callers without a terminal (the
/// window): returns its exit code and the last [`STDERR_TAIL`] lines it wrote to
/// standard error.
pub fn run_captured(root: &Root, tool: &str, args: &[OsString]) -> Result<(i32, String)> {
    launch(root, tool, args, |mut command| {
        let mut child = command
            .current_dir(root.dir())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut tail = VecDeque::with_capacity(STDERR_TAIL);
        let mut stderr = BufReader::new(child.stderr.take().expect("piped stderr"));
        let mut line = Vec::new();
        // a tool may run for hours: keep only the end of what it writes
        while stderr.read_until(b'\n', &mut line)? > 0 {
            if tail.len() == STDERR_TAIL {
                tail.pop_front();
            }
            tail.push_back(String::from_utf8_lossy(&line).trim_end().to_string());
            line.clear();
        }
        let code = child.wait()?.code().unwrap_or(1);
        Ok((code, Vec::from(tail).join("\n")))
    })
}

/// Hands `exec` the command that starts `tool` of the active release, while holding
/// the release's in-use lock.
fn launch<T>(
    root: &Root,
    tool: &str,
    args: &[OsString],
    exec: impl FnOnce(Command) -> std::io::Result<T>,
) -> Result<T> {
    // an activation may switch release between reading the state and taking the lock
    for _ in 0..3 {
        let release = root
            .state()?
            .current
            .context("no active release: run `jarsuite update` first")?;
        let tools = root.release_manifest(&release)?.tools;
        ensure!(
            tools.iter().any(|t| t == tool),
            "release {release} has no tool {tool} (available: {})",
            tools.join(", ")
        );
        let lock = root.inuse_lock(&release, tool)?;
        let _in_use = lock.read().context("taking the in-use lock")?;
        if root.state()?.current.as_deref() != Some(release.as_str()) {
            continue;
        }

        let exe = root
            .release_dir(&release)
            .join(format!("{tool}{}", std::env::consts::EXE_SUFFIX));
        let mut command = Command::new(&exe);
        command
            .args(args)
            .env(jarexec::paths::CACHE_ENV, root.cache())
            .env("JARSUITE_ROOT", root.dir())
            .env("JARSUITE_RELEASE", &release);
        return exec(command).with_context(|| format!("launching {}", exe.display()));
    }
    bail!("the active release keeps changing: retry")
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// An installation with release 1.0 active, whose `tool-a` is `script`.
    fn install(script: &str) -> (tempfile::TempDir, Root) {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::new(dir.path());
        let release = root.release_dir("1.0");
        fs::create_dir_all(&release).unwrap();
        fs::write(
            release.join(".suite.toml"),
            "version = 1\ntools = [\"tool-a\"]\n[suite]\nname = \"demo\"\nrelease = \"1.0\"\n",
        )
        .unwrap();
        fs::write(release.join("tool-a"), format!("#!/bin/sh\n{script}")).unwrap();
        fs::set_permissions(release.join("tool-a"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.path().join("jarsuite.toml"), "current = \"1.0\"\n").unwrap();
        (dir, root)
    }

    #[test]
    fn runs_the_tool_of_the_active_release() {
        let out = tempfile::NamedTempFile::new().unwrap();
        let (_dir, root) = install(&format!(
            "echo \"$JARSUITE_RELEASE $JAREXEC_CACHE $*\" > '{}'\nexit 3\n",
            out.path().display()
        ));

        let code = run(&root, "tool-a", &["add".into(), "x".into()]).unwrap();
        assert_eq!(code, 3);
        assert_eq!(
            fs::read_to_string(out.path()).unwrap().trim(),
            format!("1.0 {} add x", root.cache().display())
        );
        assert!(run(&root, "tool-z", &[]).is_err());
    }

    #[test]
    fn captured_run_keeps_the_end_of_stderr_and_works_in_the_root() {
        let (_dir, root) = install(
            "pwd > ran-here.txt\necho to stdout\n\
             for i in $(seq 1 30); do echo \"line $i\" >&2; done\nexit 64\n",
        );

        let (code, stderr) = run_captured(&root, "tool-a", &[]).unwrap();
        assert_eq!(code, 64);
        let lines: Vec<_> = stderr.lines().collect();
        assert_eq!(lines.len(), STDERR_TAIL);
        assert_eq!(lines.first(), Some(&"line 11"));
        assert_eq!(lines.last(), Some(&"line 30"));
        let cwd = fs::read_to_string(root.dir().join("ran-here.txt")).unwrap();
        assert_eq!(
            fs::canonicalize(cwd.trim()).unwrap(),
            fs::canonicalize(root.dir()).unwrap()
        );
    }
}
