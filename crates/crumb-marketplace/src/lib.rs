//! Validated, scoped installation for Crumb marketplace packages.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sigstore_trust_root::{SIGSTORE_PRODUCTION_TRUSTED_ROOT, TrustedRoot};
use sigstore_types::Bundle;
use sigstore_verify::{VerificationPolicy, Verifier};
use tempfile::{Builder, NamedTempFile};
use url::{Host, Url};

const MAX_ARTIFACT_BYTES: usize = 256 * 1024;
const MAX_PACKAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_INSTALLATION_STATE_BYTES: usize = 2 * 1024 * 1024;
const MAX_INSTALLED_RELEASES: usize = 512;
/// Maximum accepted size for a downloaded marketplace catalog.
pub const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
/// Maximum accepted size for one Sigstore bundle.
pub const MAX_SIGSTORE_BUNDLE_BYTES: usize = 256 * 1024;
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
    SigstoreBundle,
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
    pub bundle_url: Option<String>,
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

/// Offline verifier backed by the Sigstore public-good production trust root.
pub struct SigstoreVerifier {
    verifier: Verifier,
}

impl SigstoreVerifier {
    /// Builds a verifier from the production root embedded by the Sigstore crate.
    ///
    /// # Errors
    ///
    /// Returns an error if the embedded trust root cannot be parsed or prepared.
    pub fn production() -> Result<Self> {
        let root = TrustedRoot::from_json(SIGSTORE_PRODUCTION_TRUSTED_ROOT)
            .context("Sigstore production trust root is invalid")?;
        let verifier = Verifier::new(&root).context("Sigstore verifier is unavailable")?;
        Ok(Self { verifier })
    }

    /// Verifies bytes against a standard Sigstore bundle and an exact signer identity.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed/oversized bundles, signer mismatch, or any failed
    /// certificate, SCT, transparency-log, consistency, digest, or signature check.
    pub fn verify(
        &self,
        artifact: &[u8],
        bundle_bytes: &[u8],
        identity: &PublisherIdentity,
    ) -> Result<()> {
        if bundle_bytes.is_empty() || bundle_bytes.len() > MAX_SIGSTORE_BUNDLE_BYTES {
            bail!("Sigstore bundle exceeds the size limit");
        }
        let bundle_json =
            std::str::from_utf8(bundle_bytes).context("Sigstore bundle is not UTF-8")?;
        let bundle = Bundle::from_json(bundle_json).context("Sigstore bundle is invalid")?;
        let policy = VerificationPolicy::new(&identity.subject, &identity.issuer);
        let result = self
            .verifier
            .verify(artifact, &bundle, &policy)
            .context("Sigstore verification failed")?;
        if !result.certificate_verified()
            || !result.sct_verified()
            || !result.tlog_verified()
            || !result.identity_policy_checked()
        {
            bail!("Sigstore bundle did not provide the required verification evidence");
        }
        Ok(())
    }
}

/// Immutable, content-addressed cache for validated remote catalogs.
///
/// Cached metadata remains inspection-only until a separate trust verifier authenticates it.
pub struct RemoteCatalogCache {
    root: PathBuf,
}

impl RemoteCatalogCache {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Validates and atomically publishes one downloaded catalog.
    ///
    /// # Errors
    ///
    /// Returns an error for oversized or invalid metadata and filesystem failures. Existing
    /// valid cache entries are left untouched on failure.
    pub fn store(&self, bytes: &[u8]) -> Result<Catalog> {
        if bytes.is_empty() || bytes.len() > MAX_CATALOG_BYTES {
            bail!("remote marketplace catalog exceeds the size limit");
        }
        let catalog = Catalog::parse(bytes)?;
        fs::create_dir_all(&self.root).with_context(|| {
            format!(
                "failed to create marketplace catalog cache {}",
                self.root.display()
            )
        })?;
        let digest = format!("{:x}", Sha256::digest(bytes));
        let received_at = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let destination = self.root.join(format!("{received_at:020}-{digest}.json"));
        let mut staging = NamedTempFile::new_in(&self.root)?;
        set_private_permissions(staging.path())?;
        std::io::Write::write_all(&mut staging, bytes)?;
        staging.as_file().sync_all()?;
        match staging.persist_noclobber(&destination) {
            Ok(_) => {}
            Err(_error) if destination.exists() => {}
            Err(error) => return Err(error.error.into()),
        }
        self.prune(3);
        Ok(catalog)
    }

