//! Where suite files come from: `http(s)://` and `file://` URLs or plain paths.
//! A relative reference is resolved against the document that names it, so a
//! manifest can point to archives next to it wherever it is hosted.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{BufWriter, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Url;

/// `reference` as a URL: absolute URLs and paths as they are, anything else
/// relative to `base` (or to the current directory without one).
pub fn locate(reference: &str, base: Option<&Url>) -> Result<Url> {
    if reference.contains("://") {
        return Url::parse(reference).with_context(|| format!("invalid URL {reference}"));
    }
    let path = Path::new(reference);
    match base {
        Some(base) if !path.is_absolute() => base
            .join(reference)
            .with_context(|| format!("resolving {reference} against {base}")),
        _ => {
            let abs =
                std::path::absolute(path).with_context(|| format!("invalid path {reference}"))?;
            Url::from_file_path(&abs).map_err(|()| anyhow!("invalid path {}", abs.display()))
        }
    }
}

/// The same URL with a trailing slash, so that `join` resolves inside it.
pub fn dir_url(mut url: Url) -> Url {
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    url
}

pub fn read_text(url: &Url) -> Result<String> {
    let (mut reader, _) = open(url)?;
    let mut text = String::new();
    reader
        .read_to_string(&mut text)
        .with_context(|| format!("reading {url}"))?;
    Ok(text)
}

/// Copies `url` to `dest` through `dest.part`, returning SHA-256 and size.
/// With `expected` (SHA-256, size) a mismatch leaves nothing behind.
pub fn download(url: &Url, dest: &Path, expected: Option<(&str, u64)>) -> Result<(String, u64)> {
    let (reader, len) = open(url)?;
    let mut part = OsString::from(dest.as_os_str());
    part.push(".part");
    let part = PathBuf::from(part);
    let label = dest.file_name().unwrap_or_default().to_string_lossy();
    let bar = jarexec::http::progress_bar(expected.map(|e| e.1).or(len).unwrap_or(0), &label);
    let copied = File::create(&part)
        .map_err(anyhow::Error::from)
        .and_then(|file| {
            Ok(jarexec::hash::copy_hashed(
                reader,
                BufWriter::new(file),
                |n| bar.inc(n),
            )?)
        });
    bar.finish_and_clear();
    let (sha256, size) = match copied {
        Ok(done) => done,
        Err(e) => {
            let _ = fs::remove_file(&part);
            return Err(e).with_context(|| format!("downloading {url}"));
        }
    };
    if let Some((want_sha, want_size)) = expected
        && (sha256 != want_sha || size != want_size)
    {
        let _ = fs::remove_file(&part);
        bail!(
            "{url}: expected SHA-256 {want_sha} ({want_size} bytes), got {sha256} ({size} bytes)"
        );
    }
    fs::rename(&part, dest).with_context(|| format!("writing {}", dest.display()))?;
    Ok((sha256, size))
}

fn open(url: &Url) -> Result<(Box<dyn Read>, Option<u64>)> {
    match url.scheme() {
        "file" => {
            let path = url
                .to_file_path()
                .map_err(|()| anyhow!("invalid file URL {url}"))?;
            let file = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
            let len = file.metadata().map(|m| m.len()).ok();
            Ok((Box::new(file), len))
        }
        "http" | "https" => {
            let response = jarexec::http::client()?
                .get(url.clone())
                .send()
                .and_then(|r| r.error_for_status())
                .with_context(|| format!("downloading {url}"))?;
            let len = response.content_length();
            Ok((Box::new(response), len))
        }
        other => bail!("unsupported URL scheme {other}: {url}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_references() {
        let base = Url::parse("https://example.com/suite/suite.toml").unwrap();
        assert_eq!(
            locate("a-1.zip", Some(&base)).unwrap().as_str(),
            "https://example.com/suite/a-1.zip"
        );
        assert_eq!(
            locate("http://x.org/a.zip", Some(&base)).unwrap().as_str(),
            "http://x.org/a.zip"
        );
        let file = locate("suite.toml", None).unwrap();
        assert_eq!(file.scheme(), "file");
        assert!(file.path().ends_with("/suite.toml"));
        let dir = dir_url(Url::parse("https://example.com/tools/a").unwrap());
        assert_eq!(
            dir.join("a.lock").unwrap().as_str(),
            "https://example.com/tools/a/a.lock"
        );
    }

    #[test]
    fn download_checks_the_expected_hash() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        fs::write(&src, b"abc").unwrap();
        let url = locate(src.to_str().unwrap(), None).unwrap();
        let dest = dir.path().join("dest.bin");
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

        assert!(download(&url, &dest, Some(("0000", 3))).is_err());
        assert!(!dest.exists() && !dir.path().join("dest.bin.part").exists());
        assert_eq!(
            download(&url, &dest, Some((abc, 3))).unwrap(),
            (abc.into(), 3)
        );
        assert_eq!(fs::read(&dest).unwrap(), b"abc");
    }

    #[test]
    fn reads_over_http() {
        let server = crate::test_util::serve(&[("/suite.toml", b"version = 1")]);
        let url = Url::parse(&format!("{}/suite.toml", server.url)).unwrap();
        assert_eq!(read_text(&url).unwrap(), "version = 1");
        let missing = Url::parse(&format!("{}/nope", server.url)).unwrap();
        assert!(read_text(&missing).is_err());
    }
}
