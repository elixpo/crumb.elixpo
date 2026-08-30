//! Validated, scoped installation for Crumb marketplace packages.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::Builder;

const MAX_ARTIFACT_BYTES: usize = 256 * 1024;
const MAX_PACKAGE_BYTES: usize = 2 * 1024 * 1024;
const BUNDLED_CATALOG: &[u8] = include_bytes!("../../../marketplace/catalog.json");

/// Install location selected by the user.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallScope {
    Project,
    User,
}

/// One installable package category.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageKind {
    Skill,
    Mcp,
    Bundle,
}

/// Capability declared by a package before it can be enabled.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    WorkspaceRead,
    WorkspaceWrite,
    ProcessExecution,
    NetworkAccess,
}

/// File copied from a package source into the immutable package cache.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub path: PathBuf,
    pub sha256: String,
}

/// Skill entry exposed after package installation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillEntry {
    pub id: String,
    pub path: PathBuf,
}

/// MCP launch metadata. Environment values are intentionally not representable.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpEntry {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub environment: Vec<String>,
}

/// Public metadata and immutable contents for one package release.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub id: String,
    pub version: String,
    pub kind: PackageKind,
    pub display_name: String,
    pub description: String,
    pub license: String,
    pub source: String,
    #[serde(default)]
    pub capabilities: BTreeSet<Capability>,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    #[serde(default)]
    pub skills: Vec<SkillEntry>,
    #[serde(default)]
    pub mcp_servers: Vec<McpEntry>,
}

/// Marketplace index. Catalogs describe packages but never grant capabilities.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub schema_version: u32,
    pub name: String,
    pub packages: Vec<Package>,
}

impl Catalog {
    /// Parses and validates a marketplace catalog.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed, duplicate, unsafe, or unsupported metadata.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let catalog: Self = serde_json::from_slice(bytes).context("invalid marketplace catalog")?;
        catalog.validate()?;
        Ok(catalog)
    }

    /// Finds an exact package identifier.
    #[must_use]
    pub fn package(&self, id: &str) -> Option<&Package> {
        self.packages.iter().find(|package| package.id == id)
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != 1 || !valid_slug(&self.name) {
            bail!("unsupported marketplace schema or invalid marketplace name");
        }
        let mut packages = BTreeSet::new();
        for package in &self.packages {
            package.validate()?;
            if !packages.insert(package.id.as_str()) {
                bail!("duplicate marketplace package `{}`", package.id);
            }
        }
        Ok(())
    }
}

/// Returns the public catalog shipped with this Crumb build.
///
/// # Errors
///
/// Returns an error when build-time catalog metadata is invalid.
pub fn bundled_catalog() -> Result<Catalog> {
    Catalog::parse(BUNDLED_CATALOG)
}

impl Package {
    fn validate(&self) -> Result<()> {
        if !valid_package_id(&self.id)
            || !valid_version(&self.version)
            || self.display_name.trim().is_empty()
            || self.description.trim().is_empty()
            || self.license.trim().is_empty()
            || self.source.trim().is_empty()
        {
            bail!("package metadata is incomplete or invalid");
        }
        let mut paths = BTreeSet::new();
        for artifact in &self.artifacts {
            validate_relative_path(&artifact.path)?;
            if artifact.sha256.len() != 64
                || !artifact.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                || !paths.insert(artifact.path.as_path())
            {
                bail!("package `{}` has an invalid artifact", self.id);
            }
        }
        for skill in &self.skills {
            if !valid_component_id(&skill.id) {
                bail!("package `{}` has an invalid skill identifier", self.id);
            }
            validate_relative_path(&skill.path)?;
            if !paths.contains(skill.path.as_path()) {
                bail!(
                    "skill `{}` does not reference a declared artifact",
                    skill.id
                );
            }
        }
        for server in &self.mcp_servers {
            if !valid_component_id(&server.id)
                || server.command.trim().is_empty()
                || server.environment.iter().any(|name| !valid_env_name(name))
            {
                bail!("package `{}` has invalid MCP metadata", self.id);
            }
        }
        if self.skills.is_empty() && self.mcp_servers.is_empty() {
            bail!("package `{}` exposes no skills or MCP servers", self.id);
        }
        Ok(())
    }
}