    /// Loads the newest valid cached catalog, skipping corrupt entries.
    ///
    /// # Errors
    ///
    /// Returns an error when the cache directory cannot be inspected.
    pub fn load(&self) -> Result<Option<Catalog>> {
        if !self.root.exists() {
            return Ok(None);
        }
        let mut entries = cache_entries(&self.root)?;
        entries.sort_by(|left, right| right.0.cmp(&left.0));
        for (_, path) in entries {
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.file_type().is_file() || metadata.len() > MAX_CATALOG_BYTES as u64 {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            if !cache_entry_matches_digest(&path, &bytes) {
                continue;
            }
            if let Ok(catalog) = Catalog::parse(&bytes) {
                return Ok(Some(catalog));
            }
        }
        Ok(None)
    }

    fn prune(&self, retain: usize) {
        let Ok(mut entries) = cache_entries(&self.root) else {
            return;
        };
        entries.sort_by(|left, right| right.0.cmp(&left.0));
        for (_, path) in entries.into_iter().skip(retain) {
            let _ = fs::remove_file(path);
        }
    }
}

fn cache_entries(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
            continue;
        }
        let Some(name) = path
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .map(str::to_owned)
        else {
            continue;
        };
        entries.push((name, path));
    }
    Ok(entries)
}

fn cache_entry_matches_digest(path: &Path, bytes: &[u8]) -> bool {
    let actual = format!("{:x}", Sha256::digest(bytes));
    path.file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .and_then(|name| name.rsplit_once('-'))
        .is_some_and(|(_, expected)| expected.len() == 64 && actual.eq_ignore_ascii_case(expected))
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
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
            ReleaseTrust::SigstoreBundle if !valid_https_url(&self.source) => {
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
                || artifact
                    .bundle_url
                    .as_deref()
                    .is_some_and(|url| !valid_https_url(url))
            {
                bail!(
                    "package `{}` has an invalid artifact or bundle URL",
                    self.id
                );
            }
            match self.provenance.trust {
                ReleaseTrust::SigstoreBundle
                    if !artifact.url.as_deref().is_some_and(valid_https_url)
                        || !artifact.bundle_url.as_deref().is_some_and(valid_https_url) =>
                {
                    bail!("package `{}` has incomplete remote artifact trust", self.id);
                }
                ReleaseTrust::Bundled
                    if artifact.url.is_some() || artifact.bundle_url.is_some() =>
                {
                    bail!(
                        "package `{}` has remote URLs for a bundled artifact",
                        self.id
                    );
                }
                _ => {}
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

/// One immutable package release recorded in a scope-local installation ledger.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledRelease {
    pub package: Package,
    pub installed_at: String,
    pub active: bool,
    pub pinned: bool,
}

/// Result of checking one recorded release without executing package code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallationDiagnostic {
    pub package_id: String,
    pub version: String,
    pub active: bool,
    pub healthy: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct InstallationState {
    schema_version: u32,
    releases: Vec<InstalledRelease>,
}

impl Default for InstallationState {
    fn default() -> Self {
        Self {
            schema_version: 1,
            releases: Vec::new(),
        }
    }
}

/// Durable lifecycle state for one project or user marketplace scope.
pub struct InstallationStore {
    root: PathBuf,
}

impl InstallationStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn packages_root(&self) -> PathBuf {
        self.root.join("packages")
    }

    /// Lists all recorded releases in stable package/version order.
    ///
    /// # Errors
    ///
    /// Returns an error when state is malformed, oversized, linked, or unavailable.
    pub fn list(&self) -> Result<Vec<InstalledRelease>> {
        let mut releases = self.load()?.releases;
        releases.sort_by(|left, right| {
            (&left.package.id, &left.package.version)
                .cmp(&(&right.package.id, &right.package.version))
        });
        Ok(releases)
    }

    /// Atomically records an installed release and makes it active.
    ///
    /// A pinned active release cannot be replaced by a different version.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or conflicting state and filesystem failures.
    pub fn record(&self, installed: &InstalledPackage) -> Result<InstalledRelease> {
        installed.package.validate()?;
        let expected_root = self.package_root(&installed.package);
        if installed.root != expected_root {
            bail!("installed package is outside the marketplace scope");
        }
        verify_package_files(&installed.package, &installed.root)?;
        let mut state = self.load()?;
        if state.releases.iter().any(|release| {
            release.package.id == installed.package.id
                && release.active
                && release.pinned
                && release.package.version != installed.package.version
        }) {
            bail!("package is pinned; unpin it before changing versions");
        }
        if let Some(existing) = state.releases.iter().find(|release| {
            release.package.id == installed.package.id
                && release.package.version == installed.package.version
        }) && existing.package != installed.package
        {
            bail!("installed release metadata conflicts with the immutable ledger");
        }
        for release in &mut state.releases {
            if release.package.id == installed.package.id {
                release.active = false;
            }
        }
        let installed_at = Utc::now().to_rfc3339();
        let position = state.releases.iter().position(|release| {
            release.package.id == installed.package.id
                && release.package.version == installed.package.version
        });
        let recorded = if let Some(position) = position {
            state.releases[position].active = true;
            state.releases[position].installed_at = installed_at;
            state.releases[position].clone()
        } else {
            let release = InstalledRelease {
                package: installed.package.clone(),
                installed_at,
                active: true,
                pinned: false,
            };
            state.releases.push(release.clone());
            release
        };
        self.save(&state)?;
        Ok(recorded)
    }

    /// Pins or unpins the active release for a package.
    ///
    /// # Errors
    ///
    /// Returns an error if no active release exists or state cannot be saved.
    pub fn set_pinned(&self, package_id: &str, pinned: bool) -> Result<InstalledRelease> {
        if !valid_package_id(package_id) {
            bail!("marketplace package id is invalid");
        }
        let mut state = self.load()?;
        let release = state
            .releases
            .iter_mut()
            .find(|release| release.package.id == package_id && release.active)
            .with_context(|| format!("marketplace package `{package_id}` is not installed"))?;
        release.pinned = pinned;
        let release = release.clone();
        self.save(&state)?;
        Ok(release)
    }

    /// Checks every recorded release against its immutable artifact metadata.
    ///
    /// # Errors
    ///
    /// Returns an error only when the ledger itself cannot be read safely.
    pub fn doctor(&self) -> Result<Vec<InstallationDiagnostic>> {
        let releases = self.list()?;
        let known_roots = releases
            .iter()
            .map(|release| self.package_root(&release.package))
            .collect::<BTreeSet<_>>();
        let mut diagnostics = releases
            .into_iter()
            .map(|release| {
                let result =
                    verify_package_files(&release.package, &self.package_root(&release.package));
                InstallationDiagnostic {
                    package_id: release.package.id,
                    version: release.package.version,
                    active: release.active,
                    healthy: result.is_ok(),
                    detail: result
                        .map_or_else(|error| error.to_string(), |()| "verified".to_owned()),
                }
            })
            .collect::<Vec<_>>();
        diagnostics.extend(self.orphan_diagnostics(&known_roots)?);
        Ok(diagnostics)
    }

    /// Removes the active release through a scope-confined rename and ledger update.
    ///
    /// Callers must remove only matching disabled runtime registrations before invoking this.
    ///
    /// # Errors
    ///
    /// Returns an error if no active release exists or the removal cannot be completed safely.
    pub fn uninstall(&self, package_id: &str) -> Result<InstalledRelease> {
        if !valid_package_id(package_id) {
            bail!("marketplace package id is invalid");
        }
        let mut state = self.load()?;
        let position = state
            .releases
            .iter()
            .position(|release| release.package.id == package_id && release.active)
            .with_context(|| format!("marketplace package `{package_id}` is not installed"))?;
        let release = state.releases[position].clone();
        let package_root = self.package_root(&release.package);
        let parent = package_root
            .parent()
            .context("package installation has no parent")?;
        let tombstone = parent.join(format!(
            ".remove-{}-{}",
            release.package.version,
            std::process::id()
        ));
        if tombstone.exists() {
            bail!("a previous package removal is incomplete");
        }
        fs::rename(&package_root, &tombstone).with_context(|| {
            format!("failed to isolate installed package `{package_id}` for removal")
        })?;
        state.releases.remove(position);
        if let Err(error) = self.save(&state) {
            let _ = fs::rename(&tombstone, &package_root);
            return Err(error);
        }
        fs::remove_dir_all(&tombstone).with_context(|| {
            format!("installed package `{package_id}` was removed but cleanup is incomplete")
        })?;
        Ok(release)
    }

    fn package_root(&self, package: &Package) -> PathBuf {
        self.packages_root()
            .join(package.id.replace('/', "--"))
            .join(&package.version)
    }

    fn orphan_diagnostics(
        &self,
        known_roots: &BTreeSet<PathBuf>,
    ) -> Result<Vec<InstallationDiagnostic>> {
        let packages_root = self.packages_root();
        let metadata = match fs::symlink_metadata(&packages_root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_dir() {
            bail!("marketplace package root is not a directory");
        }
        let mut diagnostics = Vec::new();
        let mut inspected = 0_usize;
        for package_entry in fs::read_dir(&packages_root)? {
            let package_entry = package_entry?;
            inspected = inspected.saturating_add(1);
            if inspected > MAX_INSTALLED_RELEASES.saturating_mul(2) {
                bail!("marketplace package directory exceeds the inspection limit");
            }
            let package_path = package_entry.path();
            let package_name = package_entry.file_name().to_string_lossy().into_owned();
            let package_metadata = fs::symlink_metadata(&package_path)?;
            if !package_metadata.file_type().is_dir() {
                diagnostics.push(orphan_diagnostic(package_name, "unknown"));
                continue;
            }
            for version_entry in fs::read_dir(package_path)? {
                let version_entry = version_entry?;
                inspected = inspected.saturating_add(1);
                if inspected > MAX_INSTALLED_RELEASES.saturating_mul(2) {
                    bail!("marketplace package directory exceeds the inspection limit");
                }
                if !known_roots.contains(&version_entry.path()) {
                    diagnostics.push(orphan_diagnostic(
                        package_name.clone(),
                        &version_entry.file_name().to_string_lossy(),
                    ));
                }
            }
        }
        Ok(diagnostics)
    }

    fn load(&self) -> Result<InstallationState> {
        let path = self.root.join("installed.json");
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(InstallationState::default());
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_file() || metadata.len() > MAX_INSTALLATION_STATE_BYTES as u64 {
            bail!("marketplace installation state is not a bounded regular file");
        }
        let state: InstallationState = serde_json::from_slice(&fs::read(path)?)
            .context("marketplace installation state is invalid")?;
        Self::validate_state(&state)?;
        Ok(state)
    }

    fn save(&self, state: &InstallationState) -> Result<()> {
        Self::validate_state(state)?;
        fs::create_dir_all(&self.root)?;
        let bytes = serde_json::to_vec_pretty(state)?;
        if bytes.len() > MAX_INSTALLATION_STATE_BYTES {
            bail!("marketplace installation state exceeds the size limit");
        }
        let destination = self.root.join("installed.json");
        let mut temporary = NamedTempFile::new_in(&self.root)?;
        set_private_permissions(temporary.path())?;
        std::io::Write::write_all(&mut temporary, &bytes)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(&destination)
            .map_err(|error| error.error)?;
        Ok(())
    }

    fn validate_state(state: &InstallationState) -> Result<()> {
        if state.schema_version != 1 || state.releases.len() > MAX_INSTALLED_RELEASES {
            bail!("marketplace installation state schema or size is unsupported");
        }
        let mut identities = BTreeSet::new();
        let mut active = BTreeSet::new();
        for release in &state.releases {
            release.package.validate()?;
            DateTime::parse_from_rfc3339(&release.installed_at)
                .context("installed release timestamp is invalid")?;
            if !identities.insert((release.package.id.clone(), release.package.version.clone())) {
                bail!("marketplace installation state contains a duplicate release");
            }
            if release.active && !active.insert(release.package.id.clone()) {
                bail!("marketplace installation state contains multiple active releases");
            }
            if release.pinned && !release.active {
                bail!("only an active marketplace release can be pinned");
            }
        }
        Ok(())
    }
}

fn orphan_diagnostic(package_id: String, version: &str) -> InstallationDiagnostic {
    InstallationDiagnostic {
        package_id,
        version: version.to_owned(),
        active: false,
        healthy: false,
        detail: "orphaned package path is not present in the installation ledger".to_owned(),
    }
}

/// Downloaded bytes and the Sigstore bundle authenticating exactly those bytes.
pub struct SignedArtifact {
    pub bytes: Vec<u8>,
    pub bundle: Vec<u8>,
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

    /// Verifies and installs a complete remote artifact set.
    ///
    /// Every artifact must match its signed catalog size and digest and pass Sigstore
    /// verification against the exact publisher identity. Installation remains disabled.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-Sigstore release, publisher mismatch, incomplete download,
    /// integrity/authenticity failure, or filesystem failure.
    pub fn install_sigstore(
        &self,
        package: &Package,
        publisher: &Publisher,
        downloads: &BTreeMap<PathBuf, SignedArtifact>,
        verifier: &SigstoreVerifier,
    ) -> Result<InstalledPackage> {
        package.validate()?;
        if package.provenance.trust != ReleaseTrust::SigstoreBundle {
            bail!("remote installation requires Sigstore release trust");
        }
        if publisher.id != package.publisher || downloads.len() != package.artifacts.len() {
            bail!("remote package publisher or artifact set does not match the catalog");
        }
        for artifact in &package.artifacts {
            let download = downloads.get(&artifact.path).with_context(|| {
                format!("remote artifact `{}` is missing", artifact.path.display())
            })?;
            if u64::try_from(download.bytes.len()).unwrap_or(u64::MAX) != artifact.size_bytes
                || !format!("{:x}", Sha256::digest(&download.bytes))
                    .eq_ignore_ascii_case(&artifact.sha256)
            {
                bail!(
                    "remote artifact `{}` failed catalog integrity verification",
                    artifact.path.display()
                );
            }
            verifier.verify(&download.bytes, &download.bundle, &publisher.identity)?;
        }
        self.install_from(package, |artifact| {
            downloads
                .get(&artifact.path)
                .map(|download| download.bytes.clone())
                .context("verified remote artifact is unavailable")
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
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.host().is_some_and(|host| match host {
                Host::Domain(domain) => {
                    domain.split('.').next_back() != Some("localhost")
                        && domain.split('.').next_back() != Some("local")
                }
                Host::Ipv4(address) => {
                    !address.is_loopback()
                        && !address.is_private()
                        && !address.is_link_local()
                        && !address.is_unspecified()
                        && !address.is_multicast()
                }
                Host::Ipv6(address) => {
                    !address.is_loopback()
                        && !address.is_unique_local()
                        && !address.is_unicast_link_local()
                        && !address.is_unspecified()
                        && !address.is_multicast()
                }
            })
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
        CONTRIBUTOR_SCHEMA, Catalog, InstallationStore, Installer, MAX_CATALOG_BYTES,
        PUBLIC_CATALOG, PUBLIC_SCHEMA, RemoteCatalogCache, Revocation, bundled_catalog,
    };
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn catalog_json(hash: &str, path: &str) -> String {
        format!(
            r#"{{"schema_version":2,"name":"crumb-public","publishers":[{{"id":"crumb","display_name":"Crumb","source_url":"https://example.test/crumb","identity":{{"issuer":"https://issuer.example.test","subject":"repo:crumb/test"}}}}],"revocations":[],"packages":[{{"id":"crumb/review","publisher":"crumb","version":"1.0.0","kind":"skill","display_name":"Review","description":"Review code","license":"MIT","source":"./packages/review","compatibility":{{"minimum_crumb_version":"0.1.0","maximum_crumb_version_exclusive":"0.2.0","platforms":["linux","macos","windows"]}},"provenance":{{"repository":"https://example.test/review","revision":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","released_at":"2026-10-17T00:00:00Z","trust":{{"kind":"bundled"}}}},"disclosures":{{"security_url":"https://example.test/security","privacy_url":null,"data_flow":"local only"}},"connector_requirements":[],"deprecation":null,"artifacts":[{{"path":"{path}","sha256":"{hash}","size_bytes":9,"url":null,"bundle_url":null}}],"skills":[{{"id":"review","path":"{path}"}}]}}]}}"#
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
    fn installation_state_records_pins_and_diagnoses_releases() {
        let source = tempdir().expect("source tempdir");
        fs::write(source.path().join("SKILL.md"), "# Review\n").expect("skill fixture");
        let hash = format!("{:x}", Sha256::digest(b"# Review\n"));
        let package = catalog(&hash, "SKILL.md").packages.remove(0);
        let scope = tempdir().expect("scope tempdir");
        let store = InstallationStore::new(scope.path());
        let installed = Installer::new(store.packages_root())
            .install(&package, source.path())
            .expect("package should install");

        store
            .record(&installed)
            .expect("release should be recorded");
        let pinned = store
            .set_pinned("crumb/review", true)
            .expect("release should pin");
        assert!(pinned.active);
        assert!(pinned.pinned);
        assert_eq!(store.list().expect("installation state").len(), 1);
        assert!(
            store
                .doctor()
                .expect("diagnostics")
                .into_iter()
                .all(|diagnostic| diagnostic.healthy)
        );

        fs::write(installed.root.join("SKILL.md"), "tampered").expect("tamper fixture");
        let diagnostics = store.doctor().expect("tamper diagnostics");
        assert!(!diagnostics[0].healthy);
        assert!(diagnostics[0].detail.contains("integrity verification"));
    }

    #[test]
    fn installation_state_uninstalls_only_the_recorded_release() {
        let source = tempdir().expect("source tempdir");
        fs::write(source.path().join("SKILL.md"), "# Review\n").expect("skill fixture");
        let hash = format!("{:x}", Sha256::digest(b"# Review\n"));
        let package = catalog(&hash, "SKILL.md").packages.remove(0);
        let scope = tempdir().expect("scope tempdir");
        let store = InstallationStore::new(scope.path());
        let installed = Installer::new(store.packages_root())
            .install(&package, source.path())
            .expect("package should install");
        store
            .record(&installed)
            .expect("release should be recorded");
        fs::write(scope.path().join("keep.txt"), "keep").expect("unrelated fixture");

        let removed = store
            .uninstall("crumb/review")
            .expect("release should uninstall");
        assert_eq!(removed.package, package);
        assert!(!installed.root.exists());
        assert!(scope.path().join("keep.txt").exists());
        assert_eq!(store.list().expect("empty state"), Vec::new());

        let orphan = store.packages_root().join("unknown--package/9.9.9");
        fs::create_dir_all(orphan).expect("orphan fixture");
        let diagnostics = store.doctor().expect("orphan diagnostics");
        assert_eq!(diagnostics.len(), 1);
        assert!(!diagnostics[0].healthy);
        assert!(diagnostics[0].detail.contains("orphaned"));
    }

    #[test]
    fn installation_state_rejects_malformed_or_linked_state() {
        let scope = tempdir().expect("scope tempdir");
        fs::write(scope.path().join("installed.json"), "not json").expect("invalid state");
        assert!(InstallationStore::new(scope.path()).list().is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            fs::remove_file(scope.path().join("installed.json")).expect("remove invalid state");
            fs::write(
                scope.path().join("target.json"),
                r#"{"schema_version":1,"releases":[]}"#,
            )
            .expect("target state");
            symlink(
                scope.path().join("target.json"),
                scope.path().join("installed.json"),
            )
            .expect("state symlink");
            assert!(InstallationStore::new(scope.path()).list().is_err());
        }
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
        value["packages"][0]["provenance"]["trust"] =
            serde_json::json!({"kind": "sigstore_bundle"});
        value["packages"][0]["source"] = serde_json::json!("http://example.test/review");
        assert!(Catalog::parse(value.to_string().as_bytes()).is_err());

        let mut value: serde_json::Value =
            serde_json::from_str(&catalog_json(&"a".repeat(64), "SKILL.md")).expect("catalog JSON");
        value["packages"][0]["source"] = serde_json::json!("https://127.0.0.1/review");
        value["packages"][0]["provenance"]["trust"] =
            serde_json::json!({"kind": "sigstore_bundle"});
        value["packages"][0]["artifacts"][0]["url"] =
            serde_json::json!("https://localhost/SKILL.md");
        value["packages"][0]["artifacts"][0]["bundle_url"] =
            serde_json::json!("https://localhost/SKILL.md.sigstore.json");
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

    #[test]
    fn remote_cache_keeps_the_last_valid_catalog() {
        let directory = tempdir().expect("cache tempdir");
        let cache = RemoteCatalogCache::new(directory.path());
        let original = catalog_json(&"a".repeat(64), "SKILL.md");
        let stored = cache
            .store(original.as_bytes())
            .expect("valid catalog should be cached");

        assert!(cache.store(b"not JSON").is_err());
        assert_eq!(
            cache.load().expect("cache should load"),
            Some(stored),
            "a failed refresh must not replace the previous catalog"
        );
    }

    #[test]
    fn remote_cache_skips_corrupt_newer_entries_and_bounds_input() {
        let directory = tempdir().expect("cache tempdir");
        let cache = RemoteCatalogCache::new(directory.path());
        let original = catalog_json(&"a".repeat(64), "SKILL.md");
        let stored = cache
            .store(original.as_bytes())
            .expect("valid catalog should be cached");
        fs::write(
            directory.path().join("99999999999999999999-corrupt.json"),
            original.replace("crumb-public", "tampered-public"),
        )
        .expect("corrupt cache fixture");

        assert_eq!(cache.load().expect("cache should fall back"), Some(stored));
        assert!(cache.store(&vec![b'x'; MAX_CATALOG_BYTES + 1]).is_err());
    }

    #[test]
    fn sigstore_verification_requires_exact_identity_and_untampered_bytes() {
        const ARTIFACT: &[u8] = include_bytes!("../tests/fixtures/cosign-v3-blob.txt");
        const BUNDLE: &[u8] = include_bytes!("../tests/fixtures/cosign-v3-blob.sigstore.json");
        let verifier = super::SigstoreVerifier::production().expect("production verifier");
        let identity = super::PublisherIdentity {
            issuer: "https://github.com/login/oauth".to_owned(),
            subject: "w.vollprecht@gmail.com".to_owned(),
        };

        verifier
            .verify(ARTIFACT, BUNDLE, &identity)
            .expect("public-good fixture should verify");
        let mut wrong_identity = identity.clone();
        wrong_identity.subject = "attacker@example.test".to_owned();
        assert!(verifier.verify(ARTIFACT, BUNDLE, &wrong_identity).is_err());
        assert!(verifier.verify(b"tampered", BUNDLE, &identity).is_err());
        assert!(
            verifier
                .verify(
                    ARTIFACT,
                    &vec![b'x'; super::MAX_SIGSTORE_BUNDLE_BYTES + 1],
                    &identity,
                )
                .is_err()
        );
    }

    #[test]
    fn remote_install_verifies_every_artifact_and_stays_atomic() {
        const ARTIFACT: &[u8] = include_bytes!("../tests/fixtures/cosign-v3-blob.txt");
        const BUNDLE: &[u8] = include_bytes!("../tests/fixtures/cosign-v3-blob.sigstore.json");
        let hash = format!("{:x}", Sha256::digest(ARTIFACT));
        let mut value: serde_json::Value =
            serde_json::from_str(&catalog_json(&hash, "SKILL.md")).expect("catalog JSON");
        value["publishers"][0]["identity"] = serde_json::json!({
            "issuer": "https://github.com/login/oauth",
            "subject": "w.vollprecht@gmail.com"
        });
        value["packages"][0]["source"] = serde_json::json!("https://example.test/review");
        value["packages"][0]["provenance"]["trust"] =
            serde_json::json!({"kind": "sigstore_bundle"});
        value["packages"][0]["artifacts"][0]["size_bytes"] = serde_json::json!(ARTIFACT.len());
        value["packages"][0]["artifacts"][0]["url"] =
            serde_json::json!("https://example.test/SKILL.md");
        value["packages"][0]["artifacts"][0]["bundle_url"] =
            serde_json::json!("https://example.test/SKILL.md.sigstore.json");
        let catalog = Catalog::parse(value.to_string().as_bytes()).expect("remote catalog");
        let package = catalog
            .installable_package("crumb/review")
            .expect("installable package");
        let publisher = &catalog.publishers[0];
        let mut downloads = BTreeMap::from([(
            PathBuf::from("SKILL.md"),
            super::SignedArtifact {
                bytes: ARTIFACT.to_vec(),
                bundle: BUNDLE.to_vec(),
            },
        )]);
        let destination = tempdir().expect("destination");
        let verifier = super::SigstoreVerifier::production().expect("production verifier");

        let installed = Installer::new(destination.path())
            .install_sigstore(package, publisher, &downloads, &verifier)
            .expect("signed package should install");
        assert_eq!(
            fs::read(installed.root.join("SKILL.md")).expect("installed artifact"),
            ARTIFACT
        );

        downloads.get_mut(&PathBuf::from("SKILL.md")).unwrap().bytes[0] ^= 1;
        let second_destination = tempdir().expect("second destination");
        assert!(
            Installer::new(second_destination.path())
                .install_sigstore(package, publisher, &downloads, &verifier)
                .is_err()
        );
        assert_eq!(
            fs::read_dir(second_destination.path())
                .expect("empty destination")
                .count(),
            0
        );
    }
}
