//! `jarsuite assemble`: composes one release of the suite from the tools' published
//! locks and JARs, checks that they can share one JRE and one data format, and writes
//! an archive per platform plus the `suite.toml` manifest.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail, ensure};
use jarexec::lock::bundle_matches;
use jarexec::lockfile::{Java, Lockfile};
use reqwest::Url;

use crate::manifest::{
    Archive, FORMAT_VERSION, JarexecDef, Manifest, SuiteDef, archive_ext, exe_suffix,
};
use crate::source;

pub struct Options {
    /// `suite-def.toml`
    pub def: PathBuf,
    pub out: PathBuf,
    /// Where the archives will be published; without it the manifest names them
    /// relative to itself.
    pub base_url: Option<String>,
    /// Local jarexec binaries by platform, instead of downloading the release.
    pub jarexec: Vec<(String, PathBuf)>,
    /// Only these platforms instead of `[jarexec] platforms` (e.g. a local build).
    pub platforms: Vec<String>,
}

#[derive(Debug)]
pub struct Assembled {
    pub manifest: PathBuf,
    pub archives: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

pub fn run(opts: &Options) -> Result<Assembled> {
    let def_url = source::locate(&opts.def.to_string_lossy(), None)?;
    let def = SuiteDef::parse(&source::read_text(&def_url)?)
        .with_context(|| format!("invalid suite definition {}", opts.def.display()))?;
    let release = &def.suite.release;
    let stage = opts.out.join(format!(".stage-{release}"));
    if stage.exists() {
        fs::remove_dir_all(&stage).with_context(|| format!("removing {}", stage.display()))?;
    }
    let files = stage.join("files");
    fs::create_dir_all(&files).with_context(|| format!("creating {}", files.display()))?;

    // archive path → (SHA-256, tool that brought it): tools share one folder
    let mut shipped: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut locks = Vec::new();
    for tool in &def.tools {
        let dir = source::dir_url(source::locate(&tool.source, Some(&def_url))?);
        let lock_url = dir.join(&format!("{}.lock", tool.name))?;
        let lock_text = source::read_text(&lock_url)?;
        let lock =
            Lockfile::parse(&lock_text).with_context(|| format!("invalid lockfile {lock_url}"))?;
        let (Some(sha256), Some(size)) = (lock.app.sha256.clone(), lock.app.size) else {
            bail!("{lock_url} has no SHA-256 for the JAR: regenerate it with jarexec 0.2 or later");
        };
        let mut wanted = vec![(lock.app.jar.clone(), sha256, size)];
        wanted.extend(lock.deps.iter().filter_map(|d| {
            d.bundled
                .clone()
                .map(|path| (path, d.sha256.clone(), d.size))
        }));
        for (reference, sha256, size) in wanted {
            let path = safe_relative(&reference)
                .with_context(|| format!("{}: unsafe path {reference}", tool.name))?;
            match shipped.get(&path) {
                Some((other_sha, _)) if *other_sha == sha256 => {}
                Some((_, other)) => {
                    bail!(
                        "{} and {other} both ship {path} with different content",
                        tool.name
                    )
                }
                None => {
                    let dest = files.join(&path);
                    if let Some(parent) = dest.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    source::download(&dir.join(&reference)?, &dest, Some((&sha256, size)))?;
                    shipped.insert(path, (sha256, tool.name.clone()));
                }
            }
        }
        fs::write(files.join(format!("{}.lock", tool.name)), &lock_text)?;
        locks.push((tool.name.clone(), lock));
    }
    for (name, _) in &locks {
        for own in [name.clone(), format!("{name}.exe"), format!("{name}.lock")] {
            if let Some((_, by)) = shipped.get(&own) {
                bail!("{by} ships a file named {own}, which is reserved for the tool {name}");
            }
        }
    }
    let warnings = check_alignment(&locks, &def.align.required)?;

    let base = match &opts.base_url {
        Some(url) => Some(source::dir_url(source::locate(url, None)?)),
        None => None,
    };
    let mut manifest = Manifest {
        version: FORMAT_VERSION,
        tools: locks.iter().map(|(name, _)| name.clone()).collect(),
        suite: def.suite.clone(),
        archive: BTreeMap::new(),
    };
    let platforms = if opts.platforms.is_empty() {
        &def.jarexec.platforms
    } else {
        &opts.platforms
    };
    let mut archives = Vec::new();
    for platform in platforms {
        let bin = match opts.jarexec.iter().find(|(p, _)| p == platform) {
            Some((_, path)) => path.clone(),
            None => fetch_jarexec(&def.jarexec, platform, &stage)?,
        };
        let mut entries = Vec::new();
        for (name, _) in &locks {
            entries.push(Entry::new(
                format!("{name}{}", exe_suffix(platform)),
                &bin,
                true,
            ));
            entries.push(Entry::new(
                format!("{name}.lock"),
                &files.join(format!("{name}.lock")),
                false,
            ));
        }
        for path in shipped.keys() {
            entries.push(Entry::new(path.clone(), &files.join(path), false));
        }
        let file = format!(
            "{}-{release}-{platform}.{}",
            def.suite.name,
            archive_ext(platform)
        );
        let dest = opts.out.join(&file);
        write_archive(&dest, &entries, archive_ext(platform) == "zip")
            .with_context(|| format!("writing {}", dest.display()))?;
        let (sha256, size) = jarexec::hash::sha256_file(&dest)?;
        let url = match &base {
            Some(base) => base.join(&file)?.to_string(),
            None => file,
        };
        manifest
            .archive
            .insert(platform.clone(), Archive { url, sha256, size });
        archives.push(dest);
    }

    let manifest_path = opts.out.join("suite.toml");
    fs::write(&manifest_path, manifest.to_toml()?)
        .with_context(|| format!("writing {}", manifest_path.display()))?;
    fs::remove_dir_all(&stage).with_context(|| format!("removing {}", stage.display()))?;
    Ok(Assembled {
        manifest: manifest_path,
        archives,
        warnings,
    })
}

/// One JRE for the whole suite, and the same version of every `required` dependency.
/// Returns the warnings: other dependencies in several versions, patterns matching nothing.
pub fn check_alignment(locks: &[(String, Lockfile)], required: &[String]) -> Result<Vec<String>> {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    if let Some((first, first_lock)) = locks.first() {
        for (name, lock) in &locks[1..] {
            if lock.java != first_lock.java {
                errors.push(format!(
                    "{first} and {name} need different Java: {} vs {}",
                    describe_java(&first_lock.java),
                    describe_java(&lock.java)
                ));
            }
        }
    }

    // (group, artifact, classifier) → version → tools
    type Key<'a> = (&'a str, &'a str, Option<&'a str>);
    let mut versions: BTreeMap<Key, BTreeMap<&str, Vec<&str>>> = BTreeMap::new();
    for (name, lock) in locks {
        for dep in &lock.deps {
            let parts: Vec<&str> = dep.coord.split(':').collect();
            let (key, version) = match parts.as_slice() {
                [g, a, _, v] => ((*g, *a, None), *v),
                [g, a, _, c, v] => ((*g, *a, Some(*c)), *v),
                _ => continue,
            };
            versions
                .entry(key)
                .or_default()
                .entry(version)
                .or_default()
                .push(name);
        }
    }
    let mut matched = vec![false; required.len()];
    for ((group, artifact, classifier), by_version) in &versions {
        let mut is_required = false;
        for (i, pattern) in required.iter().enumerate() {
            if bundle_matches(pattern, group, artifact) {
                matched[i] = true;
                is_required = true;
            }
        }
        if by_version.len() < 2 {
            continue;
        }
        let id = match classifier {
            Some(c) => format!("{group}:{artifact}:{c}"),
            None => format!("{group}:{artifact}"),
        };
        let detail = by_version
            .iter()
            .map(|(v, tools)| format!("{v} ({})", tools.join(", ")))
            .collect::<Vec<_>>()
            .join(" vs ");
        if is_required {
            errors.push(format!("{id} must be aligned: {detail}"));
        } else {
            warnings.push(format!("{id} in several versions: {detail}"));
        }
    }
    for (pattern, hit) in required.iter().zip(matched) {
        if !hit {
            warnings.push(format!("[align] pattern {pattern} matches no dependency"));
        }
    }
    ensure!(
        errors.is_empty(),
        "the tools cannot form one release:\n  {}",
        errors.join("\n  ")
    );
    Ok(warnings)
}

