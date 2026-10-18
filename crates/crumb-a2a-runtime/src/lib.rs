//! Long-lived process lifecycle for Crumb's bounded local A2A transport.
//!
//! One runtime owns one process registration and one joined heartbeat worker.
//! Cloneable clients share that ownership. This crate performs no discovery
//! outside the caller-supplied bus root and workspace, makes no network calls,
//! and emits no logs. The trusted workspace path stays in process memory only.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use crumb_a2a::{SendMessageRequest, SendMessageResponse};
use crumb_a2a_local::{
    DeliverySummary, EnqueueReceipt, LocalBus, LocalBusOptions, PeerEntry, PendingDeliverySummary,
    ProcessMetadata, ReceivedDelivery, ReceivedMessage, Registration, StagedArtifact,
};
use crumb_instance::{InstanceMetadata, InstanceStore};

/// Minimum lease duration that leaves useful scheduling room for a heartbeat.
pub const MIN_RUNTIME_LEASE_TTL: Duration = Duration::from_millis(30);

/// Runtime configuration shared with the bounded local transport.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeOptions {
    pub local_bus: LocalBusOptions,
}

impl RuntimeOptions {
    fn heartbeat_interval(self) -> Result<Duration> {
        let ttl = self.local_bus.lease_ttl;
        if ttl < MIN_RUNTIME_LEASE_TTL {
            bail!("A2A runtime lease TTL must be at least 30 milliseconds");
        }
        let interval = ttl / 3;
        if interval.is_zero() || interval >= ttl {
            bail!("A2A runtime heartbeat interval is unsafe");
        }
        Ok(interval)
    }
}

/// Observable lifecycle state. It never contains message or path data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeState {
    Healthy,
    Degraded,
    Stopping,
    Stopped,
}

/// Bounded failure categories for runtime health reporting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeIssue {
    HeartbeatFailed,
    WorkerSynchronizationFailed,
    WorkerPanicked,
    UnregisterFailed,
}

/// Snapshot of heartbeat and shutdown health without raw error strings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeHealth {
    pub state: RuntimeState,
    pub last_success_at_ms: Option<u64>,
    pub consecutive_heartbeat_failures: u32,
    pub last_issue: Option<RuntimeIssue>,
}

impl RuntimeHealth {
    fn healthy(now_ms: u64) -> Self {
        Self {
            state: RuntimeState::Healthy,
            last_success_at_ms: Some(now_ms),
            consecutive_heartbeat_failures: 0,
            last_issue: None,
        }
    }
}

#[derive(Debug, Default)]
struct StopControl {
    requested: Mutex<bool>,
    changed: Condvar,
}

#[derive(Debug)]
struct RuntimeInner {
    bus: LocalBus,
    workspace: PathBuf,
    registration: Arc<Mutex<Registration>>,
    health: Arc<Mutex<RuntimeHealth>>,
    stop: Arc<StopControl>,
    worker: Mutex<Option<JoinHandle<()>>>,
    shutdown_gate: Mutex<()>,
    shutdown_started: AtomicBool,
}

impl RuntimeInner {
    fn ensure_running(&self) -> Result<()> {
        if self.shutdown_started.load(Ordering::Acquire) {
            bail!("local A2A runtime is shutting down");
        }
        Ok(())
    }

