//! The two suite files: `suite-def.toml`, written by hand in the suite repository,
//! and `suite.toml`, the manifest `assemble` publishes and clients install from.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Result, ensure};
use jarexec::platform::{Os, Platform};
use serde::{Deserialize, Serialize};

pub const FORMAT_VERSION: u32 = 1;
/// Platforms jarexec is released for, with its archive names.
pub const PLATFORMS: [&str; 5] = [
    "linux-x64",
    "linux-aarch64",
    "windows-x64",
    "mac-aarch64",
    "mac-x64",
];
pub const JAREXEC_URL: &str = "https://github.com/npedot/jarexec/releases/download/v{version}/jarexec-v{version}-{platform}.{ext}";

/// `suite-def.toml`: which tool versions make up a release.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteDef {
    pub suite: SuiteInfo,
    #[serde(default)]
    pub align: Align,
    #[serde(default)]
    pub jarexec: JarexecDef,
    #[serde(rename = "tool")]
    pub tools: Vec<ToolDef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteInfo {
    pub name: String,
    pub release: String,
    /// Format of the data the tools share. A client never goes back to a lower one
    /// without `--force`: the data may already have been upgraded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_format: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Align {
    /// Dependencies every tool must use in the same version (`jarexec lock --bundle`
    /// syntax). Other version differences are only reported.
    #[serde(default)]
    pub required: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct JarexecDef {
    /// jarexec release whose binaries become the tools' launchers.
    pub version: Option<String>,
    /// Archive URL, with `{version}`, `{platform}` and `{ext}`; `.sha256` is appended for the checksum.
    pub url: String,
    pub platforms: Vec<String>,
}

impl Default for JarexecDef {
    fn default() -> Self {
        Self {
            version: None,
            url: JAREXEC_URL.into(),
            platforms: PLATFORMS.iter().map(|p| p.to_string()).collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDef {
    pub name: String,
    /// Folder or URL holding `<name>.lock`, the JAR and `lib/`, as `jarexec lock` leaves them.
    /// `{platform}` in it stands for each platform, for tools whose lock differs by
    /// platform (native libraries).
    pub source: String,
}

impl SuiteDef {
    pub fn parse(text: &str) -> Result<Self> {
        let def: Self = toml::from_str(text)?;
        def.suite.validate()?;
        ensure!(!def.tools.is_empty(), "the suite has no [[tool]]");
        let mut seen = HashSet::new();
        for tool in &def.tools {
            check_name("tool", &tool.name)?;
            ensure!(
                seen.insert(&tool.name),
                "tool {} is listed twice",
                tool.name
            );
        }
        for p in &def.jarexec.platforms {
            ensure!(
                PLATFORMS.contains(&p.as_str()),
                "unknown platform {p} (known: {})",
                PLATFORMS.join(", ")
            );
        }
        Ok(def)
    }
}

impl SuiteInfo {
    fn validate(&self) -> Result<()> {
        check_name("suite", &self.name)?;
        check_name("release", &self.release)
    }
}

/// `suite.toml`: one release, with an archive per platform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub tools: Vec<String>,
    pub suite: SuiteInfo,
    #[serde(default)]
    pub archive: BTreeMap<String, Archive>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Archive {
    /// Relative URLs are resolved against the manifest's own URL.
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self> {
        let manifest: Self = toml::from_str(text)?;
        ensure!(
            manifest.version == FORMAT_VERSION,
            "unsupported manifest version: {} (expected {FORMAT_VERSION})",
            manifest.version
        );
        manifest.suite.validate()?;
        for tool in &manifest.tools {
            check_name("tool", tool)?;
        }
        Ok(manifest)
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string(self)?)
    }
}

/// Names become file and folder names: letters, digits, `.`, `_`, `-`, not starting with `.`.
pub fn check_name(kind: &str, name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && !name.starts_with('.')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
        "invalid {kind} name {name:?}: use letters, digits, '.', '_' and '-'"
    );
    Ok(())
}

/// The running platform, named like jarexec's archives (`linux-x64`, …).
pub fn current_platform() -> Result<String> {
    let p = Platform::current()?;
    Ok(format!("{}-{}", p.os, p.arch))
}

pub fn exe_suffix(platform: &str) -> &'static str {
    if platform.starts_with(&Os::Windows.to_string()) {
        ".exe"
    } else {
        ""
    }
}

pub fn archive_ext(platform: &str) -> &'static str {
    if platform.starts_with(&Os::Windows.to_string()) {
        "zip"
    } else {
        "tar.gz"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEF: &str = r#"
        [suite]
        name = "mysuite"
        release = "2026.11.0"
        data_format = 2

        [align]
        required = ["com.example:commons"]

        [[tool]]
        name = "tool-a"
        source = "tools/tool-a"
    "#;

    #[test]
    fn parses_definition_with_defaults() {
        let def = SuiteDef::parse(DEF).unwrap();
        assert_eq!(def.suite.data_format, Some(2));
        assert_eq!(def.align.required, ["com.example:commons"]);
        assert_eq!(def.jarexec.platforms.len(), PLATFORMS.len());
        assert_eq!(def.jarexec.url, JAREXEC_URL);
        assert_eq!(def.tools[0].source, "tools/tool-a");
    }

    #[test]
    fn rejects_unsafe_names_duplicates_and_unknown_platforms() {
        assert!(SuiteDef::parse(&DEF.replace("2026.11.0", "../x")).is_err());
        let twice = format!("{DEF}\n[[tool]]\nname = \"tool-a\"\nsource = \"b\"\n");
        assert!(SuiteDef::parse(&twice).is_err());
        let platform = format!("{DEF}\n[jarexec]\nplatforms = [\"amiga\"]\n");
        assert!(SuiteDef::parse(&platform).is_err());
        assert!(check_name("tool", ".hidden").is_err());
        assert!(check_name("tool", "tool_a-1.0").is_ok());
    }

    #[test]
    fn manifest_roundtrips_through_toml() {
        let manifest = Manifest {
            version: FORMAT_VERSION,
            tools: vec!["tool-a".into(), "tool-b".into()],
            suite: SuiteDef::parse(DEF).unwrap().suite,
            archive: BTreeMap::from([(
                "linux-x64".to_string(),
                Archive {
                    url: "mysuite-2026.11.0-linux-x64.tar.gz".into(),
                    sha256: "ab".into(),
                    size: 1,
                },
            )]),
        };
        let text = manifest.to_toml().unwrap();
        assert_eq!(Manifest::parse(&text).unwrap(), manifest);
    }

    #[test]
    fn platform_specific_names() {
        assert_eq!(exe_suffix("windows-x64"), ".exe");
        assert_eq!(archive_ext("windows-x64"), "zip");
        assert_eq!(archive_ext("mac-aarch64"), "tar.gz");
        assert!(PLATFORMS.contains(&current_platform().unwrap().as_str()));
    }
}
