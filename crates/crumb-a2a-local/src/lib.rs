//! Bounded same-machine transport for Crumb's A2A protocol models.
//!
//! A caller supplies the trusted bus root. This crate never searches home
//! directories or workspaces, performs network requests, starts threads, or
//! writes diagnostic logs. Registry records contain only public instance and
//! process metadata. Message bodies live transiently in inbox queue files and
//! are removed when acknowledged.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use crumb_a2a::{MAX_WIRE_BYTES, PROTOCOL_VERSION, SendMessageRequest, Validate as _, to_json};
use crumb_instance::{InstanceMetadata, validate_instance_name};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};

/// Version of registry and queue documents owned by this crate.
pub const LOCAL_BUS_SCHEMA_VERSION: u32 = 1;
/// Maximum active registry entries under one caller-supplied bus root.
pub const MAX_PEERS: usize = 128;
/// Maximum unacknowledged messages in one instance inbox.
pub const MAX_INBOX_MESSAGES: usize = 256;
/// Maximum retained payload-free acknowledgement records per inbox.
pub const MAX_ACKNOWLEDGEMENTS: usize = 1024;
/// Maximum immutable staged artifact files per sender instance.
pub const MAX_STAGED_ARTIFACTS: usize = 64;
/// Maximum encoded registry entry size.
pub const MAX_REGISTRY_BYTES: usize = 16 * 1024;
/// Maximum encoded delivery envelope size.
pub const MAX_ENVELOPE_BYTES: usize = MAX_WIRE_BYTES + 16 * 1024;
/// Maximum size of a file staged for later approval-gated import.
pub const MAX_ARTIFACT_BYTES: u64 = 8 * 1024 * 1024;

const REGISTRY_DIRECTORY: &str = "registry";
const INBOX_DIRECTORY: &str = "inboxes";
const ARTIFACT_DIRECTORY: &str = "artifacts";
const PENDING_DIRECTORY: &str = "pending";
const PROCESSING_DIRECTORY: &str = "processing";
const ACKNOWLEDGED_DIRECTORY: &str = "acknowledged";
const INSTANCE_ID_PREFIX: &str = "crumb_";
const INSTANCE_ID_HEX_BYTES: usize = 32;
const RUNTIME_ID_PREFIX: &str = "run_";
const RUNTIME_ID_HEX_BYTES: usize = 32;
const DELIVERY_ID_PREFIX: &str = "delivery_";
const DELIVERY_ID_HEX_BYTES: usize = 64;
const MAX_CLOCK_SKEW_MS: u64 = 60_000;

/// Bounds and lease policy for a local bus.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalBusOptions {
    pub lease_ttl: Duration,
}

impl Default for LocalBusOptions {
    fn default() -> Self {
        Self {
            lease_ttl: Duration::from_secs(30),
        }
    }
}

impl LocalBusOptions {
    fn validate(self) -> Result<Self> {
        if self.lease_ttl.is_zero() || self.lease_ttl > Duration::from_secs(300) {
            bail!("local A2A lease TTL must be between 1 millisecond and 5 minutes");
        }
        Ok(self)
    }

    fn lease_ttl_ms(self) -> Result<u64> {
        u64::try_from(self.lease_ttl.as_millis())
            .context("local A2A lease TTL exceeds the supported range")
    }
}

/// Non-secret process identity used to prevent one process from heartbeating
/// or unregistering another process's lease.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessMetadata {
    pub process_id: u32,
    pub started_at_ms: u64,
    pub runtime_id: String,
}

impl ProcessMetadata {
    /// Creates metadata for the calling process.
    ///
    /// # Errors
    ///
    /// Returns an error if secure randomness or the system clock is unavailable.
    pub fn current() -> Result<Self> {
        let process = Self {
            process_id: std::process::id(),
            started_at_ms: now_ms()?,
            runtime_id: random_identifier(RUNTIME_ID_PREFIX)?,
        };
        process.validate()?;
        Ok(process)
    }

    fn validate(&self) -> Result<()> {
        if self.process_id == 0 || self.started_at_ms == 0 {
            bail!("local A2A process metadata is invalid");
        }
        validate_prefixed_hex(
            &self.runtime_id,
            RUNTIME_ID_PREFIX,
            RUNTIME_ID_HEX_BYTES,
            "runtime identifier",
        )
    }
}

/// Public, non-secret lease record for one running Crumb instance.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PeerEntry {
    pub schema_version: u32,
    pub instance_id: String,
    pub name: String,
    pub inbox_endpoint: PathBuf,
    pub protocol_version: String,
    pub lease_updated_at_ms: u64,
    pub process: ProcessMetadata,
}

impl PeerEntry {
    /// Returns whether the lease has exceeded the bus TTL at `now_ms`.
    #[must_use]
    pub fn is_stale(&self, now_ms: u64, lease_ttl_ms: u64) -> bool {
        now_ms.saturating_sub(self.lease_updated_at_ms) > lease_ttl_ms
    }

    fn validate(&self, observed_at_ms: u64) -> Result<()> {
        if self.schema_version != LOCAL_BUS_SCHEMA_VERSION {
            bail!("unsupported local A2A registry schema");
        }
        validate_instance_id(&self.instance_id)?;
        validate_instance_name(&self.name)?;
        validate_inbox_endpoint(&self.instance_id, &self.inbox_endpoint)?;
        if self.protocol_version != PROTOCOL_VERSION {
            bail!("unsupported local A2A protocol version");
        }
        self.process.validate()?;
        if self.lease_updated_at_ms < self.process.started_at_ms {
            bail!("local A2A lease predates its process");
        }
        if self.lease_updated_at_ms > observed_at_ms.saturating_add(MAX_CLOCK_SKEW_MS) {
            bail!("local A2A lease is implausibly far in the future");
        }
        Ok(())
    }
}

/// Ownership handle returned by [`LocalBus::register`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Registration {
    entry: PeerEntry,
}

impl Registration {
    /// Current public peer record.
    #[must_use]
    pub fn entry(&self) -> &PeerEntry {
        &self.entry
    }

    /// Stable workspace instance identifier.
    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.entry.instance_id
    }
}

/// Receipt for an idempotently enqueued delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnqueueReceipt {
    pub delivery_id: String,
    pub recipient_instance_id: String,
}

/// A delivery atomically claimed from an inbox.
#[derive(Clone, Debug, PartialEq)]
pub struct ReceivedMessage {
    pub delivery_id: String,
    pub sender_instance_id: String,
    pub recipient_instance_id: String,
    pub enqueued_at_ms: u64,
    pub request: SendMessageRequest,
}

