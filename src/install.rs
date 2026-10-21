//! An installed suite: download, prepare, activate, roll back and clean releases.
//!
//! ```text
//! <root>/
//! ├── jarsuite.toml        manifest URL and state (current, previous, pending release)
//! ├── releases/<r>/        extracted archive + .suite.toml (its manifest) + .inuse/<tool>.lock
//! ├── staging/             downloads in progress
//! ├── locks/jarsuite.lock  one update/activation at a time
//! └── .jarexec/            cache shared by every tool and release (JRE, dependencies)
//! ```
//!
//! The tools share data, so two releases must never run side by side: `update`
//! prepares a release even while tools are running, `activate` switches to it only
//! when none is (`run` holds `.inuse/<tool>.lock` shared while a tool runs).

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use fd_lock::RwLock;
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::manifest::{Manifest, archive_ext, current_platform};
use crate::source;

const STATE_FILE: &str = "jarsuite.toml";
const RELEASE_MANIFEST: &str = ".suite.toml";
const INUSE_DIR: &str = ".inuse";

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// Manifest (`suite.toml`) this installation follows.
    pub url: Option<String>,
    pub current: Option<String>,
    pub previous: Option<String>,
    /// Installed and prepared, waiting for `activate`.
    pub pending: Option<String>,
    /// Highest data format ever active: the shared data may already be in it.
    pub data_format: Option<u32>,
}

#[derive(Debug, PartialEq)]
pub enum Update {
    /// The published release is the active one.
    UpToDate(String),
    /// Installed and prepared: `activate` makes it current.
    Ready(String),
}

#[derive(Debug)]
pub struct Activated {
    pub release: String,
    /// Releases removed because neither current, previous nor pending.
    pub removed: Vec<String>,
    /// Cache cleanup: files and bytes removed, `None` if a tool was being prepared.
    pub cache: Option<jarexec::clean::Report>,
}

/// Tools still running from some release: activation must wait for them.
#[derive(Debug)]
pub struct InUse(pub Vec<String>);

impl fmt::Display for InUse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "close {} and retry", self.0.join(", "))
    }
}

impl std::error::Error for InUse {}

/// Installation folder: `arg`, then `$JARSUITE_ROOT`, then the folder of the executable.
pub fn root_dir(arg: Option<PathBuf>) -> Result<PathBuf> {
    match arg.or_else(|| std::env::var_os("JARSUITE_ROOT").map(PathBuf::from)) {
        Some(dir) => {
            std::path::absolute(&dir).with_context(|| format!("invalid root {}", dir.display()))
        }
        None => jarexec::paths::exe_dir(),
    }
}

pub struct Root {
    dir: PathBuf,
}

impl Root {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn cache(&self) -> PathBuf {
        self.dir.join(".jarexec")
    }

    pub fn release_dir(&self, release: &str) -> PathBuf {
        self.dir.join("releases").join(release)
    }

    pub fn inuse_lock(&self, release: &str, tool: &str) -> Result<RwLock<File>> {
        let dir = self.release_dir(release).join(INUSE_DIR);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        open_lock(&dir.join(format!("{tool}.lock")))
    }

