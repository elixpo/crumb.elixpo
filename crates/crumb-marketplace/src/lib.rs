//! Validated, scoped installation for Crumb marketplace packages.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::DateTime;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::Builder;
use url::Url;

const MAX_ARTIFACT_BYTES: usize = 256 * 1024;
const MAX_PACKAGE_BYTES: usize = 2 * 1024 * 1024;
const BUNDLED_CATALOG: &[u8] = include_bytes!("../../../marketplace/catalog.json");
#[cfg(test)]
const PUBLIC_CATALOG: &[u8] =
    include_bytes!("../../../crumb.elixpo/public/marketplace/catalog.json");
#[cfg(test)]
const CONTRIBUTOR_SCHEMA: &[u8] = include_bytes!("../../../marketplace/schema/catalog.schema.json");
#[cfg(test)]
const PUBLIC_SCHEMA: &[u8] =
    include_bytes!("../../../crumb.elixpo/public/marketplace/schema/catalog.schema.json");

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

/// Operating system supported by one package release.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketplacePlatform {
    Linux,
    Macos,
    Windows,
}

/// Crumb and platform compatibility declared before installation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Compatibility {
    pub minimum_crumb_version: String,
    pub maximum_crumb_version_exclusive: Option<String>,
    pub platforms: BTreeSet<MarketplacePlatform>,
}

/// Stable publisher identity bound to a catalog namespace.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PublisherIdentity {
    pub issuer: String,
    pub subject: String,
}

/// Publisher metadata displayed before a package is installed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Publisher {
    pub id: String,
    pub display_name: String,
    pub source_url: String,
    pub identity: PublisherIdentity,
}

/// How one immutable release is expected to be authenticated.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReleaseTrust {
    Bundled,
    SigstoreBundle { bundle_url: String },
}

/// Immutable source and release identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseProvenance {
    pub repository: String,
    pub revision: String,
    pub released_at: String,
    pub trust: ReleaseTrust,
}

/// User-facing security and data-flow disclosures.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Disclosures {
    pub security_url: String,
    pub privacy_url: Option<String>,
    pub data_flow: String,
}

/// External connector and exact OAuth scopes required by a package.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorRequirement {
    pub id: String,
    #[serde(default)]
    pub scopes: BTreeSet<String>,
    pub required: bool,
}

/// Optional migration guidance for an old package release.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Deprecation {
    pub message: String,
    pub successor: Option<String>,
}

/// Catalog-level emergency block for one immutable release.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Revocation {
    pub package: String,
    pub version: String,
    pub reason: String,
    pub revoked_at: String,
}

/// File copied from a package source into the immutable package cache.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub path: PathBuf,
    pub sha256: String,
    pub size_bytes: u64,
    pub url: Option<String>,
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
    pub transport: McpTransport,
    pub command: Option<String>,
    pub url: Option<String>,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub environment: Vec<String>,
}

/// Transport required by a marketplace MCP server.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    StreamableHttp,
}