/// Immutable content-addressed file staged in the local bus.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagedArtifact {
    /// A2A-safe URL relative to the bus root.
    pub relative_url: String,
    /// Lowercase SHA-256 digest of the staged bytes.
    pub sha256: String,
    pub size_bytes: u64,
    pub filename: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeliveryEnvelope {
    schema_version: u32,
    delivery_id: String,
    sender_instance_id: String,
    recipient_instance_id: String,
    enqueued_at_ms: u64,
    request: SendMessageRequest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Acknowledgement {
    schema_version: u32,
    delivery_id: String,
    sender_instance_id: String,
    recipient_instance_id: String,
    acknowledged_at_ms: u64,
}

impl Acknowledgement {
    fn validate(&self) -> Result<()> {
        if self.schema_version != LOCAL_BUS_SCHEMA_VERSION || self.acknowledged_at_ms == 0 {
            bail!("local A2A acknowledgement is invalid");
        }
        validate_delivery_id(&self.delivery_id)?;
        validate_instance_id(&self.sender_instance_id)?;
        validate_instance_id(&self.recipient_instance_id)
    }
}

impl DeliveryEnvelope {
    fn validate(&self) -> Result<()> {
        if self.schema_version != LOCAL_BUS_SCHEMA_VERSION {
            bail!("unsupported local A2A delivery schema");
        }
        validate_delivery_id(&self.delivery_id)?;
        validate_instance_id(&self.sender_instance_id)?;
        validate_instance_id(&self.recipient_instance_id)?;
        if self.enqueued_at_ms == 0 {
            bail!("local A2A delivery timestamp is invalid");
        }
        self.request
            .validate()
            .context("local A2A request is invalid")?;
        Ok(())
    }

    fn received(self) -> ReceivedMessage {
        ReceivedMessage {
            delivery_id: self.delivery_id,
            sender_instance_id: self.sender_instance_id,
            recipient_instance_id: self.recipient_instance_id,
            enqueued_at_ms: self.enqueued_at_ms,
            request: self.request,
        }
    }

    fn matches_delivery(&self, other: &Self) -> bool {
        self.schema_version == other.schema_version
            && self.delivery_id == other.delivery_id
            && self.sender_instance_id == other.sender_instance_id
            && self.recipient_instance_id == other.recipient_instance_id
            && self.request == other.request
    }
}

/// Filesystem-backed local A2A bus rooted only at a caller-approved path.
#[derive(Clone, Debug)]
pub struct LocalBus {
    root: PathBuf,
    registry: PathBuf,
    inboxes: PathBuf,
    artifacts: PathBuf,
    options: LocalBusOptions,
}

impl LocalBus {
    /// Opens or creates a bounded local bus at a caller-trusted root.
    ///
    /// The parent of `root` must already exist. Existing bus directories may
    /// not be symlinks.
    ///
    /// # Errors
    ///
    /// Returns an error when the root or policy is unsafe or cannot be created.
    pub fn open(root: &Path, options: LocalBusOptions) -> Result<Self> {
        if root.as_os_str().is_empty() {
            bail!("local A2A bus root must not be empty");
        }
        let options = options.validate()?;
        ensure_private_directory(root)?;
        let registry = root.join(REGISTRY_DIRECTORY);
        let inboxes = root.join(INBOX_DIRECTORY);
        let artifacts = root.join(ARTIFACT_DIRECTORY);
        ensure_private_directory(&registry)?;
        ensure_private_directory(&inboxes)?;
        ensure_private_directory(&artifacts)?;
        Ok(Self {
            root: root.to_path_buf(),
            registry,
            inboxes,
            artifacts,
            options,
        })
    }

    /// Returns the exact trusted root supplied by the caller.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Registers a running process for a stable Crumb instance.
    ///
    /// A live lease owned by another runtime is never overwritten. A stale
    /// lease or a lease owned by the same runtime may be atomically replaced.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identity/process metadata, unsafe storage,
    /// a full registry, or an already-active different runtime.
    pub fn register(
        &self,
        instance: &InstanceMetadata,
        process: ProcessMetadata,
    ) -> Result<Registration> {
        instance
            .validate()
            .context("Crumb instance metadata is invalid")?;
        process.validate()?;
        self.inspect_base_directories()?;
        let now = now_ms()?;
        if process.started_at_ms > now.saturating_add(MAX_CLOCK_SKEW_MS) {
            bail!("local A2A process start is implausibly far in the future");
        }
        let endpoint = inbox_endpoint(&instance.instance_id)?;
        self.ensure_inbox(&instance.instance_id)?;

        let path = self.registry_path(&instance.instance_id)?;
        if let Some(existing) =
            read_optional_json::<PeerEntry>(&path, MAX_REGISTRY_BYTES, "local A2A registry entry")?
        {
            existing.validate(now)?;
            let ttl = self.options.lease_ttl_ms()?;
            let same_runtime = existing.process == process;
            if !same_runtime && !existing.is_stale(now, ttl) {
                bail!("Crumb instance already has an active local A2A runtime");
            }
        } else if count_visible_entries(&self.registry)? >= MAX_PEERS {
            bail!("local A2A registry is full");
        }

        let entry = PeerEntry {
            schema_version: LOCAL_BUS_SCHEMA_VERSION,
            instance_id: instance.instance_id.clone(),
            name: instance.name.clone(),
            inbox_endpoint: endpoint,
            protocol_version: PROTOCOL_VERSION.to_owned(),
            lease_updated_at_ms: now.max(process.started_at_ms),
            process,
        };
        entry.validate(entry.lease_updated_at_ms)?;
        atomic_replace_json(&self.registry, &path, &entry, MAX_REGISTRY_BYTES)?;
        Ok(Registration { entry })
    }

    /// Atomically renews a registration owned by this runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if the lease disappeared, changed owners, or storage is
    /// malformed or unsafe.
    pub fn heartbeat(&self, registration: &mut Registration) -> Result<()> {
        self.inspect_base_directories()?;
        let path = self.registry_path(registration.instance_id())?;
        let mut current: PeerEntry =
            read_required_json(&path, MAX_REGISTRY_BYTES, "local A2A registry entry")?;
        current.validate(now_ms()?)?;
        ensure_owner(&current, registration)?;
        current.lease_updated_at_ms = now_ms()?.max(current.process.started_at_ms);
        current.validate(current.lease_updated_at_ms)?;
        atomic_replace_json(&self.registry, &path, &current, MAX_REGISTRY_BYTES)?;
        registration.entry = current;
        Ok(())
    }

    /// Atomically removes a registration owned by this runtime.
    ///
    /// Queued messages remain for a future registration of the stable instance.
    ///
    /// # Errors
    ///
    /// Returns an error if the lease changed owners or storage is unsafe.
    pub fn unregister(&self, registration: &Registration) -> Result<()> {
        self.inspect_base_directories()?;
        let path = self.registry_path(registration.instance_id())?;
        let current: PeerEntry =
            read_required_json(&path, MAX_REGISTRY_BYTES, "local A2A registry entry")?;
        current.validate(now_ms()?)?;
        ensure_owner(&current, registration)?;
        remove_regular_file(&path, "local A2A registry entry")?;
        sync_directory(&self.registry).context("local A2A registry could not sync")
    }

    /// Lists non-stale peer leases in deterministic name/identifier order.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe, malformed, oversized, or overfull
    /// registry instead of silently trusting partial discovery data.
    pub fn list_peers(&self) -> Result<Vec<PeerEntry>> {
        self.inspect_base_directories()?;
        let paths = visible_regular_entries(&self.registry, MAX_PEERS, "registry")?;
        let now = now_ms()?;
        let ttl = self.options.lease_ttl_ms()?;
        let mut peers = Vec::with_capacity(paths.len());
        for path in paths {
            let entry: PeerEntry =
                read_required_json(&path, MAX_REGISTRY_BYTES, "local A2A registry entry")?;
            entry.validate(now)?;
            if path != self.registry_path(&entry.instance_id)? {
                bail!("local A2A registry filename does not match its instance");
            }
            if !entry.is_stale(now, ttl) {
                peers.push(entry);
            }
        }
        peers.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.instance_id.cmp(&right.instance_id))
        });
        Ok(peers)
    }

    /// Atomically and idempotently enqueues an A2A request for a live peer.
    ///
    /// The deterministic delivery identifier makes retrying the same request
    /// safe. Message content is not copied to logs or registry metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if either lease is unavailable/stale, the request is
    /// invalid or oversized, the inbox is full, or storage is unsafe.
    pub fn enqueue(
        &self,
        sender: &Registration,
        recipient_instance_id: &str,
        request: &SendMessageRequest,
    ) -> Result<EnqueueReceipt> {
        self.inspect_base_directories()?;
        validate_instance_id(recipient_instance_id)?;
        request.validate().context("local A2A request is invalid")?;
        let now = now_ms()?;
        self.require_live_owner(sender, now)?;
        self.require_live_peer(recipient_instance_id, now)?;
        let request_json = to_json(request).context("local A2A request could not be encoded")?;
        let delivery_id = delivery_id(sender.instance_id(), recipient_instance_id, &request_json);
        let envelope = DeliveryEnvelope {
            schema_version: LOCAL_BUS_SCHEMA_VERSION,
            delivery_id: delivery_id.clone(),
            sender_instance_id: sender.instance_id().to_owned(),
            recipient_instance_id: recipient_instance_id.to_owned(),
            enqueued_at_ms: now,
            request: request.clone(),
        };
        envelope.validate()?;

        let (pending, processing, acknowledged) = self.queue_directories(recipient_instance_id)?;
        let queued = count_visible_entries(&pending)? + count_visible_entries(&processing)?;
        let filename = delivery_filename(&delivery_id)?;
        let path = pending.join(&filename);
        let processing_path = processing.join(&filename);
        let acknowledged_path = acknowledged.join(&filename);
        let receipt = EnqueueReceipt {
            delivery_id: delivery_id.clone(),
            recipient_instance_id: recipient_instance_id.to_owned(),
        };
        if path_exists_without_following(&acknowledged_path)? {
            let existing: Acknowledgement = read_required_json(
                &acknowledged_path,
                MAX_REGISTRY_BYTES,
                "local A2A acknowledgement",
            )?;
            existing.validate()?;
            if existing.delivery_id != delivery_id
                || existing.sender_instance_id != sender.instance_id()
                || existing.recipient_instance_id != recipient_instance_id
            {
                bail!("local A2A acknowledgement does not match delivery");
            }
            return Ok(receipt);
        }
        for queued_path in [&path, &processing_path] {
            if path_exists_without_following(queued_path)? {
                let existing: DeliveryEnvelope =
                    read_required_json(queued_path, MAX_ENVELOPE_BYTES, "local A2A delivery")?;
                existing.validate()?;
                if !existing.matches_delivery(&envelope) {
                    bail!("local A2A delivery identifier collision");
                }
                return Ok(receipt);
            }
        }
        if queued >= MAX_INBOX_MESSAGES {
            bail!("local A2A inbox is full");
        }
        match atomic_write_new_json(&pending, &path, &envelope, MAX_ENVELOPE_BYTES) {
            Ok(()) => {}
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::AlreadyExists) =>
            {
                let existing: DeliveryEnvelope =
                    read_required_json(&path, MAX_ENVELOPE_BYTES, "local A2A delivery")?;
                existing.validate()?;
                if !existing.matches_delivery(&envelope) {
                    bail!("local A2A delivery identifier collision");
                }
            }
            Err(error) => return Err(error),
        }
        Ok(receipt)
    }

    /// Atomically claims the oldest pending message for this runtime.
    ///
    /// The message remains in `processing` until [`Self::acknowledge`] or
    /// [`Self::release`] is called.
    ///
    /// # Errors
    ///
    /// Returns an error if ownership, queue storage, or a delivery is invalid.
    pub fn dequeue(&self, recipient: &Registration) -> Result<Option<ReceivedMessage>> {
        self.inspect_base_directories()?;
        self.require_live_owner(recipient, now_ms()?)?;
        let (pending, processing, _acknowledged) =
            self.queue_directories(recipient.instance_id())?;
        let paths = visible_regular_entries(&pending, MAX_INBOX_MESSAGES, "pending inbox")?;
        for source in paths {
            let Some(filename) = source.file_name() else {
                continue;
            };
            let target = processing.join(filename);
            if path_exists_without_following(&target)? {
                bail!("local A2A delivery is already being processed");
            }
            match fs::rename(&source, &target) {
                Ok(()) => {
                    sync_directory(&pending).context("local A2A pending inbox could not sync")?;
                    sync_directory(&processing)
                        .context("local A2A processing inbox could not sync")?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error).context("local A2A delivery could not be claimed"),
            }
            let envelope: DeliveryEnvelope =
                read_required_json(&target, MAX_ENVELOPE_BYTES, "local A2A delivery")?;
            envelope.validate()?;
            if target.file_name() != Some(delivery_filename(&envelope.delivery_id)?.as_ref()) {
                bail!("local A2A delivery filename does not match its identifier");
            }
            if envelope.recipient_instance_id != recipient.instance_id() {
                bail!("local A2A delivery recipient does not match inbox owner");
            }
            return Ok(Some(envelope.received()));
        }
        Ok(None)
    }

    /// Acknowledges and removes a previously claimed delivery.
    ///
    /// # Errors
    ///
    /// Returns an error if ownership, the delivery identifier, or queue storage
    /// is invalid.
    pub fn acknowledge(&self, recipient: &Registration, delivery_id: &str) -> Result<()> {
        self.finish_delivery(recipient, delivery_id, true)
    }

    /// Returns a claimed delivery to the pending queue for retry.
    ///
    /// # Errors
    ///
    /// Returns an error if ownership, the delivery identifier, or queue storage
    /// is invalid.
    pub fn release(&self, recipient: &Registration, delivery_id: &str) -> Result<()> {
        self.finish_delivery(recipient, delivery_id, false)
    }

    /// Stages one caller-selected workspace file as an immutable bus artifact.
    ///
    /// `workspace_root` is trusted by the caller while `relative_source` must
    /// be a traversal-free relative path. No path from the source workspace is
    /// persisted. The returned URL can be placed in an A2A [`crumb_a2a::Part`].
    /// This operation does not import or overwrite files in another workspace.
    ///
    /// # Errors
    ///
    /// Returns an error if the sender lease is invalid; the workspace path or
    /// any source component is a symlink; the source is not a regular file; the
    /// file exceeds 8 MiB; or staging cannot complete atomically.
    pub fn stage_artifact(
        &self,
        sender: &Registration,
        workspace_root: &Path,
        relative_source: &Path,
    ) -> Result<StagedArtifact> {
        self.inspect_base_directories()?;
        self.require_live_owner(sender, now_ms()?)?;
        inspect_directory(workspace_root, "artifact workspace root")?;
        validate_relative_path(relative_source)?;
        let source = inspect_workspace_source(workspace_root, relative_source)?;
        let filename = source
            .file_name()
            .and_then(|value| value.to_str())
            .context("artifact filename must be UTF-8")?;
        validate_artifact_filename(filename)?;

        let sender_root = self.artifacts.join(sender.instance_id());
        ensure_private_directory(&sender_root)?;
        let mut source_file = File::open(&source).context("artifact source could not be opened")?;
        let source_metadata = source_file
            .metadata()
            .context("artifact source could not be inspected after opening")?;
        if !source_metadata.is_file() || source_metadata.len() > MAX_ARTIFACT_BYTES {
            bail!("artifact source must be a regular file no larger than 8 MiB");
        }

        let mut temporary = tempfile::NamedTempFile::new_in(&sender_root)
            .context("temporary artifact could not be created")?;
        set_private_permissions(temporary.as_file())?;
        let (digest, size_bytes) = copy_and_hash(&mut source_file, &mut temporary)?;
        temporary
            .as_file_mut()
            .sync_all()
            .context("temporary artifact could not sync")?;
        let digest_directory_name = format!("sha256-{digest}");
        let digest_directory = sender_root.join(&digest_directory_name);
        let destination = digest_directory.join(filename);
        let exists = path_exists_without_following(&destination)?;
        if !exists && count_staged_artifacts(&sender_root)? >= MAX_STAGED_ARTIFACTS {
            bail!("local A2A staged artifact limit has been reached");
        }
        ensure_private_directory(&digest_directory)?;
        match temporary.persist_noclobber(&destination) {
            Ok(_) => {
                sync_directory(&digest_directory)
                    .context("artifact digest directory could not sync")?;
            }
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                verify_existing_artifact(&destination, &digest, size_bytes)?;
            }
            Err(error) => {
                return Err(error.error).context("artifact could not be atomically staged");
            }
        }
        let relative_url = format!(
            "{ARTIFACT_DIRECTORY}/{}/{digest_directory_name}/{filename}",
            sender.instance_id()
        );
        Ok(StagedArtifact {
            relative_url,
            sha256: digest,
            size_bytes,
            filename: filename.to_owned(),
        })
    }

    fn finish_delivery(
        &self,
        recipient: &Registration,
        delivery_id: &str,
        acknowledge: bool,
    ) -> Result<()> {
        self.inspect_base_directories()?;
        self.require_live_owner(recipient, now_ms()?)?;
        validate_delivery_id(delivery_id)?;
        let (pending, processing, acknowledged) =
            self.queue_directories(recipient.instance_id())?;
        let filename = delivery_filename(delivery_id)?;
        let source = processing.join(&filename);
        let acknowledgement_path = acknowledged.join(&filename);
        if acknowledge && path_exists_without_following(&acknowledgement_path)? {
            let record: Acknowledgement = read_required_json(
                &acknowledgement_path,
                MAX_REGISTRY_BYTES,
                "local A2A acknowledgement",
            )?;
            record.validate()?;
            if record.delivery_id != delivery_id
                || record.recipient_instance_id != recipient.instance_id()
            {
                bail!("local A2A acknowledgement does not match delivery");
            }
            if path_exists_without_following(&source)? {
                remove_regular_file(&source, "local A2A delivery")?;
                sync_directory(&processing).context("local A2A processing inbox could not sync")?;
            }
            return Ok(());
        }
        let envelope: DeliveryEnvelope =
            read_required_json(&source, MAX_ENVELOPE_BYTES, "local A2A delivery")?;
        envelope.validate()?;
        if envelope.delivery_id != delivery_id
            || envelope.recipient_instance_id != recipient.instance_id()
        {
            bail!("local A2A delivery does not match acknowledgement");
        }
        if acknowledge {
            if count_visible_entries(&acknowledged)? >= MAX_ACKNOWLEDGEMENTS {
                bail!("local A2A acknowledgement limit has been reached");
            }
            let record = Acknowledgement {
                schema_version: LOCAL_BUS_SCHEMA_VERSION,
                delivery_id: delivery_id.to_owned(),
                sender_instance_id: envelope.sender_instance_id,
                recipient_instance_id: envelope.recipient_instance_id,
                acknowledged_at_ms: now_ms()?,
            };
            record.validate()?;
            atomic_write_new_json(
                &acknowledged,
                &acknowledgement_path,
                &record,
                MAX_REGISTRY_BYTES,
            )?;
            remove_regular_file(&source, "local A2A delivery")?;
            sync_directory(&processing).context("local A2A processing inbox could not sync")?;
        } else {
            let target = pending.join(filename);
            if path_exists_without_following(&target)? {
                bail!("local A2A pending delivery already exists");
            }
            fs::rename(&source, &target).context("local A2A delivery could not be released")?;
            sync_directory(&processing).context("local A2A processing inbox could not sync")?;
            sync_directory(&pending).context("local A2A pending inbox could not sync")?;
        }
        Ok(())
    }

    fn inspect_base_directories(&self) -> Result<()> {
        inspect_directory(&self.root, "local A2A bus root")?;
        inspect_directory(&self.registry, "local A2A registry")?;
        inspect_directory(&self.inboxes, "local A2A inbox root")
            .and_then(|()| inspect_directory(&self.artifacts, "local A2A artifact root"))
    }

    fn registry_path(&self, instance_id: &str) -> Result<PathBuf> {
        validate_instance_id(instance_id)?;
        Ok(self.registry.join(format!("{instance_id}.json")))
    }

    fn ensure_inbox(&self, instance_id: &str) -> Result<()> {
        validate_instance_id(instance_id)?;
        let inbox = self.inboxes.join(instance_id);
        ensure_private_directory(&inbox)?;
        ensure_private_directory(&inbox.join(PENDING_DIRECTORY))?;
        ensure_private_directory(&inbox.join(PROCESSING_DIRECTORY))?;
        ensure_private_directory(&inbox.join(ACKNOWLEDGED_DIRECTORY))
    }

    fn queue_directories(&self, instance_id: &str) -> Result<(PathBuf, PathBuf, PathBuf)> {
        validate_instance_id(instance_id)?;
        let inbox = self.inboxes.join(instance_id);
        inspect_directory(&inbox, "local A2A instance inbox")?;
        let pending = inbox.join(PENDING_DIRECTORY);
        let processing = inbox.join(PROCESSING_DIRECTORY);
        let acknowledged = inbox.join(ACKNOWLEDGED_DIRECTORY);
        inspect_directory(&pending, "local A2A pending inbox")?;
        inspect_directory(&processing, "local A2A processing inbox")?;
        inspect_directory(&acknowledged, "local A2A acknowledgement inbox")?;
        Ok((pending, processing, acknowledged))
    }

    fn require_live_peer(&self, instance_id: &str, now: u64) -> Result<PeerEntry> {
        let path = self.registry_path(instance_id)?;
        let entry: PeerEntry =
            read_required_json(&path, MAX_REGISTRY_BYTES, "local A2A registry entry")?;
        entry.validate(now)?;
        if entry.instance_id != instance_id {
            bail!("local A2A registry entry does not match its filename");
        }
        if entry.is_stale(now, self.options.lease_ttl_ms()?) {
            bail!("local A2A peer lease is stale");
        }
        Ok(entry)
    }

    fn require_live_owner(&self, registration: &Registration, now: u64) -> Result<PeerEntry> {
        let entry = self.require_live_peer(registration.instance_id(), now)?;
        ensure_owner(&entry, registration)?;
        Ok(entry)
    }
}