    pub fn state(&self) -> Result<State> {
        let path = self.dir.join(STATE_FILE);
        match fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn save(&self, state: &State) -> Result<()> {
        write_atomic(&self.dir.join(STATE_FILE), &toml::to_string(state)?)
    }

    /// Manifest of an installed release.
    pub fn release_manifest(&self, release: &str) -> Result<Manifest> {
        let path = self.release_dir(release).join(RELEASE_MANIFEST);
        let text = fs::read_to_string(&path)
            .with_context(|| format!("release {release} is not installed"))?;
        Manifest::parse(&text).with_context(|| format!("invalid {}", path.display()))
    }

    /// Installed releases, sorted by name.
    pub fn installed(&self) -> Result<Vec<String>> {
        let dir = self.dir.join("releases");
        let mut releases = Vec::new();
        if dir.is_dir() {
            for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
                let entry = entry?;
                if entry.path().join(RELEASE_MANIFEST).is_file() {
                    releases.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
        }
        releases.sort();
        Ok(releases)
    }

    pub fn init(&self, manifest: &str) -> Result<Url> {
        let url = source::locate(manifest, None)?;
        self.exclusive(|| {
            let mut state = self.state()?;
            state.url = Some(url.to_string());
            self.save(&state)
        })?;
        Ok(url)
    }

    /// The published manifest and its URL.
    pub fn fetch_manifest(&self) -> Result<(Url, Manifest)> {
        let url = self
            .state()?
            .url
            .context("no manifest configured: run `jarsuite init <url>` first")?;
        let url = Url::parse(&url)?;
        let manifest = Manifest::parse(&source::read_text(&url)?)
            .with_context(|| format!("invalid manifest {url}"))?;
        Ok((url, manifest))
    }

    /// Installs the published release and prepares every tool with `prepare(lock)`
    /// (Java and dependencies into the shared cache). Does not activate it.
    pub fn update(
        &self,
        prepare: impl Fn(&Path) -> Result<()>,
        step: impl Fn(&str),
    ) -> Result<Update> {
        let (url, manifest) = self.fetch_manifest()?;
        let release = manifest.suite.release.clone();
        self.exclusive(|| {
            let mut state = self.state()?;
            if state.current.as_deref() == Some(&release) {
                return Ok(Update::UpToDate(release.clone()));
            }
            if !self.release_dir(&release).join(RELEASE_MANIFEST).is_file() {
                self.install(&url, &manifest, &prepare, &step)?;
            }
            state.pending = Some(release.clone());
            self.save(&state)?;
            Ok(Update::Ready(release.clone()))
        })
    }

    fn install(
        &self,
        url: &Url,
        manifest: &Manifest,
        prepare: &impl Fn(&Path) -> Result<()>,
        step: &impl Fn(&str),
    ) -> Result<()> {
        let release = &manifest.suite.release;
        let platform = current_platform()?;
        let archive = manifest
            .archive
            .get(&platform)
            .with_context(|| format!("release {release} has no archive for {platform}"))?;
        let staging = self.dir.join("staging");
        if staging.exists() {
            fs::remove_dir_all(&staging)
                .with_context(|| format!("removing {}", staging.display()))?;
        }
        fs::create_dir_all(&staging)?;

        step(&format!("downloading release {release} ({platform})"));
        let file = staging.join(format!("{release}.{}", archive_ext(&platform)));
        source::download(
            &source::locate(&archive.url, Some(url))?,
            &file,
            Some((&archive.sha256, archive.size)),
        )?;
        let extracted = staging.join(release);
        extract(&file, archive_ext(&platform) == "zip", &extracted)
            .with_context(|| format!("extracting {}", file.display()))?;
        for tool in &manifest.tools {
            ensure!(
                extracted.join(format!("{tool}.lock")).is_file(),
                "the archive of {release} has no {tool}.lock"
            );
        }
        fs::write(extracted.join(RELEASE_MANIFEST), manifest.to_toml()?)?;

        // prepared in its final place: the cache markers record the lock's path
        let dest = self.release_dir(release);
        if dest.exists() {
            fs::remove_dir_all(&dest).with_context(|| format!("removing {}", dest.display()))?;
        }
        fs::create_dir_all(dest.parent().unwrap())?;
        fs::rename(&extracted, &dest).with_context(|| format!("moving to {}", dest.display()))?;
        let _ = fs::remove_dir_all(&staging);
        let prepared = manifest.tools.iter().try_for_each(|tool| {
            step(&format!("preparing {tool}"));
            prepare(&dest.join(format!("{tool}.lock"))).with_context(|| format!("preparing {tool}"))
        });
        if prepared.is_err() {
            let _ = fs::remove_dir_all(&dest);
        }
        prepared
    }

    /// Makes `release` (default: the pending one) current. Refuses while any tool
    /// runs ([`InUse`]) and, without `force`, when the release has a lower data
    /// format than the data may already be in.
    pub fn activate(&self, release: Option<&str>, force: bool) -> Result<Activated> {
        self.exclusive(|| {
            let mut state = self.state()?;
            let target = match release {
                Some(r) => r.to_string(),
                None => state
                    .pending
                    .clone()
                    .context("no release waiting: run `jarsuite update` first")?,
            };
            let format = self.release_manifest(&target)?.suite.data_format;
            if let (Some(high), Some(format)) = (state.data_format, format)
                && format < high
                && !force
            {
                bail!(
                    "release {target} uses data format {format}, but the shared data may \
                     already be in format {high} and unreadable for it. \
                     Use --force only if you have a backup of the data"
                );
            }

            // exclusive locks on every tool of every release, held until the switch is saved
            let mut locks = Vec::new();
            for r in self.installed()? {
                for tool in self.release_manifest(&r)?.tools {
                    locks.push((format!("{tool} ({r})"), self.inuse_lock(&r, &tool)?));
                }
            }
            let mut guards = Vec::new();
            let mut busy = Vec::new();
            for (name, lock) in &mut locks {
                match lock.try_write() {
                    Ok(guard) => guards.push(guard),
                    Err(_) => busy.push(name.clone()),
                }
            }
            if !busy.is_empty() {
                return Err(InUse(busy).into());
            }

            if state.current.as_deref() != Some(&target) {
                state.previous = state.current.take();
                state.current = Some(target.clone());
            }
            if state.pending.as_deref() == Some(&target) {
                state.pending = None;
            }
            state.data_format = state.data_format.max(format);
            self.save(&state)?;
            drop(guards);
            drop(locks);

            let removed = self.remove_unlisted(&state)?;
            let cache = jarexec::clean::unused(&self.cache())?;
            Ok(Activated {
                release: target,
                removed,
                cache,
            })
        })
    }

    pub fn rollback(&self, force: bool) -> Result<Activated> {
        let previous = self
            .state()?
            .previous
            .context("no previous release to go back to")?;
        self.activate(Some(&previous), force)
    }

    /// Removes releases that are neither current, previous nor pending, then what the
    /// cache holds for no remaining release.
    pub fn clean(&self) -> Result<(Vec<String>, Option<jarexec::clean::Report>)> {
        self.exclusive(|| {
            let removed = self.remove_unlisted(&self.state()?)?;
            Ok((removed, jarexec::clean::unused(&self.cache())?))
        })
    }

    fn remove_unlisted(&self, state: &State) -> Result<Vec<String>> {
        let keep = [&state.current, &state.previous, &state.pending];
        let mut removed = Vec::new();
        for release in self.installed()? {
            if keep.iter().any(|k| k.as_deref() == Some(&release)) {
                continue;
            }
            // a tool of an old release still running keeps it (and its lock files) alive
            let tools = self.release_manifest(&release)?.tools;
            let mut locks = tools
                .iter()
                .map(|t| self.inuse_lock(&release, t))
                .collect::<Result<Vec<_>>>()?;
            let idle = locks.iter_mut().all(|l| l.try_write().is_ok());
            drop(locks);
            if idle && fs::remove_dir_all(self.release_dir(&release)).is_ok() {
                removed.push(release);
            }
        }
        Ok(removed)
    }

    /// Runs `f` holding the installation lock: one update or activation at a time.
    fn exclusive<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let dir = self.dir.join("locks");
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut lock = open_lock(&dir.join("jarsuite.lock"))?;
        let _guard = lock.try_write().map_err(|_| {
            anyhow!(
                "another jarsuite is updating {}: retry when it is done",
                self.dir.display()
            )
        })?;
        f()
    }
}

pub fn open_lock(path: &Path) -> Result<RwLock<File>> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    Ok(RwLock::new(file))
}