fn describe_java(java: &Java) -> String {
    let max = java
        .max
        .as_ref()
        .map(|m| format!("..{m}"))
        .unwrap_or_default();
    format!(
        "Java {}{max} {} ({}, {:?})",
        java.version,
        java.image.as_str(),
        java.vendor,
        java.source
    )
}

/// `lib/x.jar` stays as it is; `..`, absolute paths and empty paths are refused.
fn safe_relative(reference: &str) -> Result<String> {
    let parts: Vec<String> = Path::new(reference)
        .components()
        .map(|c| match c {
            Component::Normal(p) => Ok(p.to_string_lossy().into_owned()),
            _ => bail!("only plain relative paths are allowed"),
        })
        .collect::<Result<_>>()?;
    ensure!(!parts.is_empty(), "empty path");
    Ok(parts.join("/"))
}

/// Downloads the jarexec release archive for `platform`, checks it against its
/// `.sha256` and extracts the binary.
fn fetch_jarexec(def: &JarexecDef, platform: &str, stage: &Path) -> Result<PathBuf> {
    let version = def.version.as_deref().with_context(|| {
        format!("no jarexec binary for {platform}: set [jarexec] version or pass --jarexec {platform}=PATH")
    })?;
    let ext = archive_ext(platform);
    let url = def
        .url
        .replace("{version}", version)
        .replace("{platform}", platform)
        .replace("{ext}", ext);
    let url = source::locate(&url, None)?;
    let sums = Url::parse(&format!("{url}.sha256"))?;
    let expected = source::read_text(&sums)?
        .split_whitespace()
        .next()
        .with_context(|| format!("{sums} is empty"))?
        .to_ascii_lowercase();
    let archive = stage.join(format!("jarexec-{platform}.{ext}"));
    let (sha256, _) = source::download(&url, &archive, None)?;
    ensure!(
        sha256 == expected,
        "{url}: SHA-256 {sha256} does not match {sums}"
    );
    let member = format!("jarexec{}", exe_suffix(platform));
    let bin = stage.join(format!("jarexec-{platform}{}", exe_suffix(platform)));
    extract_member(&archive, ext == "zip", &member, &bin)
        .with_context(|| format!("extracting {member} from {url}"))?;
    Ok(bin)
}