fn ensure_owner(entry: &PeerEntry, registration: &Registration) -> Result<()> {
    if entry.instance_id != registration.entry.instance_id
        || entry.process != registration.entry.process
    {
        bail!("local A2A registration is owned by another runtime");
    }
    Ok(())
}

fn inbox_endpoint(instance_id: &str) -> Result<PathBuf> {
    validate_instance_id(instance_id)?;
    Ok(PathBuf::from(INBOX_DIRECTORY).join(instance_id))
}

fn validate_inbox_endpoint(instance_id: &str, endpoint: &Path) -> Result<()> {
    let expected = inbox_endpoint(instance_id)?;
    validate_relative_path(endpoint)?;
    if endpoint != expected {
        bail!("local A2A inbox endpoint is not canonical");
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("local A2A path must be non-empty, relative, and traversal-free");
    }
    Ok(())
}

fn validate_instance_id(value: &str) -> Result<()> {
    validate_prefixed_hex(
        value,
        INSTANCE_ID_PREFIX,
        INSTANCE_ID_HEX_BYTES,
        "instance identifier",
    )
}

fn validate_delivery_id(value: &str) -> Result<()> {
    validate_prefixed_hex(
        value,
        DELIVERY_ID_PREFIX,
        DELIVERY_ID_HEX_BYTES,
        "delivery identifier",
    )
}

