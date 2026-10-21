//! `jarsuite run <tool>`: launches a tool of the active release with the shared cache,
//! holding its `.inuse` lock so that no activation happens while it runs.

use std::ffi::OsString;
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use crate::install::Root;

/// Runs `tool` and returns its exit code. Ctrl+C handling is up to the caller.
pub fn run(root: &Root, tool: &str, args: &[OsString]) -> Result<i32> {
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
        let status = Command::new(&exe)
            .args(args)
            .env(jarexec::paths::CACHE_ENV, root.cache())
            .env("JARSUITE_ROOT", root.dir())
            .env("JARSUITE_RELEASE", &release)
            .status()
            .with_context(|| format!("launching {}", exe.display()))?;
        return Ok(status.code().unwrap_or(1));
    }
    bail!("the active release keeps changing: retry")
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn runs_the_tool_of_the_active_release() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::new(dir.path());
        let release = root.release_dir("1.0");
        fs::create_dir_all(&release).unwrap();
        fs::write(
            release.join(".suite.toml"),
            "version = 1\ntools = [\"tool-a\"]\n[suite]\nname = \"demo\"\nrelease = \"1.0\"\n",
        )
        .unwrap();
        let out = dir.path().join("out.txt");
        let script = format!(
            "#!/bin/sh\necho \"$JARSUITE_RELEASE $JAREXEC_CACHE $*\" > '{}'\nexit 3\n",
            out.display()
        );
        fs::write(release.join("tool-a"), script).unwrap();
        fs::set_permissions(release.join("tool-a"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.path().join("jarsuite.toml"), "current = \"1.0\"\n").unwrap();

        let code = run(&root, "tool-a", &["add".into(), "x".into()]).unwrap();
        assert_eq!(code, 3);
        assert_eq!(
            fs::read_to_string(&out).unwrap().trim(),
            format!("1.0 {} add x", root.cache().display())
        );
        assert!(run(&root, "tool-z", &[]).is_err());
    }
}