fn extract_member(archive: &Path, is_zip: bool, member: &str, dest: &Path) -> Result<()> {
    let file = File::open(archive)?;
    let is_member = |path: &Path| path.file_name().is_some_and(|n| n == member);
    if is_zip {
        let mut zip = zip::ZipArchive::new(file)?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i)?;
            if entry.is_file() && is_member(Path::new(entry.name()?.as_ref())) {
                io::copy(&mut entry, &mut File::create(dest)?)?;
                return Ok(());
            }
        }
    } else {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
        for entry in tar.entries()? {
            let mut entry = entry?;
            if entry.header().entry_type().is_file() && is_member(&entry.path()?) {
                entry.unpack(dest)?;
                return Ok(());
            }
        }
    }
    bail!("{member} not found in the archive")
}

struct Entry {
    name: String,
    path: PathBuf,
    executable: bool,
}

impl Entry {
    fn new(name: String, path: &Path, executable: bool) -> Self {
        Self {
            name,
            path: path.to_path_buf(),
            executable,
        }
    }
}

/// zip for Windows, tar.gz elsewhere; the launchers are executable in both.
fn write_archive(dest: &Path, entries: &[Entry], is_zip: bool) -> Result<()> {
    let file = File::create(dest)?;
    let mode = |e: &Entry| if e.executable { 0o755 } else { 0o644 };
    if is_zip {
        let mut zip = zip::ZipWriter::new(file);
        for e in entries {
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(mode(e));
            zip.start_file(e.name.as_str(), options)?;
            io::copy(&mut File::open(&e.path)?, &mut zip)?;
        }
        zip.finish()?;
    } else {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        for e in entries {
            let mut src = File::open(&e.path)?;
            let mut header = tar::Header::new_gnu();
            header.set_size(src.metadata()?.len());
            header.set_mode(mode(e));
            header.set_mtime(now);
            tar.append_data(&mut header, &e.name, &mut src)?;
        }
        tar.into_inner()?.finish()?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use jarexec::lockfile::{App, Dep, JavaImage, JavaSource};

    pub fn java(version: &str) -> Java {
        Java {
            version: version.into(),
            max: None,
            source: JavaSource::Auto,
            vendor: "temurin".into(),
            image: JavaImage::Jre,
        }
    }

    pub fn dep(coord: &str, content: &[u8], bundled: Option<&str>) -> Dep {
        let (sha256, size) = jarexec::hash::sha256_reader(content).unwrap();
        Dep {
            coord: coord.into(),
            sha256,
            size,
            bundled: bundled.map(Into::into),
        }
    }

    fn lock(name: &str, java_version: &str, deps: Vec<Dep>) -> (String, Lockfile) {
        let lock = Lockfile {
            version: 1,
            app: App {
                name: name.into(),
                jar: format!("{name}.jar"),
                sha256: None,
                size: None,
                main: Some("Main".into()),
                jvm_args: vec![],
            },
            java: java(java_version),
            repositories: vec![],
            deps,
        };
        (name.into(), lock)
    }

    /// A tool as `jarexec lock --bundle` leaves it: `<dir>/<name>.lock`, the JAR, `lib/`.
    pub fn write_tool(dir: &Path, name: &str, jar: &[u8], deps: &[(&str, &[u8], bool)]) {
        fs::create_dir_all(dir.join("lib")).unwrap();
        let (_, mut lock) = lock(name, "21", vec![]);
        let (sha256, size) = jarexec::hash::sha256_reader(jar).unwrap();
        lock.app.sha256 = Some(sha256);
        lock.app.size = Some(size);
        fs::write(dir.join(&lock.app.jar), jar).unwrap();
        for (coord, content, bundled) in deps {
            let artifact = coord.split(':').nth(1).unwrap();
            let version = coord.rsplit(':').next().unwrap();
            let path = format!("lib/{artifact}-{version}.jar");
            if *bundled {
                fs::write(dir.join(&path), content).unwrap();
            }
            lock.deps
                .push(dep(coord, content, bundled.then_some(path.as_str())));
        }
        fs::write(dir.join(format!("{name}.lock")), lock.to_toml().unwrap()).unwrap();
    }

    /// Two tools sharing `commons`; returns the path of suite-def.toml.
    pub fn write_suite(
        dir: &Path,
        release: &str,
        data_format: u32,
        commons: (&str, &str),
    ) -> PathBuf {
        let (a_commons, b_commons) = commons;
        let coord = |v: &str| format!("com.example:commons:jar:{v}");
        write_tool(
            &dir.join("tool-a"),
            "tool-a",
            format!("tool-a for {release}").as_bytes(),
            &[
                (&coord(a_commons), a_commons.as_bytes(), true),
                ("org.slf4j:slf4j-api:jar:2.0.16", b"slf4j", false),
            ],
        );
        write_tool(
            &dir.join("tool-b"),
            "tool-b",
            format!("tool-b for {release}").as_bytes(),
            &[(&coord(b_commons), b_commons.as_bytes(), true)],
        );
        let def = dir.join("suite-def.toml");
        fs::write(
            &def,
            format!(
                r#"
                [suite]
                name = "demo"
                release = "{release}"
                data_format = {data_format}

                [align]
                required = ["com.example:commons"]

                [jarexec]
                platforms = ["linux-x64", "windows-x64"]

                [[tool]]
                name = "tool-a"
                source = "tool-a"

                [[tool]]
                name = "tool-b"
                source = "tool-b"
                "#
            ),
        )
        .unwrap();
        def
    }

    pub fn options(def: &Path, out: &Path, bin: &Path) -> Options {
        Options {
            def: def.to_path_buf(),
            out: out.to_path_buf(),
            base_url: None,
            jarexec: ["linux-x64", "windows-x64"]
                .iter()
                .map(|p| (p.to_string(), bin.to_path_buf()))
                .collect(),
            platforms: vec![],
        }
    }

    fn tar_names(path: &Path) -> Vec<(String, u32)> {
        let file = File::open(path).unwrap();
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
        let mut names: Vec<_> = tar
            .entries()
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let name = e.path().unwrap().to_string_lossy().into_owned();
                (name, e.header().mode().unwrap())
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn java_must_match_and_required_dependencies_must_align() {
        let ok = check_alignment(
            &[
                lock(
                    "a",
                    "21",
                    vec![dep("com.example:commons:jar:1.0", b"", None)],
                ),
                lock(
                    "b",
                    "21",
                    vec![dep("com.example:commons:jar:1.0", b"", None)],
                ),
            ],
            &["com.example:commons".into()],
        );
        assert_eq!(ok.unwrap(), Vec::<String>::new());

        let err = check_alignment(
            &[
                lock(
                    "a",
                    "21",
                    vec![dep("com.example:commons:jar:2.0", b"", None)],
                ),
                lock(
                    "b",
                    "17",
                    vec![dep("com.example:commons:jar:1.0", b"", None)],
                ),
            ],
            &["com.example:*".into()],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("a and b need different Java"), "{err}");
        assert!(
            err.contains("com.example:commons must be aligned: 1.0 (b) vs 2.0 (a)"),
            "{err}"
        );
    }

    #[test]
    fn other_differences_are_warnings() {
        let warnings = check_alignment(
            &[
                lock("a", "21", vec![dep("org.x:lang:jar:3.14", b"", None)]),
                lock("b", "21", vec![dep("org.x:lang:jar:3.17", b"", None)]),
            ],
            &["com.example:commons".into()],
        )
        .unwrap();
        assert_eq!(
            warnings,
            [
                "org.x:lang in several versions: 3.14 (a) vs 3.17 (b)",
                "[align] pattern com.example:commons matches no dependency"
            ]
        );
    }

    #[test]
    fn refuses_unsafe_paths() {
        assert_eq!(safe_relative("lib/x.jar").unwrap(), "lib/x.jar");
        assert!(safe_relative("../x.jar").is_err());
        assert!(safe_relative("/etc/x.jar").is_err());
        assert!(safe_relative("").is_err());
    }

    #[test]
    fn assembles_archives_and_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let def = write_suite(dir.path(), "1.0", 1, ("1.0.0", "1.0.0"));
        let bin = dir.path().join("jarexec-bin");
        fs::write(&bin, b"launcher").unwrap();
        let mut opts = options(&def, &dir.path().join("dist"), &bin);
        opts.base_url = Some("https://example.com/demo/1.0".into());

        let done = run(&opts).unwrap();
        let manifest = Manifest::parse(&fs::read_to_string(&done.manifest).unwrap()).unwrap();
        assert_eq!(manifest.tools, ["tool-a", "tool-b"]);
        assert_eq!(manifest.suite.data_format, Some(1));
        let linux = &manifest.archive["linux-x64"];
        assert_eq!(
            linux.url,
            "https://example.com/demo/1.0/demo-1.0-linux-x64.tar.gz"
        );
        let path = dir.path().join("dist/demo-1.0-linux-x64.tar.gz");
        assert_eq!(jarexec::hash::sha256_file(&path).unwrap().0, linux.sha256);
        assert_eq!(
            tar_names(&path),
            [
                ("lib/commons-1.0.0.jar".to_string(), 0o644),
                ("tool-a".into(), 0o755),
                ("tool-a.jar".into(), 0o644),
                ("tool-a.lock".into(), 0o644),
                ("tool-b".into(), 0o755),
                ("tool-b.jar".into(), 0o644),
                ("tool-b.lock".into(), 0o644),
            ]
        );
        let zip = zip::ZipArchive::new(
            File::open(dir.path().join("dist/demo-1.0-windows-x64.zip")).unwrap(),
        )
        .unwrap();
        assert!(zip.file_names().any(|n| n.unwrap() == "tool-a.exe"));
        assert!(!dir.path().join("dist/.stage-1.0").exists());
    }

    #[test]
    fn refuses_misaligned_commons_and_tampered_jars() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("jarexec-bin");
        fs::write(&bin, b"launcher").unwrap();

        let def = write_suite(dir.path(), "1.0", 1, ("2.0.0", "1.0.0"));
        let err = run(&options(&def, &dir.path().join("dist"), &bin)).unwrap_err();
        assert!(format!("{err:#}").contains("must be aligned"), "{err:#}");

        let def = write_suite(dir.path(), "1.0", 1, ("1.0.0", "1.0.0"));
        fs::write(dir.path().join("tool-b/tool-b.jar"), b"tampered").unwrap();
        let err = run(&options(&def, &dir.path().join("dist"), &bin)).unwrap_err();
        assert!(format!("{err:#}").contains("expected SHA-256"), "{err:#}");
    }

    #[test]
    fn downloads_jarexec_and_checks_its_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let def = write_suite(dir.path(), "1.0", 1, ("1.0.0", "1.0.0"));
        // a jarexec release archive: jarexec-v9.9.9-linux-x64/jarexec
        let release = dir.path().join("jarexec-release");
        fs::create_dir_all(&release).unwrap();
        let bin = dir.path().join("real-jarexec");
        fs::write(&bin, b"real launcher").unwrap();
        let archive = release.join("jarexec-v9.9.9-linux-x64.tar.gz");
        write_archive(
            &archive,
            &[Entry::new(
                "jarexec-v9.9.9-linux-x64/jarexec".into(),
                &bin,
                true,
            )],
            false,
        )
        .unwrap();
        let (sha256, _) = jarexec::hash::sha256_file(&archive).unwrap();
        fs::write(
            release.join("jarexec-v9.9.9-linux-x64.tar.gz.sha256"),
            format!("{sha256}  x\n"),
        )
        .unwrap();
        let text = fs::read_to_string(&def).unwrap().replace(
            "platforms = [\"linux-x64\", \"windows-x64\"]",
            &format!(
                "version = \"9.9.9\"\nurl = \"{}/jarexec-v{{version}}-{{platform}}.{{ext}}\"\nplatforms = [\"linux-x64\"]",
                source::locate(release.to_str().unwrap(), None).unwrap()
            ),
        );
        fs::write(&def, text).unwrap();
        let mut opts = options(&def, &dir.path().join("dist"), &bin);
        opts.jarexec.clear();

        run(&opts).unwrap();
        let out = dir.path().join("dist/demo-1.0-linux-x64.tar.gz");
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(File::open(out).unwrap()));
        let mut launcher = tar
            .entries()
            .unwrap()
            .map(|e| e.unwrap())
            .find(|e| e.path().unwrap() == Path::new("tool-a"))
            .unwrap();
        let mut content = Vec::new();
        io::Read::read_to_end(&mut launcher, &mut content).unwrap();
        assert_eq!(content, b"real launcher");

        fs::write(
            release.join("jarexec-v9.9.9-linux-x64.tar.gz.sha256"),
            "00 x\n",
        )
        .unwrap();
        let err = run(&opts).unwrap_err();
        assert!(format!("{err:#}").contains("does not match"), "{err:#}");
    }
}