fn validate_prefixed_hex(value: &str, prefix: &str, hex_bytes: usize, label: &str) -> Result<()> {
    let Some(encoded) = value.strip_prefix(prefix) else {
        bail!("local A2A {label} has an invalid prefix");
    };
    if encoded.len() != hex_bytes
        || !encoded
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("local A2A {label} is malformed");
    }
    Ok(())
}

fn delivery_id(sender: &str, recipient: &str, request_json: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(sender.as_bytes());
    digest.update([0]);
    digest.update(recipient.as_bytes());
    digest.update([0]);
    digest.update(request_json);
    format!("{DELIVERY_ID_PREFIX}{:x}", digest.finalize())
}

fn delivery_filename(delivery_id: &str) -> Result<String> {
    validate_delivery_id(delivery_id)?;
    Ok(format!("{delivery_id}.json"))
}

fn validate_artifact_filename(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 256
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("artifact filename must be a safe ASCII filename of at most 256 bytes");
    }
    Ok(())
}

fn inspect_workspace_source(workspace_root: &Path, relative: &Path) -> Result<PathBuf> {
    validate_relative_path(relative)?;
    let mut current = workspace_root.to_path_buf();
    let component_count = relative.components().count();
    for (index, component) in relative.components().enumerate() {
        let Component::Normal(component) = component else {
            bail!("artifact source contains an unsafe path component");
        };
        current.push(component);
        let metadata = fs::symlink_metadata(&current)
            .context("artifact source component could not be inspected")?;
        if metadata.file_type().is_symlink() {
            bail!("artifact source components must not be symlinks");
        }
        let is_final = index + 1 == component_count;
        if (!is_final && !metadata.file_type().is_dir())
            || (is_final && !metadata.file_type().is_file())
        {
            bail!("artifact source must resolve to a regular file inside the workspace");
        }
    }
    Ok(current)
}