/// Installed package metadata returned to the CLI configuration boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledPackage {
    pub root: PathBuf,
    pub package: Package,
}

/// Installs packages from an already-fetched, trusted source directory.
pub struct Installer {
    destination_root: PathBuf,
}

impl Installer {
    #[must_use]
    pub fn new(destination_root: impl Into<PathBuf>) -> Self {
        Self {
            destination_root: destination_root.into(),
        }
    }

    /// Verifies every declared artifact and atomically publishes the package.
    ///
    /// Installation does not enable skills, start MCP processes, or grant capabilities.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid metadata, unsafe paths, digest mismatches, size
    /// limit violations, or filesystem failures.
    pub fn install(&self, package: &Package, source_root: &Path) -> Result<InstalledPackage> {
        self.install_from(package, |artifact| {
            let source = source_root.join(&artifact.path);
            let metadata = fs::symlink_metadata(&source)
                .with_context(|| format!("missing package artifact {}", artifact.path.display()))?;
            if !metadata.file_type().is_file() || metadata.len() > MAX_ARTIFACT_BYTES as u64 {
                bail!(
                    "package artifact `{}` is not a bounded file",
                    artifact.path.display()
                );
            }
            fs::read(source).map_err(Into::into)
        })
    }

    /// Installs an artifact set embedded in the Crumb binary.
    ///
    /// # Errors
    ///
    /// Returns an error when the package is not bundled or verification fails.
    pub fn install_bundled(&self, package: &Package) -> Result<InstalledPackage> {
        self.install_from(package, |artifact| {
            bundled_artifact(&package.id, &artifact.path)
                .map(<[u8]>::to_vec)
                .with_context(|| {
                    format!(
                        "package artifact `{}` is not bundled in this build",
                        artifact.path.display()
                    )
                })
        })
    }

    fn install_from<F>(&self, package: &Package, mut read: F) -> Result<InstalledPackage>
    where
        F: FnMut(&Artifact) -> Result<Vec<u8>>,
    {
        package.validate()?;
        fs::create_dir_all(&self.destination_root).with_context(|| {
            format!(
                "failed to create marketplace cache {}",
                self.destination_root.display()
            )
        })?;
        let package_root = self
            .destination_root
            .join(package.id.replace('/', "--"))
            .join(&package.version);
        if package_root.exists() {
            return Ok(InstalledPackage {
                root: package_root,
                package: package.clone(),
            });
        }
        let parent = package_root
            .parent()
            .context("package destination has no parent")?;
        fs::create_dir_all(parent)?;
        let staging = Builder::new().prefix(".install-").tempdir_in(parent)?;
        let mut total_bytes = 0_usize;
        for artifact in &package.artifacts {
            let bytes = read(artifact)?;
            if bytes.len() > MAX_ARTIFACT_BYTES {
                bail!(
                    "package artifact `{}` exceeds the size limit",
                    artifact.path.display()
                );
            }
            total_bytes = total_bytes.saturating_add(bytes.len());
            if total_bytes > MAX_PACKAGE_BYTES {
                bail!("package `{}` exceeds the install size limit", package.id);
            }
            let actual = format!("{:x}", Sha256::digest(&bytes));
            if !actual.eq_ignore_ascii_case(&artifact.sha256) {
                bail!(
                    "package artifact `{}` failed integrity verification",
                    artifact.path.display()
                );
            }
            let destination = staging.path().join(&artifact.path);
            if let Some(directory) = destination.parent() {
                fs::create_dir_all(directory)?;
            }
            fs::write(destination, bytes)?;
        }
        fs::rename(staging.keep(), &package_root)?;
        Ok(InstalledPackage {
            root: package_root,
            package: package.clone(),
        })
    }
}