/// Public metadata and immutable contents for one package release.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub id: String,
    pub publisher: String,
    pub version: String,
    pub kind: PackageKind,
    pub display_name: String,
    pub description: String,
    pub license: String,
    pub source: String,
    pub compatibility: Compatibility,
    pub provenance: ReleaseProvenance,
    pub disclosures: Disclosures,
    #[serde(default)]
    pub connector_requirements: Vec<ConnectorRequirement>,
    pub deprecation: Option<Deprecation>,
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
    pub publishers: Vec<Publisher>,
    #[serde(default)]
    pub revocations: Vec<Revocation>,
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
        self.packages
            .iter()
            .filter(|package| package.id == id)
            .max_by(|left, right| {
                let left = Version::parse(&left.version).ok();
                let right = Version::parse(&right.version).ok();
                left.cmp(&right)
            })
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != 2 || !valid_slug(&self.name) {
            bail!("unsupported marketplace schema or invalid marketplace name");
        }
        if self.publishers.is_empty() {
            bail!("marketplace catalog has no publishers");
        }
        let mut publishers = BTreeMap::new();
        for publisher in &self.publishers {
            publisher.validate()?;
            if publishers
                .insert(publisher.id.as_str(), publisher)
                .is_some()
            {
                bail!("duplicate marketplace publisher `{}`", publisher.id);
            }
        }
        let mut packages = BTreeSet::new();
        for package in &self.packages {
            package.validate()?;
            let namespace = package.id.split_once('/').map(|(namespace, _)| namespace);
            if namespace != Some(package.publisher.as_str())
                || !publishers.contains_key(package.publisher.as_str())
            {
                bail!(
                    "package `{}` has an unknown publisher namespace",
                    package.id
                );
            }
            if !packages.insert((package.id.as_str(), package.version.as_str())) {
                bail!(
                    "duplicate marketplace package release `{}@{}`",
                    package.id,
                    package.version
                );
            }
        }
        let mut revocations = BTreeSet::new();
        for revocation in &self.revocations {
            revocation.validate()?;
            let release = (revocation.package.as_str(), revocation.version.as_str());
            if !revocations.insert(release) {
                bail!("marketplace repeats a revoked release");
            }
        }
        Ok(())
    }

    /// Returns whether an immutable package release has been revoked.
    #[must_use]
    pub fn is_revoked(&self, package: &Package) -> bool {
        self.revocations.iter().any(|revocation| {
            revocation.package == package.id && revocation.version == package.version
        })
    }

    /// Finds an installable, compatible, non-revoked package release.
    ///
    /// # Errors
    ///
    /// Returns an error when the package is missing, revoked, or incompatible.
    pub fn installable_package(&self, id: &str) -> Result<&Package> {
        let package = self
            .package(id)
            .with_context(|| format!("marketplace package `{id}` was not found"))?;
        if self.is_revoked(package) {
            bail!("marketplace package `{id}` is revoked");
        }
        if !package.compatibility.supports_current() {
            bail!("marketplace package `{id}` is incompatible with this Crumb build");
        }
        Ok(package)
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
            || !valid_slug(&self.publisher)
            || Version::parse(&self.version).is_err()
            || self.display_name.trim().is_empty()
            || self.description.trim().is_empty()
            || self.license.trim().is_empty()
            || self.source.trim().is_empty()
        {
            bail!("package metadata is incomplete or invalid");
        }
        self.compatibility.validate()?;
        self.provenance.validate()?;
        self.disclosures.validate()?;
        match self.provenance.trust {
            ReleaseTrust::Bundled
                if self.publisher != "crumb" || !self.source.starts_with("./packages/") =>
            {
                bail!("package `{}` has invalid bundled provenance", self.id);
            }
            ReleaseTrust::SigstoreBundle { .. } if !valid_https_url(&self.source) => {
                bail!("package `{}` must use an HTTPS source", self.id);
            }
            _ => {}
        }
        if let Some(deprecation) = &self.deprecation {
            deprecation.validate()?;
        }
        let mut connectors = BTreeSet::new();
        for connector in &self.connector_requirements {
            connector.validate()?;
            if !connectors.insert(connector.id.as_str()) {
                bail!("package `{}` repeats a connector requirement", self.id);
            }
        }
        let paths = self.validate_artifacts()?;
        self.validate_skills(&paths)?;
        self.validate_mcp_servers()?;
        if self.skills.is_empty() && self.mcp_servers.is_empty() {
            bail!("package `{}` exposes no skills or MCP servers", self.id);
        }
        Ok(())
    }

    fn validate_artifacts(&self) -> Result<BTreeSet<&Path>> {
        let mut paths = BTreeSet::new();
        let mut declared_bytes = 0_u64;
        for artifact in &self.artifacts {
            validate_relative_path(&artifact.path)?;
            if artifact.sha256.len() != 64
                || !artifact.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                || artifact.size_bytes == 0
                || artifact.size_bytes > MAX_ARTIFACT_BYTES as u64
                || !paths.insert(artifact.path.as_path())
            {
                bail!("package `{}` has an invalid artifact", self.id);
            }
            declared_bytes = declared_bytes.saturating_add(artifact.size_bytes);
            if declared_bytes > MAX_PACKAGE_BYTES as u64 {
                bail!("package `{}` exceeds the declared size limit", self.id);
            }
            if artifact
                .url
                .as_deref()
                .is_some_and(|url| !valid_https_url(url))
            {
                bail!("package `{}` has an invalid artifact URL", self.id);
            }
            if matches!(self.provenance.trust, ReleaseTrust::SigstoreBundle { .. })
                && !artifact.url.as_deref().is_some_and(valid_https_url)
            {
                bail!("package `{}` has an invalid remote artifact URL", self.id);
            }
        }
        Ok(paths)
    }

    fn validate_skills(&self, paths: &BTreeSet<&Path>) -> Result<()> {
        let mut skills = BTreeSet::new();
        for skill in &self.skills {
            if !valid_component_id(&skill.id) {
                bail!("package `{}` has an invalid skill identifier", self.id);
            }
            if !skills.insert(skill.id.as_str()) {
                bail!("package `{}` repeats a skill identifier", self.id);
            }
            validate_relative_path(&skill.path)?;
            if !paths.contains(skill.path.as_path()) {
                bail!(
                    "skill `{}` does not reference a declared artifact",
                    skill.id
                );
            }
        }
        Ok(())
    }

    fn validate_mcp_servers(&self) -> Result<()> {
        let mut servers = BTreeSet::new();
        for server in &self.mcp_servers {
            if !valid_component_id(&server.id)
                || server.environment.iter().any(|name| !valid_env_name(name))
            {
                bail!("package `{}` has invalid MCP metadata", self.id);
            }
            if !servers.insert(server.id.as_str()) {
                bail!("package `{}` repeats an MCP identifier", self.id);
            }
            match server.transport {
                McpTransport::Stdio
                    if server
                        .command
                        .as_deref()
                        .is_none_or(|command| command.trim().is_empty())
                        || server.url.is_some() =>
                {
                    bail!("package `{}` has invalid STDIO MCP metadata", self.id);
                }
                McpTransport::StreamableHttp
                    if !server.url.as_deref().is_some_and(valid_https_url)
                        || server.command.is_some()
                        || !server.arguments.is_empty()
                        || !server.environment.is_empty() =>
                {
                    bail!("package `{}` has invalid remote MCP metadata", self.id);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

impl Compatibility {
    fn validate(&self) -> Result<()> {
        let minimum = Version::parse(&self.minimum_crumb_version)
            .context("marketplace minimum Crumb version is invalid")?;
        if self.platforms.is_empty() {
            bail!("marketplace compatibility has no supported platforms");
        }
        if let Some(maximum) = &self.maximum_crumb_version_exclusive {
            let maximum =
                Version::parse(maximum).context("marketplace maximum Crumb version is invalid")?;
            if maximum <= minimum {
                bail!("marketplace Crumb version range is invalid");
            }
        }
        Ok(())
    }

    fn supports_current(&self) -> bool {
        let Ok(current) = Version::parse(env!("CARGO_PKG_VERSION")) else {
            return false;
        };
        let Ok(minimum) = Version::parse(&self.minimum_crumb_version) else {
            return false;
        };
        let below_maximum = self
            .maximum_crumb_version_exclusive
            .as_deref()
            .map(Version::parse)
            .transpose()
            .is_ok_and(|maximum| maximum.is_none_or(|maximum| current < maximum));
        current >= minimum && below_maximum && self.platforms.contains(&current_platform())
    }
}

impl Publisher {
    fn validate(&self) -> Result<()> {
        if !valid_slug(&self.id)
            || self.display_name.trim().is_empty()
            || !valid_https_url(&self.source_url)
            || !valid_https_url(&self.identity.issuer)
            || self.identity.subject.trim().is_empty()
            || self.identity.subject.chars().any(char::is_control)
        {
            bail!("marketplace publisher metadata is invalid");
        }
        Ok(())
    }
}

impl ReleaseProvenance {
    fn validate(&self) -> Result<()> {
        if !valid_https_url(&self.repository)
            || !matches!(self.revision.len(), 40 | 64)
            || !self.revision.bytes().all(|byte| byte.is_ascii_hexdigit())
            || DateTime::parse_from_rfc3339(&self.released_at).is_err()
        {
            bail!("marketplace release provenance is invalid");
        }
        if let ReleaseTrust::SigstoreBundle { bundle_url } = &self.trust
            && !valid_https_url(bundle_url)
        {
            bail!("marketplace Sigstore bundle URL is invalid");
        }
        Ok(())
    }
}

impl Disclosures {
    fn validate(&self) -> Result<()> {
        if !valid_https_url(&self.security_url)
            || self
                .privacy_url
                .as_deref()
                .is_some_and(|url| !valid_https_url(url))
            || self.data_flow.trim().is_empty()
            || self.data_flow.chars().any(char::is_control)
        {
            bail!("marketplace package disclosures are invalid");
        }
        Ok(())
    }
}

impl ConnectorRequirement {
    fn validate(&self) -> Result<()> {
        if !valid_component_id(&self.id)
            || self.scopes.iter().any(|scope| {
                scope.is_empty()
                    || scope.len() > 256
                    || !scope.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric()
                            || matches!(byte, b'.' | b'_' | b'-' | b':' | b'/')
                    })
            })
        {
            bail!("marketplace connector requirement is invalid");
        }
        Ok(())
    }
}

impl Deprecation {
    fn validate(&self) -> Result<()> {
        if self.message.trim().is_empty()
            || self.message.chars().any(char::is_control)
            || self
                .successor
                .as_deref()
                .is_some_and(|successor| !valid_package_id(successor))
        {
            bail!("marketplace deprecation metadata is invalid");
        }
        Ok(())
    }
}

impl Revocation {
    fn validate(&self) -> Result<()> {
        if !valid_package_id(&self.package)
            || Version::parse(&self.version).is_err()
            || self.reason.trim().is_empty()
            || self.reason.chars().any(char::is_control)
            || DateTime::parse_from_rfc3339(&self.revoked_at).is_err()
        {
            bail!("marketplace revocation metadata is invalid");
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
            verify_package_files(package, &package_root)?;
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
            if bytes.len() > MAX_ARTIFACT_BYTES
                || u64::try_from(bytes.len()).unwrap_or(u64::MAX) != artifact.size_bytes
            {
                bail!(
                    "package artifact `{}` has an invalid size",
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

/// Verifies declared files in a package source or installed cache.
///
/// # Errors
///
/// Returns an error for missing, linked, oversized, or digest-mismatched artifacts.
pub fn verify_package_files(package: &Package, root: &Path) -> Result<()> {
    let mut total_bytes = 0_usize;
    for artifact in &package.artifacts {
        let path = root.join(&artifact.path);
        let metadata = fs::symlink_metadata(&path).with_context(|| {
            format!(
                "installed artifact `{}` is unavailable",
                artifact.path.display()
            )
        })?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_ARTIFACT_BYTES as u64 {
            bail!(
                "installed artifact `{}` is not a bounded file",
                artifact.path.display()
            );
        }
        let bytes = fs::read(path)?;
        total_bytes = total_bytes.saturating_add(bytes.len());
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != artifact.size_bytes
            || total_bytes > MAX_PACKAGE_BYTES
            || !format!("{:x}", Sha256::digest(&bytes)).eq_ignore_ascii_case(&artifact.sha256)
        {
            bail!(
                "installed package `{}` failed integrity verification",
                package.id
            );
        }
    }
    Ok(())
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

fn valid_env_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_https_url(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn current_platform() -> MarketplacePlatform {
    match std::env::consts::OS {
        "macos" => MarketplacePlatform::Macos,
        "windows" => MarketplacePlatform::Windows,
        _ => MarketplacePlatform::Linux,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CONTRIBUTOR_SCHEMA, Catalog, Installer, PUBLIC_CATALOG, PUBLIC_SCHEMA, Revocation,
        bundled_catalog,
    };
    use sha2::{Digest, Sha256};
    use std::fs;
    use tempfile::tempdir;

    fn catalog_json(hash: &str, path: &str) -> String {
        format!(
            r#"{{"schema_version":2,"name":"crumb-public","publishers":[{{"id":"crumb","display_name":"Crumb","source_url":"https://example.test/crumb","identity":{{"issuer":"https://issuer.example.test","subject":"repo:crumb/test"}}}}],"revocations":[],"packages":[{{"id":"crumb/review","publisher":"crumb","version":"1.0.0","kind":"skill","display_name":"Review","description":"Review code","license":"MIT","source":"./packages/review","compatibility":{{"minimum_crumb_version":"0.1.0","maximum_crumb_version_exclusive":"0.2.0","platforms":["linux","macos","windows"]}},"provenance":{{"repository":"https://example.test/review","revision":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","released_at":"2026-10-17T00:00:00Z","trust":{{"kind":"bundled"}}}},"disclosures":{{"security_url":"https://example.test/security","privacy_url":null,"data_flow":"local only"}},"connector_requirements":[],"deprecation":null,"artifacts":[{{"path":"{path}","sha256":"{hash}","size_bytes":9,"url":null}}],"skills":[{{"id":"review","path":"{path}"}}]}}]}}"#
        )
    }

    fn catalog(hash: &str, path: &str) -> Catalog {
        Catalog::parse(catalog_json(hash, path).as_bytes()).expect("catalog should be valid")
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
        let traversal = catalog_json(&"a".repeat(64), "../SKILL.md");
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
    fn rejects_unknown_publishers_and_insecure_remote_metadata() {
        let mut value: serde_json::Value =
            serde_json::from_str(&catalog_json(&"a".repeat(64), "SKILL.md")).expect("catalog JSON");
        value["packages"][0]["publisher"] = serde_json::json!("unknown");
        assert!(Catalog::parse(value.to_string().as_bytes()).is_err());

        let mut value: serde_json::Value =
            serde_json::from_str(&catalog_json(&"a".repeat(64), "SKILL.md")).expect("catalog JSON");
        value["packages"][0]["provenance"]["trust"] = serde_json::json!({
            "kind": "sigstore_bundle",
            "bundle_url": "http://example.test/bundle.json"
        });
        value["packages"][0]["source"] = serde_json::json!("http://example.test/review");
        assert!(Catalog::parse(value.to_string().as_bytes()).is_err());
    }

    #[test]
    fn revoked_and_incompatible_releases_cannot_be_installed() {
        let mut catalog = catalog(&"a".repeat(64), "SKILL.md");
        catalog.revocations.push(Revocation {
            package: "crumb/review".to_owned(),
            version: "1.0.0".to_owned(),
            reason: "compromised release".to_owned(),
            revoked_at: "2026-10-17T00:00:00Z".to_owned(),
        });
        assert!(catalog.installable_package("crumb/review").is_err());

        catalog.revocations.clear();
        catalog.packages[0].compatibility.minimum_crumb_version = "9.0.0".to_owned();
        assert!(catalog.installable_package("crumb/review").is_err());
    }

    #[test]
    fn bundled_catalog_and_artifacts_are_in_sync() {
        let catalog = bundled_catalog().expect("bundled catalog should be valid");
        assert_eq!(
            catalog,
            Catalog::parse(PUBLIC_CATALOG).expect("public catalog should be valid")
        );
        let cache = tempdir().expect("cache tempdir");
        let installer = Installer::new(cache.path());
        for package in catalog.packages {
            installer
                .install_bundled(&package)
                .expect("bundled package should install");
        }
        let contributor_schema: serde_json::Value =
            serde_json::from_slice(CONTRIBUTOR_SCHEMA).expect("contributor schema should parse");
        let public_schema: serde_json::Value =
            serde_json::from_slice(PUBLIC_SCHEMA).expect("public schema should parse");
        assert_eq!(contributor_schema, public_schema);
    }
}