fn copy_and_hash(
    source: &mut File,
    destination: &mut tempfile::NamedTempFile,
) -> Result<(String, u64)> {
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 32 * 1024].into_boxed_slice();
    let mut size = 0_u64;
    loop {
        let read = source
            .read(&mut buffer)
            .context("artifact source could not be read")?;
        if read == 0 {
            break;
        }
        let read_u64 = u64::try_from(read).context("artifact chunk size is unsupported")?;
        size = size
            .checked_add(read_u64)
            .context("artifact size overflowed")?;
        if size > MAX_ARTIFACT_BYTES {
            bail!("artifact source exceeds the 8 MiB limit");
        }
        digest.update(&buffer[..read]);
        destination
            .write_all(&buffer[..read])
            .context("temporary artifact could not be written")?;
    }
    Ok((format!("{:x}", digest.finalize()), size))
}

fn verify_existing_artifact(path: &Path, expected_digest: &str, expected_size: u64) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).context("existing artifact could not be inspected")?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.len() != expected_size
    {
        bail!("existing artifact is unsafe or does not match staged content");
    }
    let mut file = File::open(path).context("existing artifact could not be opened")?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 32 * 1024].into_boxed_slice();
    loop {
        let read = file
            .read(&mut buffer)
            .context("existing artifact could not be read")?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    if format!("{:x}", digest.finalize()) != expected_digest {
        bail!("existing artifact digest does not match staged content");
    }
    Ok(())
}