fn bundled_artifact(package_id: &str, path: &Path) -> Option<&'static [u8]> {
    match (package_id, path.to_str()) {
        ("crumb/code-review", Some("skills/code-review/SKILL.md")) => Some(include_bytes!(
            "../../../marketplace/packages/code-review/skills/code-review/SKILL.md"
        )),
        ("crumb/rust-quality", Some("skills/rust-quality/SKILL.md")) => Some(include_bytes!(
            "../../../marketplace/packages/rust-quality/skills/rust-quality/SKILL.md"
        )),
        ("crumb/workspace-mcp", Some("README.md")) => Some(include_bytes!(
            "../../../marketplace/packages/workspace-mcp/README.md"
        )),
        _ => None,
    }
}

fn validate_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("marketplace paths must be safe relative paths");
    }
    Ok(())
}

fn valid_package_id(value: &str) -> bool {
    let mut parts = value.split('/');
    matches!((parts.next(), parts.next(), parts.next()), (Some(namespace), Some(name), None) if valid_slug(namespace) && valid_slug(name))
}

fn valid_component_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
}

fn valid_env_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::{Catalog, Installer, bundled_catalog};
    use sha2::{Digest, Sha256};
    use std::fs;
    use tempfile::tempdir;

    fn catalog(hash: &str, path: &str) -> Catalog {
        Catalog::parse(
            format!(
                r#"{{"schema_version":1,"name":"crumb-public","packages":[{{"id":"crumb/review","version":"1.0.0","kind":"skill","display_name":"Review","description":"Review code","license":"MIT","source":"https://example.test/review","artifacts":[{{"path":"{path}","sha256":"{hash}"}}],"skills":[{{"id":"review","path":"{path}"}}]}}]}}"#
            )
            .as_bytes(),
        )
        .expect("catalog should be valid")
    }

    #[test]
    fn installs_verified_package_without_enabling_it() {
        let source = tempdir().expect("source tempdir");
        fs::create_dir(source.path().join("skills")).expect("skill directory");
        let body = b"# Review\n";
        fs::write(source.path().join("skills/SKILL.md"), body).expect("skill fixture");
        let hash = format!("{:x}", Sha256::digest(body));
        let package = catalog(&hash, "skills/SKILL.md").packages.remove(0);
        let cache = tempdir().expect("cache tempdir");
        let installed = Installer::new(cache.path())
            .install(&package, source.path())
            .expect("package should install");
        assert_eq!(
            fs::read_to_string(installed.root.join("skills/SKILL.md")).expect("installed skill"),
            "# Review\n"
        );
    }

    #[test]
    fn rejects_path_traversal_and_digest_mismatch() {
        let traversal = "{\"schema_version\":1,\"name\":\"crumb-public\",\"packages\":[{\"id\":\"crumb/review\",\"version\":\"1\",\"kind\":\"skill\",\"display_name\":\"Review\",\"description\":\"Review code\",\"license\":\"MIT\",\"source\":\"local\",\"artifacts\":[{\"path\":\"../SKILL.md\",\"sha256\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}],\"skills\":[{\"id\":\"review\",\"path\":\"../SKILL.md\"}]}]}";
        assert!(Catalog::parse(traversal.as_bytes()).is_err());

        let source = tempdir().expect("source tempdir");
        fs::write(source.path().join("SKILL.md"), "changed").expect("skill fixture");
        let package = catalog(&"a".repeat(64), "SKILL.md").packages.remove(0);
        let cache = tempdir().expect("cache tempdir");
        assert!(
            Installer::new(cache.path())
                .install(&package, source.path())
                .is_err()
        );
    }

    #[test]
    fn bundled_catalog_and_artifacts_are_in_sync() {
        let catalog = bundled_catalog().expect("bundled catalog should be valid");
        let cache = tempdir().expect("cache tempdir");
        let installer = Installer::new(cache.path());
        for package in catalog.packages {
            installer
                .install_bundled(&package)
                .expect("bundled package should install");
        }
    }
}