    fn shutdown(&self) -> Result<()> {
        let _gate = lock_recover(&self.shutdown_gate);
        if self.shutdown_started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        update_health(&self.health, |health| {
            health.state = RuntimeState::Stopping;
        });
        request_stop(&self.stop);

        let mut first_error = None;
        let worker = lock_recover(&self.worker).take();
        if worker.is_some_and(|worker| worker.join().is_err()) {
            update_health(&self.health, |health| {
                health.state = RuntimeState::Degraded;
                health.last_issue = Some(RuntimeIssue::WorkerPanicked);
            });
            first_error = Some(anyhow!("local A2A heartbeat worker panicked"));
        }

        let registration = lock_recover(&self.registration);
        let unregister_result = self.bus.unregister(&registration);
        if let Err(error) = unregister_result {
            update_health(&self.health, |health| {
                health.state = RuntimeState::Degraded;
                health.last_issue = Some(RuntimeIssue::UnregisterFailed);
            });
            if first_error.is_none() {
                first_error = Some(error.context("local A2A runtime could not unregister"));
            }
        }

        update_health(&self.health, |health| {
            health.state = RuntimeState::Stopped;
        });
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for RuntimeInner {
    fn drop(&mut self) {
        drop(self.shutdown());
    }
}

/// Cloneable handle to one registered local A2A process runtime.
#[derive(Clone, Debug)]
pub struct A2aRuntime {
    inner: Arc<RuntimeInner>,
}

impl A2aRuntime {
    /// Registers one process and starts its joined heartbeat worker.
    ///
    /// Both paths are supplied by the caller. The workspace must contain the
    /// exact persisted `InstanceMetadata` passed here; its absolute path is
    /// retained only in memory for explicit artifact staging.
    ///
    /// # Errors
    ///
    /// Returns an error when options, workspace identity, local bus storage,
    /// registration, or heartbeat thread creation fails.
    pub fn start(
        bus_root: &Path,
        workspace: &Path,
        instance: &InstanceMetadata,
        options: RuntimeOptions,
    ) -> Result<Self> {
        let heartbeat_interval = options.heartbeat_interval()?;
        validate_workspace_identity(workspace, instance)?;
        let bus = LocalBus::open(bus_root, options.local_bus)?;
        let process = ProcessMetadata::current()?;
        let registration = bus.register(instance, process)?;
        let registered_at = registration.entry().lease_updated_at_ms;
        let registration = Arc::new(Mutex::new(registration));
        let health = Arc::new(Mutex::new(RuntimeHealth::healthy(registered_at)));
        let stop = Arc::new(StopControl::default());

        let worker = spawn_heartbeat(
            bus.clone(),
            Arc::clone(&registration),
            Arc::clone(&health),
            Arc::clone(&stop),
            heartbeat_interval,
        );
        let worker = match worker {
            Ok(worker) => worker,
            Err(error) => {
                if let Ok(registration) = registration.lock() {
                    drop(bus.unregister(&registration));
                }
                return Err(error).context("local A2A heartbeat worker could not start");
            }
        };

        Ok(Self {
            inner: Arc::new(RuntimeInner {
                bus,
                workspace: workspace.to_path_buf(),
                registration,
                health,
                stop,
                worker: Mutex::new(Some(worker)),
                shutdown_gate: Mutex::new(()),
                shutdown_started: AtomicBool::new(false),
            }),
        })
    }

    /// Returns this runtime's current public peer entry.
    ///
    /// # Errors
    ///
    /// Returns an error after shutdown begins or if runtime synchronization is
    /// unavailable.
    pub fn own_peer(&self) -> Result<PeerEntry> {
        self.inner.ensure_running()?;
        Ok(lock(&self.inner.registration, "A2A runtime registration")?
            .entry()
            .clone())
    }

    /// Lists active peers excluding this runtime itself.
    ///
    /// # Errors
    ///
    /// Returns an error after shutdown begins or when bus discovery fails.
    pub fn list_peers(&self) -> Result<Vec<PeerEntry>> {
        self.inner.ensure_running()?;
        let own_id = self.own_peer()?.instance_id;
        Ok(self
            .inner
            .bus
            .list_peers()?
            .into_iter()
            .filter(|peer| peer.instance_id != own_id)
            .collect())
    }

    /// Enqueues one bounded request for an active peer.
    ///
    /// # Errors
    ///
    /// Returns an error after shutdown begins or when registration, request,
    /// recipient, or inbox storage validation fails.
    pub fn enqueue(
        &self,
        recipient_instance_id: &str,
        request: &SendMessageRequest,
    ) -> Result<EnqueueReceipt> {
        self.inner.ensure_running()?;
        let registration = lock(&self.inner.registration, "A2A runtime registration")?.clone();
        self.inner
            .bus
            .enqueue(&registration, recipient_instance_id, request)
    }

    /// Atomically claims this runtime's oldest pending message.
    ///
    /// # Errors
    ///
    /// Returns an error after shutdown begins or when registration or inbox
    /// storage validation fails.
    pub fn dequeue(&self) -> Result<Option<ReceivedMessage>> {
        self.inner.ensure_running()?;
        let registration = lock(&self.inner.registration, "A2A runtime registration")?.clone();
        self.inner.bus.dequeue(&registration)
    }

    /// Lists this runtime's pending deliveries without exposing payload data.
    ///
    /// # Errors
    ///
    /// Returns an error after shutdown begins or when registration, queue
    /// bounds, or a pending envelope is invalid.
    pub fn list_pending(&self) -> Result<Vec<PendingDeliverySummary>> {
        self.inner.ensure_running()?;
        let registration = lock(&self.inner.registration, "A2A runtime registration")?.clone();
        self.inner.bus.list_pending(&registration)
    }

    /// Lists all pending requests and responses without exposing payload data.
    ///
    /// # Errors
    ///
    /// Returns an error after shutdown begins or when inbox validation fails.
    pub fn list_deliveries(&self) -> Result<Vec<DeliverySummary>> {
        self.inner.ensure_running()?;
        let registration = lock(&self.inner.registration, "A2A runtime registration")?.clone();
        self.inner.bus.list_deliveries(&registration)
    }

    /// Reads one exact pending delivery without changing queue state.
    ///
    /// # Errors
    ///
    /// Returns an error after shutdown begins or for invalid/unknown delivery
    /// identifiers, wrong ownership, or unsafe queue storage.
    pub fn inspect_pending(&self, delivery_id: &str) -> Result<ReceivedMessage> {
        self.inner.ensure_running()?;
        let registration = lock(&self.inner.registration, "A2A runtime registration")?.clone();
        self.inner.bus.inspect_pending(&registration, delivery_id)
    }

    /// Reads one exact pending request or response without changing queue state.
    ///
    /// # Errors
    ///
    /// Returns an error after shutdown begins or when delivery validation fails.
    pub fn inspect_delivery(&self, delivery_id: &str) -> Result<ReceivedDelivery> {
        self.inner.ensure_running()?;
        let registration = lock(&self.inner.registration, "A2A runtime registration")?.clone();
        self.inner.bus.inspect_delivery(&registration, delivery_id)
    }

    /// Atomically claims one exact pending delivery by identifier.
    ///
    /// # Errors
    ///
    /// Returns an error after shutdown begins or for invalid/unknown/already
    /// claimed identifiers, wrong ownership, or unsafe queue storage.
    pub fn claim_pending(&self, delivery_id: &str) -> Result<ReceivedMessage> {
        self.inner.ensure_running()?;
        let registration = lock(&self.inner.registration, "A2A runtime registration")?.clone();
        self.inner.bus.claim_pending(&registration, delivery_id)
    }

    /// Atomically claims one exact pending request or response by identifier.
    ///
    /// # Errors
    ///
    /// Returns an error after shutdown begins or when the claim cannot be completed safely.
    pub fn claim_delivery(&self, delivery_id: &str) -> Result<ReceivedDelivery> {
        self.inner.ensure_running()?;
        let registration = lock(&self.inner.registration, "A2A runtime registration")?.clone();
        self.inner.bus.claim_delivery(&registration, delivery_id)
    }

    /// Durably queues a correlated response before acknowledging a claimed request.
    ///
    /// # Errors
    ///
    /// Returns an error after shutdown begins or when completion cannot be committed safely.
    pub fn complete_claimed_request(
        &self,
        delivery_id: &str,
        response: &SendMessageResponse,
    ) -> Result<EnqueueReceipt> {
        self.inner.ensure_running()?;
        let registration = lock(&self.inner.registration, "A2A runtime registration")?.clone();
        self.inner
            .bus
            .complete_claimed_request(&registration, delivery_id, response)
    }

    /// Rejects a claimed request with a protocol-level rejected task response.
    ///
    /// # Errors
    ///
    /// Returns an error after shutdown begins or when rejection cannot be committed safely.
    pub fn reject_claimed_request(&self, delivery_id: &str) -> Result<EnqueueReceipt> {
        self.inner.ensure_running()?;
        let registration = lock(&self.inner.registration, "A2A runtime registration")?.clone();
        self.inner
            .bus
            .reject_claimed_request(&registration, delivery_id)
    }

    /// Acknowledges a claimed response without routing it through the agent loop.
    ///
    /// # Errors
    ///
    /// Returns an error after shutdown begins or for invalid response queue state.
    pub fn acknowledge_response(&self, delivery_id: &str) -> Result<()> {
        self.inner.ensure_running()?;
        let registration = lock(&self.inner.registration, "A2A runtime registration")?.clone();
        self.inner
            .bus
            .acknowledge_response(&registration, delivery_id)
    }

    /// Acknowledges one claimed delivery.
    ///
    /// # Errors
    ///
    /// Returns an error after shutdown begins or when ownership or delivery
    /// validation fails.
    pub fn acknowledge(&self, delivery_id: &str) -> Result<()> {
        self.inner.ensure_running()?;
        let registration = lock(&self.inner.registration, "A2A runtime registration")?.clone();
        self.inner.bus.acknowledge(&registration, delivery_id)
    }

    /// Releases one claimed delivery for retry.
    ///
    /// # Errors
    ///
    /// Returns an error after shutdown begins or when ownership or delivery
    /// validation fails.
    pub fn release(&self, delivery_id: &str) -> Result<()> {
        self.inner.ensure_running()?;
        let registration = lock(&self.inner.registration, "A2A runtime registration")?.clone();
        self.inner.bus.release(&registration, delivery_id)
    }

    /// Stages a safe relative file from this runtime's trusted workspace.
    ///
    /// # Errors
    ///
    /// Returns an error after shutdown begins or when the relative source,
    /// registration, source file, or artifact storage is invalid.
    pub fn stage_artifact(&self, relative_source: &Path) -> Result<StagedArtifact> {
        self.inner.ensure_running()?;
        let registration = lock(&self.inner.registration, "A2A runtime registration")?.clone();
        self.inner
            .bus
            .stage_artifact(&registration, &self.inner.workspace, relative_source)
    }

    /// Returns a bounded, content-free runtime health snapshot.
    #[must_use]
    pub fn health(&self) -> RuntimeHealth {
        lock_recover(&self.inner.health).clone()
    }

    /// Stops and joins the heartbeat worker, then unregisters only this lease.
    ///
    /// This method is idempotent across cloned handles.
    ///
    /// # Errors
    ///
    /// Returns an error if the worker panicked or the owned lease could not be
    /// unregistered.
    pub fn shutdown(&self) -> Result<()> {
        self.inner.shutdown()
    }
}

fn validate_workspace_identity(workspace: &Path, expected: &InstanceMetadata) -> Result<()> {
    let metadata =
        std::fs::symlink_metadata(workspace).context("A2A runtime workspace is unavailable")?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        bail!("A2A runtime workspace must be a directory, not a symlink");
    }
    let persisted = InstanceStore::for_workspace(workspace)
        .load()
        .context("A2A runtime workspace identity could not be loaded")?;
    if &persisted != expected {
        bail!("A2A runtime workspace identity does not match supplied metadata");
    }
    Ok(())
}

fn spawn_heartbeat(
    bus: LocalBus,
    registration: Arc<Mutex<Registration>>,
    health: Arc<Mutex<RuntimeHealth>>,
    stop: Arc<StopControl>,
    interval: Duration,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("crumb-a2a-heartbeat".to_owned())
        .spawn(move || heartbeat_loop(&bus, &registration, &health, &stop, interval))
}

fn heartbeat_loop(
    bus: &LocalBus,
    registration: &Mutex<Registration>,
    health: &Mutex<RuntimeHealth>,
    stop: &StopControl,
    interval: Duration,
) {
    loop {
        let requested = lock_recover(&stop.requested);
        let wait = stop.changed.wait_timeout(requested, interval);
        let (requested, _timeout) = match wait {
            Ok(result) => result,
            Err(poisoned) => {
                let (requested, timeout) = poisoned.into_inner();
                update_health_direct(health, |snapshot| {
                    snapshot.state = RuntimeState::Degraded;
                    snapshot.last_issue = Some(RuntimeIssue::WorkerSynchronizationFailed);
                });
                (requested, timeout)
            }
        };
        if *requested {
            break;
        }
        drop(requested);

        let result = {
            let mut registration = lock_recover(registration);
            bus.heartbeat(&mut registration)
        };
        match result {
            Ok(()) => {
                let observed_at = now_ms().ok();
                update_health_direct(health, |snapshot| {
                    snapshot.state = RuntimeState::Healthy;
                    if observed_at.is_some() {
                        snapshot.last_success_at_ms = observed_at;
                    }
                    snapshot.consecutive_heartbeat_failures = 0;
                    snapshot.last_issue = None;
                });
            }
            Err(_) => {
                update_health_direct(health, |snapshot| {
                    snapshot.state = RuntimeState::Degraded;
                    snapshot.consecutive_heartbeat_failures =
                        snapshot.consecutive_heartbeat_failures.saturating_add(1);
                    snapshot.last_issue = Some(RuntimeIssue::HeartbeatFailed);
                });
            }
        }
    }
}

fn request_stop(stop: &StopControl) {
    *lock_recover(&stop.requested) = true;
    stop.changed.notify_all();
}

fn update_health(health: &Arc<Mutex<RuntimeHealth>>, update: impl FnOnce(&mut RuntimeHealth)) {
    update_health_direct(health, update);
}

fn update_health_direct(health: &Mutex<RuntimeHealth>, update: impl FnOnce(&mut RuntimeHealth)) {
    update(&mut lock_recover(health));
}

fn lock<'a, T>(mutex: &'a Mutex<T>, label: &str) -> Result<MutexGuard<'a, T>> {
    mutex
        .lock()
        .map_err(|_| anyhow!("{label} synchronization failed"))
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn now_ms() -> Result<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    u64::try_from(millis).context("system timestamp exceeds the supported range")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::thread;
    use std::time::{Duration, Instant};

    use crumb_a2a::{Message, Part, Role};
    use crumb_instance::InstanceStore;
    use tempfile::TempDir;

    use super::*;

    fn request(id: &str, text: &str) -> SendMessageRequest {
        SendMessageRequest {
            tenant: None,
            message: Message {
                message_id: id.to_owned(),
                context_id: Some("ctx-runtime".to_owned()),
                task_id: None,
                role: Role::User,
                parts: vec![Part::text(text)],
                metadata: None,
                extensions: Vec::new(),
                reference_task_ids: Vec::new(),
            },
            configuration: None,
            metadata: None,
        }
    }

    fn options() -> RuntimeOptions {
        RuntimeOptions {
            local_bus: LocalBusOptions {
                lease_ttl: Duration::from_millis(90),
            },
        }
    }

    fn workspace(root: &Path, name: &str) -> (PathBuf, InstanceMetadata) {
        let workspace = root.join(name);
        fs::create_dir(&workspace).expect("workspace");
        let instance = InstanceStore::for_workspace(&workspace)
            .load_or_create_named(name)
            .expect("instance metadata");
        (workspace, instance)
    }

    fn wait_until(timeout: Duration, predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(predicate(), "condition was not met before timeout");
    }

    #[test]
    fn distinct_runtimes_discover_exchange_and_use_clones() {
        let temporary = TempDir::new().expect("temporary root");
        let bus = temporary.path().join("bus");
        let (first_workspace, first_instance) = workspace(temporary.path(), "Builder One");
        let (second_workspace, second_instance) = workspace(temporary.path(), "Builder Two");
        let first = A2aRuntime::start(&bus, &first_workspace, &first_instance, options())
            .expect("first runtime");
        let second = A2aRuntime::start(&bus, &second_workspace, &second_instance, options())
            .expect("second runtime");

        assert_eq!(first.list_peers().expect("first peers").len(), 1);
        assert_eq!(second.list_peers().expect("second peers").len(), 1);
        let first_clone = first.clone();
        let receipt = first_clone
            .enqueue(
                &second_instance.instance_id,
                &request("msg-1", "hello peer"),
            )
            .expect("enqueue from clone");
        let pending = second.list_pending().expect("pending summaries");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].delivery_id, receipt.delivery_id);
        assert_eq!(pending[0].part_count, 1);
        let received = second
            .claim_pending(&receipt.delivery_id)
            .expect("claim exact message");
        assert_eq!(received.sender_instance_id, first_instance.instance_id);
        second
            .acknowledge(&received.delivery_id)
            .expect("acknowledge");
        assert_eq!(first.own_peer().expect("own peer").name, "Builder One");

        first.shutdown().expect("first shutdown");
        second.shutdown().expect("second shutdown");
    }

    #[test]
    fn heartbeat_keeps_runtime_live_past_multiple_leases() {
        let temporary = TempDir::new().expect("temporary root");
        let bus = temporary.path().join("bus");
        let (first_workspace, first_instance) = workspace(temporary.path(), "Heartbeat One");
        let (second_workspace, second_instance) = workspace(temporary.path(), "Heartbeat Two");
        let first = A2aRuntime::start(&bus, &first_workspace, &first_instance, options())
            .expect("first runtime");
        let second = A2aRuntime::start(&bus, &second_workspace, &second_instance, options())
            .expect("second runtime");

        thread::sleep(Duration::from_millis(240));
        assert_eq!(second.list_peers().expect("live peers").len(), 1);
        assert_eq!(first.health().state, RuntimeState::Healthy);
        first.shutdown().expect("first shutdown");
        second.shutdown().expect("second shutdown");
    }

    #[test]
    fn shutdown_joins_worker_and_removes_only_owned_lease() {
        let temporary = TempDir::new().expect("temporary root");
        let bus = temporary.path().join("bus");
        let (first_workspace, first_instance) = workspace(temporary.path(), "Shutdown One");
        let (second_workspace, second_instance) = workspace(temporary.path(), "Shutdown Two");
        let first = A2aRuntime::start(&bus, &first_workspace, &first_instance, options())
            .expect("first runtime");
        let second = A2aRuntime::start(&bus, &second_workspace, &second_instance, options())
            .expect("second runtime");

        first.shutdown().expect("first shutdown");
        assert_eq!(first.health().state, RuntimeState::Stopped);
        assert!(first.list_peers().is_err());
        assert_eq!(second.list_peers().expect("remaining peers"), Vec::new());
        second.shutdown().expect("second shutdown");
    }

    #[test]
    fn final_clone_drop_joins_and_unregisters() {
        let temporary = TempDir::new().expect("temporary root");
        let bus = temporary.path().join("bus");
        let (first_workspace, first_instance) = workspace(temporary.path(), "Drop One");
        let (second_workspace, second_instance) = workspace(temporary.path(), "Drop Two");
        let first = A2aRuntime::start(&bus, &first_workspace, &first_instance, options())
            .expect("first runtime");
        let final_clone = first.clone();
        let second = A2aRuntime::start(&bus, &second_workspace, &second_instance, options())
            .expect("second runtime");

        drop(first);
        assert_eq!(second.list_peers().expect("peer retained").len(), 1);
        drop(final_clone);
        assert_eq!(second.list_peers().expect("peer removed"), Vec::new());
        second.shutdown().expect("second shutdown");
    }

    #[test]
    fn health_reports_forced_bus_failure_without_error_text() {
        let temporary = TempDir::new().expect("temporary root");
        let bus = temporary.path().join("bus");
        let (workspace, instance) = workspace(temporary.path(), "Health Runtime");
        let runtime = A2aRuntime::start(&bus, &workspace, &instance, options()).expect("runtime");
        let registry = bus.join("registry");
        let unavailable = bus.join("registry-unavailable");
        fs::rename(&registry, &unavailable).expect("force registry failure");

        wait_until(Duration::from_secs(1), || {
            runtime.health().last_issue == Some(RuntimeIssue::HeartbeatFailed)
        });
        let health = runtime.health();
        assert_eq!(health.state, RuntimeState::Degraded);
        assert!(health.consecutive_heartbeat_failures > 0);

        fs::rename(&unavailable, &registry).expect("restore registry");
        runtime.shutdown().expect("shutdown");
    }

    #[test]
    fn artifact_staging_delegates_with_memory_only_workspace() {
        let temporary = TempDir::new().expect("temporary root");
        let bus = temporary.path().join("bus");
        let (workspace, instance) = workspace(temporary.path(), "Artifact Runtime");
        fs::write(workspace.join("result.txt"), b"runtime artifact\n").expect("artifact source");
        let runtime = A2aRuntime::start(&bus, &workspace, &instance, options()).expect("runtime");

        let artifact = runtime
            .stage_artifact(Path::new("result.txt"))
            .expect("stage artifact");
        assert!(artifact.relative_url.starts_with("artifacts/crumb_"));
        assert_eq!(artifact.size_bytes, 17);
        let registry = fs::read_to_string(
            bus.join("registry")
                .join(format!("{}.json", instance.instance_id)),
        )
        .expect("registry entry");
        assert!(!registry.contains(workspace.to_string_lossy().as_ref()));
        assert!(runtime.stage_artifact(Path::new("../result.txt")).is_err());
        runtime.shutdown().expect("shutdown");
    }
}