fn count_staged_artifacts(sender_root: &Path) -> Result<usize> {
    let mut count = 0_usize;
    for directory in fs::read_dir(sender_root).context("staged artifact root could not be read")? {
        let directory = directory.context("staged artifact entry could not be read")?;
        if directory.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let metadata = fs::symlink_metadata(directory.path())
            .context("staged artifact digest directory could not be inspected")?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            bail!("staged artifact digest entries must be directories, not symlinks");
        }
        for file in fs::read_dir(directory.path())
            .context("staged artifact digest directory could not be read")?
        {
            let file = file.context("staged artifact file could not be read")?;
            if file.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let metadata = fs::symlink_metadata(file.path())
                .context("staged artifact file could not be inspected")?;
            if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                bail!("staged artifacts must be regular files, not symlinks");
            }
            count = count.saturating_add(1);
            if count > MAX_STAGED_ARTIFACTS {
                bail!("local A2A staged artifact limit has been exceeded");
            }
        }
    }
    Ok(count)
}

fn random_identifier(prefix: &str) -> Result<String> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .context("secure local A2A runtime identifier generation failed")?;
    let mut encoded = String::with_capacity(prefix.len() + random.len() * 2);
    encoded.push_str(prefix);
    for byte in random {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(encoded)
}

fn now_ms() -> Result<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    u64::try_from(millis).context("system timestamp exceeds the supported range")
}

fn path_exists_without_following(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!("local A2A path must not be a symlink");
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("local A2A path could not be inspected"),
    }
}

fn inspect_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).with_context(|| format!("{label} is unavailable"))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        bail!("{label} must be a directory, not a symlink");
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            inspect_directory(path, "local A2A directory")
        }
        Err(error) => Err(error).context("local A2A directory could not be created"),
    }
}

#[cfg(not(unix))]
fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            inspect_directory(path, "local A2A directory")
        }
        Err(error) => Err(error).context("local A2A directory could not be created"),
    }
}

fn count_visible_entries(directory: &Path) -> Result<usize> {
    let mut count = 0_usize;
    for entry in fs::read_dir(directory).context("local A2A directory could not be read")? {
        let entry = entry.context("local A2A directory entry could not be read")?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        count = count.saturating_add(1);
    }
    Ok(count)
}

fn visible_regular_entries(directory: &Path, max: usize, label: &str) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in
        fs::read_dir(directory).with_context(|| format!("local A2A {label} could not be read"))?
    {
        let entry = entry.with_context(|| format!("local A2A {label} entry could not be read"))?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())
            .with_context(|| format!("local A2A {label} entry could not be inspected"))?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            bail!("local A2A {label} entries must be regular files, not symlinks");
        }
        paths.push(entry.path());
        if paths.len() > max {
            bail!("local A2A {label} exceeds its entry limit");
        }
    }
    paths.sort();
    Ok(paths)
}

fn read_optional_json<T>(path: &Path, max_bytes: usize, label: &str) -> Result<Option<T>>
where
    T: DeserializeOwned,
{
    match fs::symlink_metadata(path) {
        Ok(_) => read_required_json(path, max_bytes, label).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("{label} could not be inspected")),
    }
}

fn read_required_json<T>(path: &Path, max_bytes: usize, label: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    let metadata = fs::symlink_metadata(path).with_context(|| format!("{label} is unavailable"))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!("{label} must be a regular file, not a symlink");
    }
    let max_u64 = u64::try_from(max_bytes).context("local A2A size limit is unsupported")?;
    if metadata.len() > max_u64 {
        bail!("{label} exceeds its byte limit");
    }
    let read_limit = max_u64
        .checked_add(1)
        .context("local A2A read limit overflowed")?;
    let capacity = usize::try_from(metadata.len()).context("local A2A file size is unsupported")?;
    let mut encoded = Vec::with_capacity(capacity);
    File::open(path)
        .with_context(|| format!("{label} could not be opened"))?
        .take(read_limit)
        .read_to_end(&mut encoded)
        .with_context(|| format!("{label} could not be read"))?;
    if encoded.len() > max_bytes {
        bail!("{label} exceeds its byte limit");
    }
    serde_json::from_slice(&encoded).with_context(|| format!("{label} is malformed"))
}

fn atomic_replace_json<T>(directory: &Path, path: &Path, value: &T, max_bytes: usize) -> Result<()>
where
    T: Serialize,
{
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.file_type().is_file())
    {
        bail!("local A2A destination must be a regular file, not a symlink");
    }
    let mut temporary = tempfile::NamedTempFile::new_in(directory)
        .context("temporary local A2A file could not be created")?;
    write_json(&mut temporary, value, max_bytes)?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("local A2A file could not be atomically replaced")?;
    sync_directory(directory).context("local A2A directory could not sync")
}

fn atomic_write_new_json<T>(
    directory: &Path,
    path: &Path,
    value: &T,
    max_bytes: usize,
) -> Result<()>
where
    T: Serialize,
{
    let mut temporary = tempfile::NamedTempFile::new_in(directory)
        .context("temporary local A2A file could not be created")?;
    write_json(&mut temporary, value, max_bytes)?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| error.error)
        .context("local A2A file could not be atomically created")?;
    sync_directory(directory).context("local A2A directory could not sync")
}

fn write_json<T>(file: &mut tempfile::NamedTempFile, value: &T, max_bytes: usize) -> Result<()>
where
    T: Serialize,
{
    let mut encoded = serde_json::to_vec(value).context("local A2A value could not be encoded")?;
    encoded.push(b'\n');
    if encoded.len() > max_bytes {
        bail!("local A2A value exceeds its byte limit");
    }
    set_private_permissions(file.as_file())?;
    file.write_all(&encoded)
        .context("local A2A file could not be written")?;
    file.as_file_mut()
        .sync_all()
        .context("local A2A file could not sync")
}

fn remove_regular_file(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).with_context(|| format!("{label} is unavailable"))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!("{label} must be a regular file, not a symlink");
    }
    fs::remove_file(path).with_context(|| format!("{label} could not be removed"))
}