/// `tar` and `zip` already refuse paths with `..` or absolute paths.
fn extract(archive: &Path, is_zip: bool, dest: &Path) -> Result<()> {
    let file = File::open(archive)?;
    if is_zip {
        zip::ZipArchive::new(file)?.extract(dest)?;
    } else {
        tar::Archive::new(flate2::read::GzDecoder::new(file)).unpack(dest)?;
    }
    Ok(())
}

fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::assemble;

    /// Builds releases of a two-tool suite into `<dir>/pub/<release>/` (archive + suite.toml).
    struct Publisher {
        dir: tempfile::TempDir,
        bin: PathBuf,
    }

    impl Publisher {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("jarexec-bin");
            fs::write(&bin, b"#!/bin/sh\necho launcher\n").unwrap();
            Self { dir, bin }
        }

        fn publish(&self, release: &str, data_format: u32) -> PathBuf {
            let src = self.dir.path().join("src").join(release);
            let def = assemble::tests::write_suite(&src, release, data_format, ("1.0.0", "1.0.0"));
            let out = self.dir.path().join("pub").join(release);
            let mut opts = assemble::tests::options(&def, &out, &self.bin);
            let platform = current_platform().unwrap();
            opts.jarexec = vec![(platform.clone(), self.bin.clone())];
            opts.platforms = vec![platform];
            assemble::run(&opts).unwrap().manifest
        }
    }

    fn update(root: &Root) -> (Result<Update>, Vec<PathBuf>) {
        let prepared = RefCell::new(Vec::new());
        let result = root.update(
            |lock| {
                prepared.borrow_mut().push(lock.to_path_buf());
                Ok(())
            },
            |_| {},
        );
        (result, prepared.into_inner())
    }

    #[test]
    fn installs_activates_and_rolls_back() {
        let publisher = Publisher::new();
        let v1 = publisher.publish("1.0", 1);
        let root_dir = tempfile::tempdir().unwrap();
        let root = Root::new(root_dir.path());
        root.init(v1.to_str().unwrap()).unwrap();

        let (result, prepared) = update(&root);
        assert_eq!(result.unwrap(), Update::Ready("1.0".into()));
        assert_eq!(
            prepared,
            [
                root.release_dir("1.0").join("tool-a.lock"),
                root.release_dir("1.0").join("tool-b.lock")
            ]
        );
        assert!(
            root.release_dir("1.0")
                .join("lib/commons-1.0.0.jar")
                .is_file()
        );
        assert_eq!(root.state().unwrap().current, None);

        root.activate(None, false).unwrap();
        let state = root.state().unwrap();
        assert_eq!(
            (state.current.as_deref(), state.pending),
            (Some("1.0"), None)
        );
        assert_eq!(update(&root).0.unwrap(), Update::UpToDate("1.0".into()));

        // 2.0 moves the shared data to format 2
        let v2 = publisher.publish("2.0", 2);
        root.init(v2.to_str().unwrap()).unwrap();
        assert_eq!(update(&root).0.unwrap(), Update::Ready("2.0".into()));
        root.activate(None, false).unwrap();
        let state = root.state().unwrap();
        assert_eq!(
            (
                state.current.as_deref(),
                state.previous.as_deref(),
                state.data_format
            ),
            (Some("2.0"), Some("1.0"), Some(2))
        );

        let err = root.rollback(false).unwrap_err().to_string();
        assert!(err.contains("data format 1"), "{err}");
        let back = root.rollback(true).unwrap();
        assert_eq!(back.release, "1.0");
        let state = root.state().unwrap();
        assert_eq!(
            (
                state.current.as_deref(),
                state.previous.as_deref(),
                state.data_format
            ),
            (Some("1.0"), Some("2.0"), Some(2))
        );
    }

    #[test]
    fn activation_waits_for_running_tools() {
        let publisher = Publisher::new();
        let root_dir = tempfile::tempdir().unwrap();
        let root = Root::new(root_dir.path());
        root.init(publisher.publish("1.0", 1).to_str().unwrap())
            .unwrap();
        update(&root).0.unwrap();
        root.activate(None, false).unwrap();
        root.init(publisher.publish("2.0", 1).to_str().unwrap())
            .unwrap();
        update(&root).0.unwrap();

        // tool-b of 1.0 is running (what `run` holds)
        let running = root.inuse_lock("1.0", "tool-b").unwrap();
        let guard = running.read().unwrap();
        let err = root.activate(None, false).unwrap_err();
        let in_use = err.downcast_ref::<InUse>().expect("InUse error");
        assert_eq!(in_use.0, ["tool-b (1.0)"]);
        assert_eq!(root.state().unwrap().pending.as_deref(), Some("2.0"));

        drop(guard);
        assert_eq!(root.activate(None, false).unwrap().release, "2.0");
    }

    #[test]
    fn corrupt_archive_changes_nothing() {
        let publisher = Publisher::new();
        let manifest = publisher.publish("1.0", 1);
        let root_dir = tempfile::tempdir().unwrap();
        let root = Root::new(root_dir.path());
        root.init(manifest.to_str().unwrap()).unwrap();
        let archive = fs::read_dir(manifest.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.to_string_lossy().ends_with(".gz") || p.to_string_lossy().ends_with(".zip"))
            .unwrap();
        fs::write(&archive, b"corrupt").unwrap();

        let err = update(&root).0.unwrap_err();
        assert!(format!("{err:#}").contains("expected SHA-256"), "{err:#}");
        assert_eq!(root.installed().unwrap(), Vec::<String>::new());
        assert_eq!(root.state().unwrap().pending, None);
    }

    #[test]
    fn failed_preparation_removes_the_release() {
        let publisher = Publisher::new();
        let root_dir = tempfile::tempdir().unwrap();
        let root = Root::new(root_dir.path());
        root.init(publisher.publish("1.0", 1).to_str().unwrap())
            .unwrap();
        let err = root.update(|_| bail!("no network"), |_| {}).unwrap_err();
        assert!(format!("{err:#}").contains("no network"));
        assert!(!root.release_dir("1.0").exists());
    }

    #[test]
    fn keeps_current_previous_and_pending_only() {
        let publisher = Publisher::new();
        let root_dir = tempfile::tempdir().unwrap();
        let root = Root::new(root_dir.path());
        for release in ["1.0", "2.0", "3.0"] {
            root.init(publisher.publish(release, 1).to_str().unwrap())
                .unwrap();
            update(&root).0.unwrap();
            let activated = root.activate(None, false).unwrap();
            if release == "3.0" {
                assert_eq!(activated.removed, ["1.0"]);
            }
        }
        assert_eq!(root.installed().unwrap(), ["2.0", "3.0"]);
    }
}