#[cfg(unix)]
fn set_private_permissions(file: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    file.set_permissions(fs::Permissions::from_mode(0o600))
        .context("local A2A file permissions could not be restricted")
}

#[cfg(not(unix))]
fn set_private_permissions(_file: &File) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> std::io::Result<()> {
    File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use crumb_a2a::{Message, Part, Role, SendMessageConfiguration};
    use crumb_instance::InstanceStore;
    use tempfile::TempDir;

    use super::*;

    fn request(id: &str, text: &str) -> SendMessageRequest {
        SendMessageRequest {
            tenant: None,
            message: Message {
                message_id: id.to_owned(),
                context_id: Some("ctx-local".to_owned()),
                task_id: None,
                role: Role::User,
                parts: vec![Part::text(text)],
                metadata: None,
                extensions: Vec::new(),
                reference_task_ids: Vec::new(),
            },
            configuration: Some(SendMessageConfiguration {
                accepted_output_modes: vec!["text/plain".to_owned()],
                history_length: Some(2),
                return_immediately: false,
            }),
            metadata: None,
        }
    }

    fn process(number: u8) -> ProcessMetadata {
        ProcessMetadata {
            process_id: u32::from(number) + 100,
            started_at_ms: now_ms().expect("time"),
            runtime_id: format!("run_{number:032x}"),
        }
    }

    fn setup() -> (
        TempDir,
        LocalBus,
        InstanceMetadata,
        InstanceMetadata,
        Registration,
        Registration,
    ) {
        let temporary = TempDir::new().expect("temporary root");
        let first_workspace = temporary.path().join("workspace-one");
        let second_workspace = temporary.path().join("workspace-two");
        fs::create_dir(&first_workspace).expect("first workspace");
        fs::create_dir(&second_workspace).expect("second workspace");
        let first = InstanceStore::for_workspace(&first_workspace)
            .load_or_create_named("Builder One")
            .expect("first identity");
        let second = InstanceStore::for_workspace(&second_workspace)
            .load_or_create_named("Builder Two")
            .expect("second identity");
        let bus =
            LocalBus::open(&temporary.path().join("bus"), LocalBusOptions::default()).expect("bus");
        let first_registration = bus
            .register(&first, process(1))
            .expect("first registration");
        let second_registration = bus
            .register(&second, process(2))
            .expect("second registration");
        (
            temporary,
            bus,
            first,
            second,
            first_registration,
            second_registration,
        )
    }

    #[test]
    fn distinct_workspaces_exchange_and_acknowledge_one_message() {
        let (_temporary, bus, first, second, first_registration, second_registration) = setup();
        let message = request("msg-1", "Please inspect the shared artifact.");
        let receipt = bus
            .enqueue(&first_registration, &second.instance_id, &message)
            .expect("enqueue");
        let duplicate = bus
            .enqueue(&first_registration, &second.instance_id, &message)
            .expect("idempotent enqueue");
        assert_eq!(receipt, duplicate);

        let received = bus
            .dequeue(&second_registration)
            .expect("dequeue")
            .expect("message");
        assert_eq!(received.sender_instance_id, first.instance_id);
        assert_eq!(received.recipient_instance_id, second.instance_id);
        assert_eq!(received.request, message);
        assert!(bus.dequeue(&second_registration).expect("empty").is_none());

        bus.acknowledge(&second_registration, &received.delivery_id)
            .expect("acknowledge");
        let after_ack = bus
            .enqueue(&first_registration, &second.instance_id, &message)
            .expect("idempotent enqueue after acknowledgement");
        assert_eq!(receipt, after_ack);
        assert!(bus.dequeue(&second_registration).expect("empty").is_none());
    }

    #[test]
    fn release_returns_claimed_message_to_pending() {
        let (_temporary, bus, _first, second, first_registration, second_registration) = setup();
        bus.enqueue(
            &first_registration,
            &second.instance_id,
            &request("msg-2", "retry"),
        )
        .expect("enqueue");
        let first = bus
            .dequeue(&second_registration)
            .expect("dequeue")
            .expect("message");
        bus.release(&second_registration, &first.delivery_id)
            .expect("release");
        let second_delivery = bus
            .dequeue(&second_registration)
            .expect("dequeue again")
            .expect("message again");
        assert_eq!(first.delivery_id, second_delivery.delivery_id);
    }

    #[test]
    fn stale_peers_are_filtered_and_cannot_receive_new_messages() {
        let (_temporary, bus, _first, second, first_registration, _second_registration) = setup();
        let path = bus
            .registry_path(&second.instance_id)
            .expect("registry path");
        let mut entry: PeerEntry =
            read_required_json(&path, MAX_REGISTRY_BYTES, "registry entry").expect("entry");
        entry.process.started_at_ms = 1;
        entry.lease_updated_at_ms = 1;
        atomic_replace_json(&bus.registry, &path, &entry, MAX_REGISTRY_BYTES).expect("stale entry");

        let peers = bus.list_peers().expect("list peers");
        assert_eq!(peers.len(), 1);
        assert_ne!(peers[0].instance_id, second.instance_id);
        assert!(
            bus.enqueue(
                &first_registration,
                &second.instance_id,
                &request("msg-3", "stale")
            )
            .is_err()
        );
    }

    #[test]
    fn corrupt_registry_and_queue_files_are_rejected() {
        let (_temporary, bus, _first, second, first_registration, second_registration) = setup();
        let registry_path = bus
            .registry_path(&second.instance_id)
            .expect("registry path");
        fs::write(&registry_path, b"{not-json").expect("corrupt registry");
        assert!(bus.list_peers().is_err());

        let second_entry = PeerEntry {
            schema_version: LOCAL_BUS_SCHEMA_VERSION,
            instance_id: second.instance_id.clone(),
            name: second.name.clone(),
            inbox_endpoint: inbox_endpoint(&second.instance_id).expect("endpoint"),
            protocol_version: PROTOCOL_VERSION.to_owned(),
            lease_updated_at_ms: now_ms().expect("time"),
            process: second_registration.entry.process.clone(),
        };
        atomic_replace_json(
            &bus.registry,
            &registry_path,
            &second_entry,
            MAX_REGISTRY_BYTES,
        )
        .expect("restore registry");
        let receipt = bus
            .enqueue(
                &first_registration,
                &second.instance_id,
                &request("msg-4", "corrupt"),
            )
            .expect("enqueue");
        let (pending, _processing, _acknowledged) =
            bus.queue_directories(&second.instance_id).expect("queue");
        fs::write(
            pending.join(delivery_filename(&receipt.delivery_id).expect("filename")),
            b"[]",
        )
        .expect("corrupt queue");
        assert!(bus.dequeue(&second_registration).is_err());
    }

    #[test]
    fn traversal_and_oversized_files_are_rejected() {
        let (_temporary, bus, _first, second, first_registration, second_registration) = setup();
        assert!(
            bus.enqueue(&first_registration, "../peer", &request("msg-5", "bad"))
                .is_err()
        );

        let receipt = bus
            .enqueue(
                &first_registration,
                &second.instance_id,
                &request("msg-6", "large"),
            )
            .expect("enqueue");
        let (pending, _processing, _acknowledged) =
            bus.queue_directories(&second.instance_id).expect("queue");
        fs::write(
            pending.join(delivery_filename(&receipt.delivery_id).expect("filename")),
            vec![b'x'; MAX_ENVELOPE_BYTES + 1],
        )
        .expect("oversized delivery");
        assert!(bus.dequeue(&second_registration).is_err());
    }

    #[test]
    fn artifact_staging_is_bounded_content_addressed_and_idempotent() {
        let (temporary, bus, _first, _second, first_registration, _second_registration) = setup();
        let workspace = temporary.path().join("artifact-workspace");
        let nested = workspace.join("output");
        fs::create_dir(&workspace).expect("artifact workspace");
        fs::create_dir(&nested).expect("artifact directory");
        fs::write(nested.join("report.txt"), b"bounded artifact\n").expect("artifact source");

        let first = bus
            .stage_artifact(
                &first_registration,
                &workspace,
                Path::new("output/report.txt"),
            )
            .expect("stage artifact");
        let second = bus
            .stage_artifact(
                &first_registration,
                &workspace,
                Path::new("output/report.txt"),
            )
            .expect("stage artifact again");
        assert_eq!(first, second);
        assert_eq!(first.size_bytes, 17);
        assert!(first.relative_url.starts_with(&format!(
            "artifacts/{}/sha256-",
            first_registration.instance_id()
        )));
        assert!(first.relative_url.ends_with("/report.txt"));
        let staged = bus.root().join(&first.relative_url);
        assert_eq!(
            fs::read(staged).expect("staged bytes"),
            b"bounded artifact\n"
        );
    }

    #[test]
    fn artifact_staging_rejects_escape_absolute_non_regular_and_oversized_sources() {
        let (temporary, bus, _first, _second, first_registration, _second_registration) = setup();
        let workspace = temporary.path().join("artifact-workspace-invalid");
        fs::create_dir(&workspace).expect("artifact workspace");
        fs::create_dir(workspace.join("directory.txt")).expect("source directory");
        let oversized = usize::try_from(MAX_ARTIFACT_BYTES)
            .expect("artifact limit fits usize")
            .saturating_add(1);
        fs::write(workspace.join("large.bin"), vec![0_u8; oversized]).expect("large source");

        for source in [
            Path::new("../outside"),
            Path::new("/etc/passwd"),
            Path::new("directory.txt"),
            Path::new("large.bin"),
        ] {
            assert!(
                bus.stage_artifact(&first_registration, &workspace, source)
                    .is_err(),
                "accepted {source:?}"
            );
        }
    }

    #[test]
    fn different_live_runtime_cannot_replace_or_unregister_owner() {
        let (_temporary, bus, first, _second, first_registration, _second_registration) = setup();
        assert!(bus.register(&first, process(9)).is_err());
        let forged = Registration {
            entry: PeerEntry {
                process: process(9),
                ..first_registration.entry.clone()
            },
        };
        assert!(bus.unregister(&forged).is_err());
    }

    #[test]
    fn tiny_ttl_filters_old_lease_without_sleeping() {
        let temporary = TempDir::new().expect("temporary root");
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let instance = InstanceStore::for_workspace(&workspace)
            .load_or_create_named("Short Lease")
            .expect("identity");
        let bus = LocalBus::open(
            &temporary.path().join("bus"),
            LocalBusOptions {
                lease_ttl: Duration::from_millis(1),
            },
        )
        .expect("bus");
        let registration = bus.register(&instance, process(1)).expect("register");
        let path = bus.registry_path(registration.instance_id()).expect("path");
        let mut entry: PeerEntry =
            read_required_json(&path, MAX_REGISTRY_BYTES, "entry").expect("entry");
        entry.process.started_at_ms = 1;
        entry.lease_updated_at_ms = 1;
        atomic_replace_json(&bus.registry, &path, &entry, MAX_REGISTRY_BYTES).expect("replace");
        assert_eq!(bus.list_peers().expect("peers"), Vec::<PeerEntry>::new());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_root_registry_and_queue_entries_are_rejected() {
        use std::os::unix::fs::symlink;

        let temporary = TempDir::new().expect("temporary root");
        let target = temporary.path().join("target");
        fs::create_dir(&target).expect("target");
        let linked_root = temporary.path().join("linked-bus");
        symlink(&target, &linked_root).expect("root symlink");
        assert!(LocalBus::open(&linked_root, LocalBusOptions::default()).is_err());

        let (_temporary, bus, _first, second, first_registration, second_registration) = setup();
        let registry_path = bus
            .registry_path(&second.instance_id)
            .expect("registry path");
        fs::remove_file(&registry_path).expect("remove registry");
        symlink("missing", &registry_path).expect("registry symlink");
        assert!(bus.list_peers().is_err());

        fs::remove_file(&registry_path).expect("remove symlink");
        atomic_replace_json(
            &bus.registry,
            &registry_path,
            second_registration.entry(),
            MAX_REGISTRY_BYTES,
        )
        .expect("restore registry");
        let receipt = bus
            .enqueue(
                &first_registration,
                &second.instance_id,
                &request("msg-7", "symlink"),
            )
            .expect("enqueue");
        let (pending, _processing, _acknowledged) =
            bus.queue_directories(&second.instance_id).expect("queue");
        let delivery_path =
            pending.join(delivery_filename(&receipt.delivery_id).expect("filename"));
        fs::remove_file(&delivery_path).expect("remove delivery");
        symlink("missing", &delivery_path).expect("delivery symlink");
        assert!(bus.dequeue(&second_registration).is_err());

        let workspace = temporary.path().join("artifact-symlink-workspace");
        fs::create_dir(&workspace).expect("artifact workspace");
        fs::write(workspace.join("real.txt"), b"real").expect("real artifact");
        symlink("real.txt", workspace.join("linked.txt")).expect("artifact symlink");
        assert!(
            bus.stage_artifact(&first_registration, &workspace, Path::new("linked.txt"))
                .is_err()
        );
    }
}
