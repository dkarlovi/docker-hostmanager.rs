use anyhow::{Context, Result};
use bollard::models::{ContainerInspectResponse, EventActor, EventMessage, EventMessageTypeEnum};
use bollard::query_parameters::EventsOptions;
use bollard::Docker;
use colored::Colorize;

use crate::engine::Engine;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tokio::time::{sleep, Duration};
use tokio_stream::StreamExt;
use tracing::{debug, error, info, warn};

use crate::health::{now_secs, HealthState};
use crate::types::{ContainerInfo, NetworkInfo};

const START_TAG: &str = "## docker-hostmanager-start";
const END_TAG: &str = "## docker-hostmanager-end";

/// The network `docker run` attaches to when none is given.
const DEFAULT_BRIDGE_NETWORK: &str = "bridge";

/// Longest name a resolver accepts: RFC 1035 caps a full domain name at 253
/// characters in its dotted text form.
const MAX_HOSTNAME_LEN: usize = 253;

/// Length of the abbreviated container IDs shown in output, matching `docker ps`.
const SHORT_ID_LEN: usize = 12;

/// How often the event loop refreshes its liveness timestamp when Docker is quiet.
const HEARTBEAT_SECS: u64 = 10;

/// Whether the hosts file can be protected against a write failing part-way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReservationSupport {
    /// Space can be reserved up front, so a failed write cannot damage the file.
    /// Only reachable on Linux, the only platform offering a reservation.
    #[cfg_attr(
        not(target_os = "linux"),
        expect(dead_code, reason = "only constructed by the Linux reservation probe")
    )]
    Available,
    /// No reservation is possible; only the restore-on-failure path applies.
    Unavailable(String),
    /// The file cannot be opened for writing at all, so *every* update will
    /// fail. Worth reporting separately and loudly: it is a misconfiguration
    /// rather than a weaker guarantee.
    NotWritable(String),
}

/// Checks whether space reservation actually works for `path`.
///
/// This probes the real file rather than inferring from the platform, because
/// support is a property of the *filesystem*: a Linux kernel says nothing about
/// whether the particular filesystem under `/etc/hosts` implements `fallocate`.
///
/// The probe cannot disturb the file. For a non-empty file it reserves exactly
/// the bytes already occupied, which changes neither size nor contents; for an
/// empty one it uses `FALLOC_FL_KEEP_SIZE`, which reserves a block while leaving
/// the file empty.
#[cfg(target_os = "linux")]
#[must_use]
pub fn probe_reservation_support(path: &Path) -> ReservationSupport {
    use std::os::unix::io::AsRawFd;

    let file = match fs::OpenOptions::new().write(true).open(path) {
        Ok(file) => file,
        Err(e) => return ReservationSupport::NotWritable(e.to_string()),
    };
    let size = file.metadata().map_or(0, |m| m.len());

    let (mode, len) = if size > 0 {
        (0, i64::try_from(size).unwrap_or(i64::MAX))
    } else {
        (libc::FALLOC_FL_KEEP_SIZE, 1)
    };

    #[expect(unsafe_code, reason = "fallocate(2) has no safe wrapper in libc")]
    // SAFETY: `file` owns a valid open file descriptor for the whole call, and
    // `fallocate` only inspects that descriptor and the three integer arguments.
    let rc = unsafe { libc::fallocate(file.as_raw_fd(), mode, 0, len) };
    if rc == 0 {
        ReservationSupport::Available
    } else {
        ReservationSupport::Unavailable(std::io::Error::last_os_error().to_string())
    }
}

/// Elsewhere there is no reservation to offer, but the writability check is
/// still worth doing — it catches a misconfigured target at startup.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn probe_reservation_support(path: &Path) -> ReservationSupport {
    match fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => ReservationSupport::Unavailable("only supported on Linux".to_string()),
        Err(e) => ReservationSupport::NotWritable(e.to_string()),
    }
}

/// Reserves `len` bytes for `file`, so a subsequent write cannot fail for want
/// of space. Returns `Ok(false)` when no such guarantee is available, which the
/// caller treats as a reason to proceed carefully rather than to give up.
///
/// Linux gets the real thing via `fallocate(2)`. This is the deployment target
/// that matters — the daemon ships as a Linux container — and it is what makes
/// the ENOSPC guarantee above hold.
#[cfg(target_os = "linux")]
fn reserve_space(file: &fs::File, len: usize) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd;

    let reserve = i64::try_from(len)
        .map_err(|_ignored| std::io::Error::other("hosts file content too large"))?;

    #[expect(unsafe_code, reason = "fallocate(2) has no safe wrapper in libc")]
    // SAFETY: `file` owns a valid open file descriptor for the whole call, and
    // `fallocate` only inspects that descriptor and the three integer arguments.
    let rc = unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, reserve) };
    if rc == 0 {
        return Ok(true);
    }

    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        // Not every filesystem implements fallocate; fall back rather than
        // refuse to write at all.
        Some(libc::EOPNOTSUPP | libc::ENOSYS) => Ok(false),
        _ => Err(err),
    }
}

/// macOS and Windows have no portable equivalent that is worth the complexity
/// here, so they rely on the caller's restore-on-failure path instead.
#[cfg(not(target_os = "linux"))]
fn reserve_space(_file: &fs::File, _len: usize) -> std::io::Result<bool> {
    Ok(false)
}

/// Writes `content` to `path` without ever leaving the file truncated, empty or
/// half-written, restoring `previous` if the write fails once already underway.
///
/// The obvious implementation — write a temporary file, then rename it over the
/// target — is **not** available here. The hosts file is bind-mounted into the
/// container as a single file (`/etc/hosts` -> `/hosts`), and a rename replaces
/// the inode: the container's mount would stay pinned to the old, orphaned inode
/// and every later write would vanish while the real file froze. (Renaming over
/// a mount point fails with `EBUSY` in the first place.) The write therefore has
/// to happen in place, on the same inode.
///
/// What makes that safe is reserving the space with `fallocate(2)` *before*
/// modifying anything. On a full filesystem the reservation fails while the old
/// contents are still completely intact, so the caller gets an error instead of
/// a destroyed hosts file.
///
/// This replaces a `fs::write` call, which is `File::create` + `write_all` —
/// `O_TRUNC` first, write second. On 2026-08-07 a transient ENOSPC on the host
/// hit exactly that window and left the user's `/etc/hosts` at zero bytes.
///
/// The reservation is Linux-only (see [`reserve_space`]); elsewhere the
/// restore-on-failure path below is what keeps the file consistent.
fn write_in_place(path: &Path, content: &str, previous: &str) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};

    let bytes = content.as_bytes();

    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("Failed to open {} for writing", path.display()))?;

    // The step that must fail on a full filesystem, while the old contents
    // are still there. Note this deliberately does not truncate first.
    match reserve_space(&file, bytes.len()) {
        Ok(true) => {}
        Ok(false) => {
            debug!(
                "No space reservation available for {}, writing without one",
                path.display()
            );
        }
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "Failed to reserve {} bytes for {}; leaving it unchanged",
                    bytes.len(),
                    path.display()
                )
            });
        }
    }

    let write = |file: &mut fs::File, data: &[u8]| -> std::io::Result<()> {
        file.seek(SeekFrom::Start(0))?;
        file.write_all(data)?;
        // Trim whatever the previous, longer contents left behind.
        file.set_len(u64::try_from(data.len()).map_err(std::io::Error::other)?)?;
        file.sync_all()
    };

    if let Err(e) = write(&mut file, bytes) {
        // Space was reserved, so this is not ENOSPC — but the file may now be
        // partially written, which is worse than either state. Put back what
        // was there before, best effort.
        warn!(
            "Write to {} failed after space was reserved; restoring previous contents",
            path.display()
        );
        if let Err(restore) = write(&mut file, previous.as_bytes()) {
            error!(
                "Failed to restore previous contents of {}: {restore}",
                path.display()
            );
        }
        return Err(e).with_context(|| format!("Failed to write {}", path.display()));
    }

    Ok(())
}

/// Replaces each `{VAR}` in `input` with its value from `vars`. Unknown
/// variables are left as written, so they can be reported as unresolved.
fn expand_template(input: &str, vars: &HashMap<String, String>) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            let mut var_name = String::new();
            let mut closed = false;
            while let Some(&next_c) = chars.peek() {
                if next_c == '}' {
                    closed = true;
                    chars.next(); // consume '}'
                    break;
                } else if next_c == '{' {
                    break;
                }
                var_name.push(next_c);
                chars.next();
            }
            let value = (closed && !var_name.is_empty())
                .then(|| vars.get(&var_name))
                .flatten();
            if let Some(val) = value {
                result.push_str(val);
            } else {
                // Not a known variable: put back exactly what was consumed.
                result.push('{');
                result.push_str(&var_name);
                if closed {
                    result.push('}');
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// The lines that belong to the user: everything except our section markers.
///
/// Markers are never the user's. Stray ones (e.g. left by a hand edit) are
/// dropped rather than preserved, or every write would add a new section.
fn user_lines<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    lines
        .iter()
        .copied()
        .filter(|line| !matches!(line.trim(), START_TAG | END_TAG))
        .collect()
}

/// The line printed after the hosts file was rewritten.
fn write_summary(has_section: bool, container_count: usize, hostname_count: usize) -> String {
    if has_section {
        format!(
            "{} Updated hosts file: {} containers, {} hostnames",
            "✓".bright_green(),
            container_count.to_string().bright_white(),
            hostname_count.to_string().bright_white()
        )
    } else {
        format!(
            "{} Removed empty managed section from hosts file",
            "✓".bright_green()
        )
    }
}

/// The first line of `preserved` that `rendered` no longer contains, if any.
fn first_dropped_line<'a>(preserved: &[&'a str], rendered: &[String]) -> Option<&'a str> {
    preserved
        .iter()
        .copied()
        .find(|line| !rendered.iter().any(|kept| kept == line))
}

/// Abbreviates container `id` the way the Docker CLI does.
fn short_id(id: &str) -> &str {
    id.get(..SHORT_ID_LEN).unwrap_or(id)
}

/// Whether `name` can be written into the hosts file without changing its
/// structure: hostname characters only, plus braces so an unresolved
/// `{TEMPLATE}` can still be reported as skipped.
fn is_hosts_file_safe(name: &str) -> bool {
    (1..=MAX_HOSTNAME_LEN).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '{' | '}'))
}

/// What a hosts file update ended up doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The file was rewritten with new contents.
    Written,
    /// No entries were left, so the managed section was removed.
    Removed,
    /// The file already had the desired contents, so it was left alone.
    Unchanged,
    /// Watch mode: entries were displayed, never written.
    DryRun,
}

/// A hostname a container could not claim because another container owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostnameConflict {
    pub hostname: String,
    pub owner_name: String,
}

#[derive(Debug)]
pub struct Synchronizer {
    engine: Engine,
    hosts_file: PathBuf,
    tld: String,
    write_enabled: bool,
    debounce_ms: u64,
    active_containers: Mutex<BTreeMap<String, ContainerInfo>>,
    /// Maps each hostname to the (`container_id`, `container_name`) of the first claimant.
    /// Dynamic network-alias hostnames are also tracked here; they just never conflict
    /// because they always include a unique network name.
    hostname_claims: Mutex<HashMap<String, (String, String)>>,
    write_notify: Notify,
    health: Arc<HealthState>,
    /// Unix seconds at which the last full sync started; 0 before the first.
    sync_started_at: AtomicU64,
}

impl Synchronizer {
    pub fn new(
        docker: Docker,
        hosts_file: PathBuf,
        tld: String,
        write_enabled: bool,
        debounce_ms: u64,
    ) -> Self {
        Self::with_engine(
            Engine::Native(docker),
            hosts_file,
            tld,
            write_enabled,
            debounce_ms,
        )
    }

    /// Creates a synchronizer backed by a fake daemon, and the handle that drives it.
    #[cfg(test)]
    pub fn new_fake(
        hosts_file: PathBuf,
        tld: String,
        write_enabled: bool,
        debounce_ms: u64,
    ) -> (Self, crate::engine::fake::Ctrl) {
        let ctrl = crate::engine::fake::Ctrl::default();
        let sync = Self::with_engine(
            Engine::Fake(ctrl.clone()),
            hosts_file,
            tld,
            write_enabled,
            debounce_ms,
        );
        (sync, ctrl)
    }

    fn with_engine(
        engine: Engine,
        hosts_file: PathBuf,
        tld: String,
        write_enabled: bool,
        debounce_ms: u64,
    ) -> Self {
        Self {
            engine,
            hosts_file,
            tld,
            write_enabled,
            debounce_ms,
            active_containers: Mutex::new(BTreeMap::new()),
            hostname_claims: Mutex::new(HashMap::new()),
            write_notify: Notify::new(),
            health: Arc::new(HealthState::new()),
            sync_started_at: AtomicU64::new(0),
        }
    }

    /// Shared liveness state, for the health socket served alongside the event loop.
    #[must_use]
    pub fn health(&self) -> Arc<HealthState> {
        Arc::clone(&self.health)
    }

    pub async fn synchronize(&self) -> Result<()> {
        self.sync_started_at.store(now_secs(), Ordering::Relaxed);

        info!("Fetching running containers...");

        let containers = self
            .engine
            .list_running()
            .await
            .context("Failed to list containers")?;

        info!("Found {} running containers", containers.len());

        // Clear all state for a clean rebuild
        self.active_containers.lock().await.clear();
        self.hostname_claims.lock().await.clear();

        // Inspect all containers, collecting start times for deterministic ordering
        let mut inspected: Vec<(Option<String>, String, ContainerInfo)> = Vec::new();
        for container in containers {
            let id = container.id.unwrap_or_default();
            if id.is_empty() {
                continue;
            }

            match self.inspect_container_with_start_time(&id).await {
                Ok((started_at, Some(info))) => inspected.push((started_at, id, info)),
                Ok((_, None)) => {}
                Err(e) => {
                    warn!("Failed to inspect container {}: {}", id, e);
                }
            }
        }

        // Sort by start time so the earliest-started container wins conflicts,
        // giving the same outcome regardless of when the manager itself starts.
        inspected.sort_by(|(a, _, _), (b, _, _)| {
            a.as_deref().unwrap_or("").cmp(b.as_deref().unwrap_or(""))
        });

        // Claim hostnames and populate active containers in start-time order
        for (_, id, info) in inspected {
            let short_id = short_id(&id);
            debug!("Adding container: {} ({})", info.name, short_id);
            self.claim_hostnames(&id, &info).await;
            let mut active = self.active_containers.lock().await;
            active.insert(id, info);
        }

        // As in `process_pending_writes`, a failure here is logged rather than
        // fatal: the daemon stays up and retries on the next container event.
        match self.write_hosts_file_immediate().await {
            Ok(_) => self.health.set_write_ok(true),
            Err(e) => {
                self.health.set_write_ok(false);
                error!("Failed to write hosts file during initial sync (will retry on next event): {e:#}");
            }
        }

        Ok(())
    }

    fn schedule_write(&self) {
        self.write_notify.notify_one();
    }

    async fn process_pending_writes(&self) -> Result<()> {
        loop {
            // Idle until the first event signals a pending write
            self.write_notify.notified().await;

            // Debounce: each new event resets the timer; write only after
            // debounce_ms of silence
            loop {
                let mut notified = std::pin::pin!(self.write_notify.notified());
                notified.as_mut().enable();

                tokio::select! {
                    () = sleep(Duration::from_millis(self.debounce_ms)) => {
                        // A write failure must never take the daemon down. On
                        // 2026-08-07 a transient ENOSPC propagated from here,
                        // aborted `listen_events`, exited `main`, and left the
                        // user without hostname management for hours. Log it and
                        // keep serving; the next event retries the write.
                        match self.write_hosts_file_immediate().await {
                            Ok(_) => self.health.set_write_ok(true),
                            Err(e) => {
                                self.health.set_write_ok(false);
                                error!("Failed to write hosts file (will retry on next event): {e:#}");
                            }
                        }
                        break;
                    }
                    () = notified => {
                        // New event during debounce window — loop resets the timer
                    }
                }
            }
        }
    }

    pub async fn listen_events(&self) -> Result<()> {
        // Replay from when the initial sync began, so a container that started
        // between listing and subscribing is not missed. Replayed events for
        // containers already synced are idempotent.
        let since = match self.sync_started_at.load(Ordering::Relaxed) {
            0 => None,
            secs => Some(secs),
        };
        let events = self.engine.events(events_options(since));

        tokio::select! {
            result = self.consume_events(events) => result,
            result = self.process_pending_writes() => result,
            result = self.mark_alive() => result,
        }
    }

    async fn consume_events<S>(&self, events: S) -> Result<()>
    where
        S: tokio_stream::Stream<Item = Result<EventMessage, bollard::errors::Error>>,
    {
        let mut pinned = std::pin::pin!(events);
        while let Some(event_result) = pinned.next().await {
            self.health.touch();
            match event_result {
                Ok(event) => {
                    if let Err(e) = self.handle_event(event).await {
                        error!("Error handling event: {}", e);
                    }
                }
                Err(e) => {
                    error!("Error receiving event: {}", e);
                }
            }
        }
        // Docker closes the stream when the daemon restarts. Exiting non-zero lets
        // the restart policy bring us back with a full resync; returning Ok would
        // look like a clean shutdown while every later event went unseen.
        anyhow::bail!("Docker event stream ended")
    }

    /// Keeps the liveness timestamp fresh on a quiet Docker host, where hours can
    /// pass without a single container event. Because the runtime is
    /// `current_thread`, this stops advancing the moment anything blocks the
    /// event loop, which is exactly what the health probe needs to see.
    #[expect(
        clippy::infinite_loop,
        reason = "runs until the surrounding select! drops it"
    )]
    async fn mark_alive(&self) -> Result<()> {
        let mut ticker = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
        loop {
            ticker.tick().await;
            self.health.touch();
        }
    }

    async fn handle_event(&self, event: EventMessage) -> Result<()> {
        let action = event.action.as_deref().unwrap_or("");
        let event_type = event.typ;
        let actor_ref = event.actor.as_ref();
        let actor_id = actor_ref.and_then(|a| a.id.as_deref()).unwrap_or("");

        if actor_id.is_empty() {
            return Ok(());
        }

        let short_actor_id = short_id(actor_id);
        debug!(
            "Event: {} {} ({})",
            action,
            short_actor_id,
            event_type
                .map(|t| format!("{t:?}"))
                .as_deref()
                .unwrap_or("unknown")
        );

        match (event_type, action) {
            // Container lifecycle: full claim/release of all hostnames the container owns.
            (Some(EventMessageTypeEnum::CONTAINER), "start" | "unpause") => {
                self.handle_container_up(actor_id).await?;
            }
            (
                Some(EventMessageTypeEnum::CONTAINER),
                "die" | "stop" | "kill" | "pause" | "destroy",
            ) => {
                self.handle_container_down(actor_id).await;
            }
            // Network events: actor.id is the network ID; the affected container ID
            // lives in attributes. Only the hostnames bound to the specific network
            // should be touched — never the container's claims on other networks.
            (Some(EventMessageTypeEnum::NETWORK), "connect") => {
                if let Some((container_id, _network_name)) = network_event_targets(actor_ref) {
                    self.handle_container_up(&container_id).await?;
                }
            }
            (Some(EventMessageTypeEnum::NETWORK), "disconnect") => {
                if let Some((container_id, network_name)) = network_event_targets(actor_ref) {
                    self.handle_network_disconnect(&container_id, &network_name)
                        .await;
                }
            }
            _ => {}
        }

        Ok(())
    }

    async fn handle_container_up(&self, container_id: &str) -> Result<()> {
        if let Some(info) = self.inspect_container(container_id).await? {
            let short_actor_id = short_id(container_id);
            println!(
                "{} Container {} ({})",
                "▶".bright_green(),
                info.name.bright_white(),
                short_actor_id.bright_black()
            );
            self.claim_hostnames(container_id, &info).await;
            let mut active = self.active_containers.lock().await;
            active.insert(container_id.to_string(), info);
            drop(active);
            self.schedule_write();
        }
        Ok(())
    }

    async fn handle_container_down(&self, container_id: &str) {
        let mut active = self.active_containers.lock().await;
        if let Some(info) = active.remove(container_id) {
            drop(active);
            self.release_hostnames(container_id, &info, None).await;
            let short_actor_id = short_id(container_id);
            println!(
                "{} Container {} ({})",
                "■".bright_red(),
                info.name.bright_white(),
                short_actor_id.bright_black()
            );
            self.schedule_write();
        }
    }

    /// Handles a network `disconnect` event: drops only the claims bound to the
    /// specific network the container left, and refreshes the active state to
    /// reflect the new network attachment set. The container itself stays running,
    /// so claims on its other networks must be preserved.
    async fn handle_network_disconnect(&self, container_id: &str, network_name: &str) {
        // Read the snapshot we hold for this container; if we don't know about it,
        // there's nothing to release.
        let snapshot = {
            let active = self.active_containers.lock().await;
            active.get(container_id).cloned()
        };
        let Some(info) = snapshot else { return };

        self.release_hostnames(container_id, &info, Some(network_name))
            .await;

        // Refresh container state from Docker so subsequent reconnects see the
        // current network set. If the container is gone or no longer exposes
        // anything, treat as a full down event.
        let refreshed = self.inspect_container(container_id).await;
        let mut active = self.active_containers.lock().await;
        if let Ok(Some(info)) = refreshed {
            active.insert(container_id.to_string(), info);
        } else {
            active.remove(container_id);
        }
        drop(active);

        self.schedule_write();
    }

    async fn inspect_container(&self, id: &str) -> Result<Option<ContainerInfo>> {
        let (_, info) = self.inspect_container_with_start_time(id).await?;
        Ok(info)
    }

    /// Attempts to claim all hostnames generated by `container`. The first container
    /// to claim a hostname owns it until it stops. Warns once on conflict.
    ///
    /// Returns the conflicts that were skipped, so callers (and tests) can observe
    /// them without scraping log output.
    async fn claim_hostnames(
        &self,
        container_id: &str,
        container: &ContainerInfo,
    ) -> Vec<HostnameConflict> {
        let all_hostnames: Vec<String> = container
            .get_hostnames(&self.tld)
            .into_iter()
            .flat_map(|(_, hosts)| hosts)
            .collect();

        let mut conflicts = Vec::new();
        let mut claims = self.hostname_claims.lock().await;
        for hostname in all_hostnames {
            match claims.entry(hostname.clone()) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    debug!(
                        "Container \"{}\" claiming hostname \"{}\"",
                        container.name, hostname
                    );
                    e.insert((container_id.to_string(), container.name.clone()));
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    let (owner_id, owner_name) = e.get().clone();
                    if owner_id == container_id {
                        // Not a conflict: `handle_container_up` runs for both the
                        // container `start` and the network `connect` event, so a
                        // container re-claims its own hostnames. Refresh the
                        // recorded name and move on.
                        e.insert((container_id.to_string(), container.name.clone()));
                        continue;
                    }
                    warn!(
                        "Hostname \"{}\" already claimed by \"{}\", skipping for \"{}\"",
                        hostname, owner_name, container.name
                    );
                    conflicts.push(HostnameConflict {
                        hostname,
                        owner_name,
                    });
                }
            }
        }
        conflicts
    }

    /// Releases hostname claims held by `container_id`. When `only_network` is
    /// `Some(name)`, only hostnames bound to that specific network attachment are
    /// released — claims tied to the container's other networks are preserved.
    /// When `None`, all of the container's hostnames are released (used for full
    /// container teardown: die/stop/kill/pause/destroy).
    async fn release_hostnames(
        &self,
        container_id: &str,
        container: &ContainerInfo,
        only_network: Option<&str>,
    ) {
        let only_network_ip =
            only_network.and_then(|net| container.networks.get(net).map(|n| n.ip_address.as_str()));

        // When filtering to a specific network, keep only the entry whose IP
        // matches that network's IP. The global-ip entry (used when the container
        // has no per-network IP) is intentionally skipped — it isn't bound to any
        // one network, so a network disconnect shouldn't drop it.
        let all_hostnames: Vec<String> = container
            .get_hostnames(&self.tld)
            .into_iter()
            .filter(|(ip, _)| only_network_ip.is_none_or(|net_ip| ip == net_ip))
            .flat_map(|(_, hosts)| hosts)
            .collect();

        let mut claims = self.hostname_claims.lock().await;
        for hostname in all_hostnames {
            if claims
                .get(&hostname)
                .is_some_and(|(id, _)| id == container_id)
            {
                debug!(
                    "Container \"{}\" releasing hostname \"{}\"",
                    container.name, hostname
                );
                claims.remove(&hostname);
            }
        }
    }

    /// Like `inspect_container` but also returns the container's `started_at` timestamp,
    /// used in `synchronize()` to sort containers before claiming hostnames.
    ///
    /// The info is `None` when the container should get no hosts entries.
    async fn inspect_container_with_start_time(
        &self,
        id: &str,
    ) -> Result<(Option<String>, Option<ContainerInfo>)> {
        let container = self
            .engine
            .inspect(id)
            .await
            .context("Failed to inspect container")?;
        let started_at = container.state.as_ref().and_then(|s| s.started_at.clone());
        // The one place routability is decided: a container we cannot address
        // gets no hosts entries, wherever it was inspected from.
        let info = extract_container_info(container).filter(ContainerInfo::is_routable);
        Ok((started_at, info))
    }

    async fn write_hosts_file_immediate(&self) -> Result<WriteOutcome> {
        // Snapshot both maps so we don't hold locks during file I/O.
        let active_containers: BTreeMap<String, ContainerInfo> =
            self.active_containers.lock().await.clone();
        let claims: HashMap<String, (String, String)> = self.hostname_claims.lock().await.clone();

        // Build new hosts entries, filtering out hostnames claimed by other containers.
        let mut host_entries_with_ip = Vec::new();
        let container_count = active_containers.len();
        let mut hostname_count: usize = 0;

        for (container_id, container) in &active_containers {
            let hostnames = container.get_hostnames(&self.tld);

            for (ip, hosts) in hostnames {
                let mut kept = Vec::new();
                let mut skipped = Vec::new();

                for h in hosts {
                    // Names come from image-controlled env vars and labels. Anything
                    // that could break out of its hosts line (whitespace, newlines,
                    // `#`) is dropped outright, not even echoed into a comment.
                    if !is_hosts_file_safe(&h) {
                        warn!(
                            "Ignoring invalid hostname {h:?} from container \"{}\"",
                            container.name
                        );
                        continue;
                    }
                    // Unresolved variables e.g. {MISSING_VAR}, or a lone brace from a
                    // malformed template, shouldn't be added as hostnames.
                    if h.contains(['{', '}']) {
                        skipped.push(h);
                    } else if claims
                        .get(&h)
                        .is_none_or(|(owner_id, _)| owner_id == container_id)
                    {
                        kept.push(h);
                    } else {
                        skipped.push(h);
                    }
                }

                if kept.is_empty() && !skipped.is_empty() {
                    // All hostnames for this IP were claimed by another container.
                    // Write a comment-only line so the skip is visible in the file.
                    host_entries_with_ip.push((
                        ip.clone(),
                        format!(
                            "# {} ({}): all hostnames skipped: {}",
                            ip,
                            container.name,
                            skipped.join(", ")
                        ),
                    ));
                } else if !kept.is_empty() {
                    let skip_comment = if skipped.is_empty() {
                        String::new()
                    } else {
                        format!("  # skipped: {}", skipped.join(", "))
                    };
                    host_entries_with_ip.push((
                        ip.clone(),
                        format!("{} {}{}", ip, kept.join(" "), skip_comment),
                    ));
                    hostname_count = hostname_count.saturating_add(kept.len());
                }
            }
        }

        // Numeric order, so 172.17.0.10 follows 172.17.0.9. The sort is stable,
        // which keeps lines sharing an IP in the order they were generated.
        host_entries_with_ip
            .sort_by_cached_key(|(ip, _)| (ip.parse::<std::net::IpAddr>().ok(), ip.clone()));
        let host_entries: Vec<String> = host_entries_with_ip
            .into_iter()
            .map(|(_, line)| line)
            .collect();
        // Whether a managed section will exist after this write; containers
        // whose names were all dropped as invalid contribute no lines.
        let has_section = !host_entries.is_empty();

        // Display the output
        println!();
        if self.write_enabled {
            if host_entries.is_empty() {
                println!("{} No active containers to write", "→".bright_cyan());
            } else {
                println!("{} Hosts entries to be written:", "→".bright_cyan());
                for line in &host_entries {
                    println!("  {}", line.bright_white());
                }
            }
        } else if host_entries.is_empty() {
            println!("{} No active containers", "→".bright_cyan());
        } else {
            println!("{} Generated hosts entries:", "→".bright_cyan());
            for line in &host_entries {
                println!("  {}", line.bright_white());
            }
        }
        println!();

        if !self.write_enabled {
            println!(
                "{} {} containers, {} hostnames",
                "ℹ".bright_blue(),
                container_count.to_string().bright_white(),
                hostname_count.to_string().bright_white()
            );
            return Ok(WriteOutcome::DryRun);
        }

        // Write mode: actually update the file
        let content = fs::read_to_string(&self.hosts_file).context("Failed to read hosts file")?;

        let lines: Vec<&str> = content.lines().collect();

        // Find the managed section. It ends at the first end marker *after* its
        // start; an end marker before it is stray, e.g. from a hand edit.
        let start_idx = lines.iter().position(|line| line.trim() == START_TAG);
        let end_idx = start_idx.and_then(|start| {
            lines
                .iter()
                .enumerate()
                .skip(start)
                .find(|(_, line)| line.trim() == END_TAG)
                .map(|(idx, _)| idx)
        });

        let mut new_lines = Vec::new();
        // Every line the user owns, i.e. everything outside our managed section.
        // Checked against the rendered result below so we can never drop one.
        let mut preserved: Vec<&str> = Vec::new();

        if let (Some(start), Some(end)) = (start_idx, end_idx) {
            // Managed section exists - replace it
            if let Some(before_managed) = lines.get(..start) {
                let before = user_lines(before_managed);
                new_lines.extend(before.iter().map(ToString::to_string));
                preserved.extend(before);
            }

            if !host_entries.is_empty() {
                // Add our managed section
                new_lines.push(START_TAG.to_string());
                new_lines.extend(host_entries);
                new_lines.push(END_TAG.to_string());
            }
            // Note: if host_entries is empty, we don't add the tags (removes empty section)

            // Everything after the end tag, which is itself skipped.
            if let Some(after_managed) = lines.get(end..).and_then(|rest| rest.get(1..)) {
                let after = user_lines(after_managed);
                new_lines.extend(after.iter().map(ToString::to_string));
                preserved.extend(after);
            }
        } else {
            // No valid managed section - append to end
            preserved.extend(user_lines(&lines));
            new_lines.extend(preserved.iter().map(ToString::to_string));

            if !host_entries.is_empty() {
                // Add a blank line before our section if the file doesn't end with one
                if let Some(last_line) = new_lines.last() {
                    if !last_line.is_empty() {
                        new_lines.push(String::new());
                    }
                }

                new_lines.push(START_TAG.to_string());
                new_lines.extend(host_entries);
                new_lines.push(END_TAG.to_string());
            }
        }

        let new_content = new_lines.join("\n") + "\n";

        // Last line of defence before touching the file: whatever happens to the
        // managed section, not one line the user wrote may go missing.
        if let Some(dropped) = first_dropped_line(&preserved, &new_lines) {
            return Err(anyhow::anyhow!(
                "refusing to write {}: it would drop the existing line {dropped:?}",
                self.hosts_file.display()
            ));
        }

        // Most events (restarts, duplicate start/stop pairs, replays) change
        // nothing; leave the system file and its mtime alone then.
        if new_content == content {
            debug!("{} already up to date", self.hosts_file.display());
            return Ok(WriteOutcome::Unchanged);
        }

        write_in_place(&self.hosts_file, &new_content, &content)?;

        println!(
            "{}",
            write_summary(has_section, container_count, hostname_count)
        );

        Ok(if has_section {
            WriteOutcome::Written
        } else {
            WriteOutcome::Removed
        })
    }
}

/// Turns Docker's inspection data into the hostnames-relevant view of a
/// container, or `None` if it is not running and publishes no ports.
fn extract_container_info(container: ContainerInspectResponse) -> Option<ContainerInfo> {
    let id = container.id?;
    let name = container.name?.trim_start_matches('/').to_string();

    let state = container.state?;
    let running = state.running.unwrap_or(false);

    let network_settings = container.network_settings?;

    // Check if container has exposed ports
    let has_ports = network_settings.ports.is_some_and(|p| !p.is_empty());

    if !has_ports && !running {
        return None;
    }

    let networks: BTreeMap<String, NetworkInfo> = network_settings
        .networks
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(network_name, network)| {
            let ip = network.ip_address.filter(|ip| !ip.is_empty())?;
            // No need to add the container name here: `get_hostnames` always
            // emits `{name}.{network}` ahead of the aliases.
            let aliases = network.aliases.unwrap_or_default();

            Some((
                network_name,
                NetworkInfo {
                    ip_address: ip,
                    aliases,
                },
            ))
        })
        .collect();

    // Build variables map from environment variables and specific labels
    let mut vars = HashMap::new();
    let mut compose_project: Option<String> = None;

    if let Some(config) = &container.config {
        if let Some(env_vars) = &config.env {
            for env_var in env_vars {
                if let Some((k, v)) = env_var.split_once('=') {
                    vars.insert(k.to_string(), v.to_string());
                }
            }
        }
        if let Some(labels) = &config.labels {
            if let Some(proj) = labels.get("com.docker.compose.project") {
                compose_project = Some(proj.clone());
            }
        }
    }

    if !vars.contains_key("COMPOSE_PROJECT_NAME") {
        let proj_name =
            compose_project.unwrap_or_else(|| name.split('-').next().unwrap_or(&name).to_string());
        vars.insert("COMPOSE_PROJECT_NAME".to_string(), proj_name);
    }

    // Extract DOMAIN_NAME environment variable
    let mut domain_names = Vec::new();
    if let Some(config) = container.config {
        if let Some(env_vars) = config.env {
            for env in env_vars {
                if let Some(domain_value) = env.strip_prefix("DOMAIN_NAME=") {
                    let replaced_domain = expand_template(domain_value, &vars);
                    domain_names.extend(
                        replaced_domain
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty()),
                    );
                }
            }
        }

        // Extract dev.orbstack.domains label
        if let Some(labels) = config.labels {
            if let Some(orbstack_domains) = labels.get("dev.orbstack.domains") {
                let replaced_domains = expand_template(orbstack_domains, &vars);
                domain_names.extend(
                    replaced_domains
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                );
            }
        }
    }

    // Docker's API used to expose the default bridge endpoint's address as a
    // top-level `IPAddress`; that field is gone, so read it from the source.
    let ip_address = networks
        .get(DEFAULT_BRIDGE_NETWORK)
        .map(|bridge| bridge.ip_address.clone());

    Some(ContainerInfo {
        id,
        name,
        ip_address,
        networks,
        domain_names,
        running,
    })
}

/// Pulls the container ID and network name out of a network event's actor.
fn network_event_targets(actor: Option<&EventActor>) -> Option<(String, String)> {
    let attrs = actor?.attributes.as_ref()?;
    let container_id = attrs.get("container")?.clone();
    let network_name = attrs.get("name")?.clone();
    if container_id.is_empty() || network_name.is_empty() {
        return None;
    }
    Some((container_id, network_name))
}

/// The event subscription: container and network events, replayed from `since`.
fn events_options(since: Option<u64>) -> EventsOptions {
    let mut filters = HashMap::new();
    filters.insert(
        "type".to_string(),
        vec!["container".to_string(), "network".to_string()],
    );

    EventsOptions {
        since: since.map(|secs| secs.to_string()),
        filters: Some(filters),
        ..Default::default()
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests should fail loudly"
)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_write_hosts_file_creates_managed_section() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();

        // Write initial content
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 100);

        // Add a test container
        sync.active_containers.lock().await.insert(
            "test123".to_string(),
            ContainerInfo {
                id: "test123".to_string(),
                name: "nginx".to_string(),
                ip_address: Some("172.17.0.2".to_string()),
                networks: BTreeMap::new(),
                domain_names: vec![],
                running: true,
            },
        );

        sync.write_hosts_file_immediate().await.unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains(START_TAG));
        assert!(content.contains(END_TAG));
        assert!(content.contains("127.0.0.1 localhost"));
        assert!(content.contains("172.17.0.2 nginx.docker"));
    }

    #[tokio::test]
    async fn test_write_hosts_file_updates_existing_section() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();

        // Write initial content with existing managed section
        fs::write(
            &path,
            format!(
                "127.0.0.1 localhost\n{START_TAG}\n172.17.0.2 old.container\n{END_TAG}\n192.168.1.1 server\n"
            ),
        )
        .unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 100);

        // Add a test container
        let mut networks = BTreeMap::new();
        networks.insert(
            "testnet".to_string(),
            NetworkInfo {
                ip_address: "172.18.0.2".to_string(),
                aliases: vec!["web".to_string()],
            },
        );

        sync.active_containers.lock().await.insert(
            "test123".to_string(),
            ContainerInfo {
                id: "test123".to_string(),
                name: "web".to_string(),
                ip_address: None,
                networks,
                domain_names: vec![],
                running: true,
            },
        );

        sync.write_hosts_file_immediate().await.unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("127.0.0.1 localhost"));
        assert!(content.contains("192.168.1.1 server"));
        assert!(content.contains("172.18.0.2 web.testnet"));
        assert!(!content.contains("172.17.0.2 old.container"));
    }

    #[tokio::test]
    async fn test_write_hosts_file_order_is_stable_for_shared_ip() {
        // On the default bridge, a container's global IP equals its network IP,
        // so several lines share one IP and only map ordering decides between them.
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 100);

        let networks = ["foxtrot", "echo", "delta", "charlie", "bravo", "alpha"]
            .into_iter()
            .map(|net| {
                (
                    net.to_string(),
                    NetworkInfo {
                        ip_address: "172.17.0.2".to_string(),
                        aliases: vec![],
                    },
                )
            })
            .collect();
        sync.active_containers.lock().await.insert(
            "test123".to_string(),
            ContainerInfo {
                id: "test123".to_string(),
                name: "web".to_string(),
                ip_address: Some("172.17.0.2".to_string()),
                networks,
                domain_names: vec![],
                running: true,
            },
        );

        sync.write_hosts_file_immediate().await.unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(
            content,
            format!(
                "127.0.0.1 localhost\n\n{START_TAG}\n\
                 172.17.0.2 web.docker\n\
                 172.17.0.2 web.alpha\n\
                 172.17.0.2 web.bravo\n\
                 172.17.0.2 web.charlie\n\
                 172.17.0.2 web.delta\n\
                 172.17.0.2 web.echo\n\
                 172.17.0.2 web.foxtrot\n\
                 {END_TAG}\n"
            ),
            "lines sharing an IP must come out in a stable order"
        );
    }

    /// Seeds one container on `net` (172.18.0.2) with the given domain names and
    /// returns the hosts file contents after a write.
    async fn write_with_domain_names(domain_names: &[&str]) -> String {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 100);

        let mut networks = BTreeMap::new();
        networks.insert(
            "net".to_string(),
            NetworkInfo {
                ip_address: "172.18.0.2".to_string(),
                aliases: vec![],
            },
        );
        sync.active_containers.lock().await.insert(
            "c1".to_string(),
            ContainerInfo {
                id: "c1".to_string(),
                name: "web".to_string(),
                ip_address: None,
                networks,
                domain_names: domain_names.iter().map(ToString::to_string).collect(),
                running: true,
            },
        );

        sync.write_hosts_file_immediate().await.unwrap();
        fs::read_to_string(&path).unwrap()
    }

    #[tokio::test]
    async fn test_write_hosts_file_rejects_hostname_with_newline() {
        let content = write_with_domain_names(&["ok.test", "evil.test\n6.6.6.6 github.com"]).await;

        assert!(
            content.lines().all(|line| !line.contains("github.com")),
            "a domain name must not be able to inject its own hosts line: {content}"
        );
        assert!(
            content.contains("172.18.0.2 web.net ok.test\n"),
            "valid names on the same container are still written: {content}"
        );
    }

    #[tokio::test]
    async fn test_write_hosts_file_rejects_hostname_with_whitespace_or_comment() {
        let content = write_with_domain_names(&["ok.test", "a.test b.test", "c.test#x"]).await;

        assert!(
            content.contains("172.18.0.2 web.net ok.test\n"),
            "only the valid name may be written: {content}"
        );
        assert!(!content.contains("b.test"), "{content}");
        assert!(!content.contains("c.test"), "{content}");
    }

    #[tokio::test]
    async fn test_write_hosts_file_skips_unchanged_content() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 100);
        seed_container(&sync, "c1", "web", "172.18.0.2").await;

        assert_eq!(
            sync.write_hosts_file_immediate().await.unwrap(),
            WriteOutcome::Written
        );
        let first = fs::read_to_string(&path).unwrap();

        assert_eq!(
            sync.write_hosts_file_immediate().await.unwrap(),
            WriteOutcome::Unchanged,
            "nothing changed, so the hosts file must not be rewritten"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), first);
    }

    #[tokio::test]
    async fn test_write_hosts_file_sorts_ips_numerically() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 100);
        seed_container(&sync, "c1", "ten", "172.17.0.10").await;
        seed_container(&sync, "c2", "two", "172.17.0.2").await;
        seed_container(&sync, "c3", "nine", "172.17.0.9").await;

        sync.write_hosts_file_immediate().await.unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let ips: Vec<&str> = content
            .lines()
            .filter(|line| line.starts_with("172."))
            .filter_map(|line| line.split_whitespace().next())
            .collect();
        assert_eq!(ips, ["172.17.0.2", "172.17.0.9", "172.17.0.10"]);
    }

    #[tokio::test]
    async fn test_consume_events_fails_when_docker_stream_ends() {
        let temp_file = NamedTempFile::new().unwrap();
        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(
            docker,
            temp_file.path().to_path_buf(),
            ".docker".to_string(),
            true,
            100,
        );

        let result = sync
            .consume_events(tokio_stream::iter(Vec::<
                Result<EventMessage, bollard::errors::Error>,
            >::new()))
            .await;

        assert!(
            result.is_err(),
            "a closed event stream means events are being missed; exiting cleanly would hide that"
        );
    }

    /// A running container `name` attached to `network` at `ip`, as Docker
    /// would report it on inspection.
    fn running_container(
        id: &str,
        name: &str,
        network: &str,
        ip: &str,
    ) -> ContainerInspectResponse {
        running_container_on(id, name, &[(network, ip)])
    }

    /// A running container `name` attached to each `(network, ip)` pair.
    fn running_container_on(
        id: &str,
        name: &str,
        networks: &[(&str, &str)],
    ) -> ContainerInspectResponse {
        ContainerInspectResponse {
            id: Some(id.to_string()),
            name: Some(format!("/{name}")),
            state: Some(bollard::models::ContainerState {
                running: Some(true),
                started_at: Some("2026-01-01T00:00:00Z".to_string()),
                ..Default::default()
            }),
            network_settings: Some(bollard::models::NetworkSettings {
                networks: Some(
                    networks
                        .iter()
                        .map(|(network, ip)| {
                            (
                                (*network).to_string(),
                                bollard::models::EndpointSettings {
                                    ip_address: Some((*ip).to_string()),
                                    ..Default::default()
                                },
                            )
                        })
                        .collect(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn network_event(action: &str, container_id: &str, network: &str) -> EventMessage {
        EventMessage {
            typ: Some(EventMessageTypeEnum::NETWORK),
            action: Some(action.to_string()),
            actor: Some(EventActor {
                id: Some(format!("{network}-id")),
                attributes: Some(HashMap::from([
                    ("container".to_string(), container_id.to_string()),
                    ("name".to_string(), network.to_string()),
                ])),
            }),
            ..Default::default()
        }
    }

    fn container_event(action: &str, id: &str) -> EventMessage {
        EventMessage {
            typ: Some(EventMessageTypeEnum::CONTAINER),
            action: Some(action.to_string()),
            actor: Some(EventActor {
                id: Some(id.to_string()),
                attributes: None,
            }),
            ..Default::default()
        }
    }

    /// Runs the event loop long enough to deliver queued events and flush the
    /// debounced write. The fake stream stays open, so the loop never ends itself.
    async fn listen_briefly(sync: &Synchronizer) {
        let outcome = tokio::time::timeout(Duration::from_millis(200), sync.listen_events()).await;
        assert!(
            outcome.is_err(),
            "the event loop ended on its own: {outcome:?}"
        );
    }

    fn hosts_file_with_localhost() -> NamedTempFile {
        let temp_file = NamedTempFile::new().unwrap();
        fs::write(temp_file.path(), "127.0.0.1 localhost\n").unwrap();
        temp_file
    }

    #[tokio::test]
    async fn test_synchronize_writes_running_containers() {
        let hosts = hosts_file_with_localhost();
        let (sync, docker) =
            Synchronizer::new_fake(hosts.path().to_path_buf(), ".docker".to_string(), true, 10);
        docker.start(running_container("c1", "web", "app", "172.18.0.2"));

        sync.synchronize().await.unwrap();

        let content = fs::read_to_string(hosts.path()).unwrap();
        assert!(content.contains("172.18.0.2 web.app\n"), "{content}");
    }

    #[tokio::test]
    async fn test_listen_events_replays_from_sync_start() {
        let hosts = hosts_file_with_localhost();
        let (sync, docker) =
            Synchronizer::new_fake(hosts.path().to_path_buf(), ".docker".to_string(), true, 10);
        let before_sync = now_secs();

        sync.synchronize().await.unwrap();
        listen_briefly(&sync).await;

        let subscriptions = docker.subscriptions();
        let since: u64 = subscriptions
            .first()
            .and_then(|options| options.since.as_deref())
            .expect("the subscription must replay events missed since the sync began")
            .parse()
            .unwrap();
        assert!(
            (before_sync..=before_sync + 5).contains(&since),
            "replay should start when the sync did ({before_sync}), got {since}"
        );
    }

    #[tokio::test]
    async fn test_container_start_event_adds_hosts_entry() {
        let hosts = hosts_file_with_localhost();
        let (sync, docker) =
            Synchronizer::new_fake(hosts.path().to_path_buf(), ".docker".to_string(), true, 10);
        sync.synchronize().await.unwrap();

        docker.start(running_container("c1", "web", "app", "172.18.0.2"));
        docker.emit(container_event("start", "c1"));
        listen_briefly(&sync).await;

        let content = fs::read_to_string(hosts.path()).unwrap();
        assert!(content.contains("172.18.0.2 web.app\n"), "{content}");
    }

    #[tokio::test]
    async fn test_container_die_event_removes_hosts_entry() {
        let hosts = hosts_file_with_localhost();
        let (sync, docker) =
            Synchronizer::new_fake(hosts.path().to_path_buf(), ".docker".to_string(), true, 10);
        docker.start(running_container("c1", "web", "app", "172.18.0.2"));
        sync.synchronize().await.unwrap();

        docker.remove("c1");
        docker.emit(container_event("die", "c1"));
        listen_briefly(&sync).await;

        let content = fs::read_to_string(hosts.path()).unwrap();
        assert!(!content.contains("web.app"), "{content}");
    }

    #[tokio::test]
    async fn test_network_connect_event_adds_hosts_entry() {
        let hosts = hosts_file_with_localhost();
        let (sync, docker) =
            Synchronizer::new_fake(hosts.path().to_path_buf(), ".docker".to_string(), true, 10);
        docker.start(running_container("c1", "web", "app", "172.18.0.2"));
        sync.synchronize().await.unwrap();

        docker.start(running_container_on(
            "c1",
            "web",
            &[("app", "172.18.0.2"), ("extra", "172.19.0.2")],
        ));
        docker.emit(network_event("connect", "c1", "extra"));
        listen_briefly(&sync).await;

        let content = fs::read_to_string(hosts.path()).unwrap();
        assert!(content.contains("172.19.0.2 web.extra\n"), "{content}");
        assert!(content.contains("172.18.0.2 web.app\n"), "{content}");
    }

    #[tokio::test]
    async fn test_network_disconnect_event_removes_only_that_network() {
        let hosts = hosts_file_with_localhost();
        let (sync, docker) =
            Synchronizer::new_fake(hosts.path().to_path_buf(), ".docker".to_string(), true, 10);
        docker.start(running_container_on(
            "c1",
            "web",
            &[("app", "172.18.0.2"), ("extra", "172.19.0.2")],
        ));
        sync.synchronize().await.unwrap();

        docker.start(running_container("c1", "web", "app", "172.18.0.2"));
        docker.emit(network_event("disconnect", "c1", "extra"));
        listen_briefly(&sync).await;

        let content = fs::read_to_string(hosts.path()).unwrap();
        assert!(!content.contains("web.extra"), "{content}");
        assert!(
            content.contains("172.18.0.2 web.app\n"),
            "the container is still on its other network: {content}"
        );
    }

    #[tokio::test]
    async fn test_health_reflects_the_synchronizers_writes() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-hosts-file");
        let (sync, _docker) = Synchronizer::new_fake(missing, ".docker".to_string(), true, 10);

        sync.synchronize().await.unwrap();

        assert!(
            !sync.health().write_ok(),
            "the health socket must see the synchronizer's failed write"
        );
    }

    #[test]
    fn test_first_dropped_line_finds_missing_user_line() {
        let rendered = vec!["127.0.0.1 localhost".to_string()];

        assert_eq!(
            first_dropped_line(&["127.0.0.1 localhost", "10.0.0.1 nas"], &rendered),
            Some("10.0.0.1 nas")
        );
        assert_eq!(
            first_dropped_line(&["127.0.0.1 localhost"], &rendered),
            None
        );
    }

    #[tokio::test]
    async fn test_write_hosts_file_with_end_tag_before_start_tag_is_stable() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        let original = format!("127.0.0.1 localhost\n{END_TAG}\n10.0.0.1 nas\n{START_TAG}\n");
        fs::write(&path, &original).unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 100);
        seed_container(&sync, "c1", "web", "172.17.0.2").await;

        sync.write_hosts_file_immediate().await.unwrap();
        let first = fs::read_to_string(&path).unwrap();
        assert!(first.contains("127.0.0.1 localhost\n"), "{first}");
        assert!(first.contains("10.0.0.1 nas\n"), "{first}");
        assert!(first.contains("172.17.0.2 web.docker\n"), "{first}");
        assert_eq!(
            first.matches(START_TAG).count(),
            1,
            "stray markers must not survive next to the real section: {first}"
        );

        assert_eq!(
            sync.write_hosts_file_immediate().await.unwrap(),
            WriteOutcome::Unchanged,
            "a misordered file must not grow a new section on every write"
        );
    }

    #[tokio::test]
    async fn test_write_hosts_file_skips_name_with_lone_brace() {
        let content = write_with_domain_names(&["ok.test", "half{open.test"]).await;

        let hostnames = content
            .lines()
            .find(|line| line.starts_with("172.18.0.2 "))
            .and_then(|line| line.split('#').next())
            .unwrap_or_default();
        assert_eq!(hostnames.trim(), "172.18.0.2 web.net ok.test", "{content}");
    }

    #[test]
    fn test_expand_template_keeps_empty_braces() {
        assert_eq!(expand_template("a{}b", &HashMap::new()), "a{}b");
    }

    #[test]
    fn test_expand_template_leaves_unclosed_variable_alone() {
        let vars = HashMap::from([("FOO".to_string(), "x".to_string())]);

        assert_eq!(expand_template("a{FOO", &vars), "a{FOO");
    }

    #[test]
    fn test_network_event_targets_requires_container_id() {
        let event = network_event("connect", "", "extra");

        assert_eq!(network_event_targets(event.actor.as_ref()), None);
    }

    #[test]
    fn test_extract_container_info_skips_stopped_container_without_ports() {
        let mut container = running_container("c1", "web", "app", "172.18.0.2");
        container.state = Some(bollard::models::ContainerState {
            running: Some(false),
            ..Default::default()
        });
        if let Some(settings) = container.network_settings.as_mut() {
            settings.ports = Some(HashMap::new());
        }

        assert!(extract_container_info(container).is_none());
    }

    #[test]
    fn test_extract_container_info_keeps_stopped_container_with_ports() {
        let mut container = running_container("c1", "web", "app", "172.18.0.2");
        container.state = Some(bollard::models::ContainerState {
            running: Some(false),
            ..Default::default()
        });
        if let Some(settings) = container.network_settings.as_mut() {
            settings.ports = Some(HashMap::from([("80/tcp".to_string(), None)]));
        }

        assert!(extract_container_info(container).is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_reserve_space_allocates_the_requested_bytes() {
        let file = NamedTempFile::new().unwrap();

        assert!(reserve_space(file.as_file(), 4096).unwrap());
        assert_eq!(file.as_file().metadata().unwrap().len(), 4096);
    }

    #[tokio::test]
    async fn test_write_reports_removed_section_when_no_entries_remain() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        fs::write(
            &path,
            format!("127.0.0.1 localhost\n{START_TAG}\n172.18.0.2 old.net\n{END_TAG}\n"),
        )
        .unwrap();
        // One container, but its only extra name is invalid and its network-less
        // state yields no entries: the section goes away entirely.
        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 100);
        sync.active_containers.lock().await.insert(
            "c1".to_string(),
            ContainerInfo {
                id: "c1".to_string(),
                name: "web".to_string(),
                ip_address: None,
                networks: BTreeMap::new(),
                domain_names: vec!["bad name".to_string()],
                running: true,
            },
        );

        assert_eq!(
            sync.write_hosts_file_immediate().await.unwrap(),
            WriteOutcome::Removed,
            "a container without entries must not count as an updated section"
        );
        assert!(!fs::read_to_string(&path).unwrap().contains(START_TAG));

        assert!(
            write_summary(false, 1, 0).contains("Removed empty managed section"),
            "{}",
            write_summary(false, 1, 0)
        );
        assert!(write_summary(true, 2, 3).contains("Updated hosts file"));
    }

    #[test]
    fn test_short_id_abbreviates_like_docker_ps() {
        assert_eq!(short_id("0123456789abcdef0123456789abcdef"), "0123456789ab");
        assert_eq!(short_id("abc"), "abc", "short IDs are shown whole");
    }

    #[tokio::test]
    async fn test_listen_events_subscribes_only_to_handled_event_types() {
        let hosts = hosts_file_with_localhost();
        let (sync, docker) =
            Synchronizer::new_fake(hosts.path().to_path_buf(), ".docker".to_string(), true, 10);

        listen_briefly(&sync).await;

        // Unfiltered, the daemon would stream every image, volume and plugin
        // event to us, only for `handle_event` to discard them.
        let subscriptions = docker.subscriptions();
        let filters = subscriptions
            .first()
            .and_then(|options| options.filters.as_ref())
            .expect("the subscription must be filtered");
        assert_eq!(
            filters.get("type"),
            Some(&vec!["container".to_string(), "network".to_string()])
        );
    }

    #[tokio::test]
    async fn test_write_hosts_file_dry_run_mode() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();

        // Write initial content
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), false, 100);

        sync.write_hosts_file_immediate().await.unwrap();

        // In dry-run mode, file should not be modified
        let content = fs::read_to_string(&path).unwrap();
        assert!(!content.contains(START_TAG));
        assert!(!content.contains(END_TAG));
        assert_eq!(content, "127.0.0.1 localhost\n");
    }

    #[tokio::test]
    async fn test_write_hosts_file_removes_empty_section() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();

        // Write initial content with existing managed section
        fs::write(
            &path,
            format!(
                "127.0.0.1 localhost\n{START_TAG}\n172.17.0.2 old.container\n{END_TAG}\n192.168.1.1 server\n"
            ),
        )
        .unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 100);

        // Don't add any containers - active_containers is empty
        sync.write_hosts_file_immediate().await.unwrap();

        let content = fs::read_to_string(&path).unwrap();
        // Should preserve other entries
        assert!(content.contains("127.0.0.1 localhost"));
        assert!(content.contains("192.168.1.1 server"));
        // Should remove managed section including tags
        assert!(!content.contains(START_TAG));
        assert!(!content.contains(END_TAG));
        assert!(!content.contains("172.17.0.2 old.container"));
    }

    #[tokio::test]
    async fn test_write_hosts_file_appends_when_no_section() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();

        // Write initial content without managed section
        fs::write(&path, "127.0.0.1 localhost\n192.168.1.1 server\n").unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 100);

        // Add a test container
        let mut networks = BTreeMap::new();
        networks.insert(
            "testnet".to_string(),
            NetworkInfo {
                ip_address: "172.18.0.2".to_string(),
                aliases: vec!["web".to_string()],
            },
        );

        sync.active_containers.lock().await.insert(
            "test123".to_string(),
            ContainerInfo {
                id: "test123".to_string(),
                name: "web".to_string(),
                ip_address: None,
                networks,
                domain_names: vec![],
                running: true,
            },
        );

        sync.write_hosts_file_immediate().await.unwrap();

        let content = fs::read_to_string(&path).unwrap();
        // Should preserve original entries
        assert!(content.contains("127.0.0.1 localhost"));
        assert!(content.contains("192.168.1.1 server"));
        // Should append managed section
        assert!(content.contains(START_TAG));
        assert!(content.contains(END_TAG));
        assert!(content.contains("172.18.0.2 web.testnet"));

        // Verify order: original entries come before managed section
        let start_pos = content.find(START_TAG).unwrap();
        let localhost_pos = content.find("127.0.0.1 localhost").unwrap();
        assert!(localhost_pos < start_pos);
    }

    #[test]
    fn test_extract_container_info() {
        // Mock a container response
        let container = ContainerInspectResponse {
            id: Some("abc123".to_string()),
            name: Some("/nginx".to_string()),
            state: Some(bollard::models::ContainerState {
                running: Some(true),
                ..Default::default()
            }),
            config: Some(bollard::models::ContainerConfig {
                env: Some(vec![
                    "PATH=/usr/bin".to_string(),
                    "DOMAIN_NAME=example.com,www.example.com".to_string(),
                ]),
                ..Default::default()
            }),
            network_settings: Some(bollard::models::NetworkSettings {
                ports: Some(HashMap::new()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = extract_container_info(container);
        assert!(info.is_some());

        let container_info = info.unwrap();
        assert_eq!(container_info.name, "nginx");
        assert_eq!(container_info.ip_address, None);
        assert!(container_info
            .domain_names
            .contains(&"example.com".to_string()));
        assert!(container_info
            .domain_names
            .contains(&"www.example.com".to_string()));
    }

    fn container_on_networks(networks: &[(&str, &str)]) -> ContainerInspectResponse {
        ContainerInspectResponse {
            id: Some("abc123".to_string()),
            name: Some("/nginx".to_string()),
            state: Some(bollard::models::ContainerState {
                running: Some(true),
                ..Default::default()
            }),
            network_settings: Some(bollard::models::NetworkSettings {
                networks: Some(
                    networks
                        .iter()
                        .map(|(name, ip)| {
                            (
                                (*name).to_string(),
                                bollard::models::EndpointSettings {
                                    ip_address: Some((*ip).to_string()),
                                    ..Default::default()
                                },
                            )
                        })
                        .collect(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn test_extract_container_info_uses_default_bridge_ip_as_global_ip() {
        let info = extract_container_info(container_on_networks(&[("bridge", "172.17.0.2")]))
            .expect("container should be extracted");

        assert_eq!(info.ip_address.as_deref(), Some("172.17.0.2"));
        assert_eq!(
            info.get_hostnames(".docker").first(),
            Some(&("172.17.0.2".to_string(), vec!["nginx.docker".to_string()])),
            "a bridge container gets the documented {{name}}{{tld}} hostname"
        );
    }

    #[test]
    fn test_extract_container_info_has_no_global_ip_off_the_default_bridge() {
        let info =
            extract_container_info(container_on_networks(&[("myapp_default", "172.18.0.2")]))
                .expect("container should be extracted");

        assert_eq!(info.ip_address, None);
    }

    #[test]
    fn test_extract_container_info_with_orbstack_label() {
        let mut labels = HashMap::new();
        labels.insert(
            "dev.orbstack.domains".to_string(),
            "foo.local,bar.local".to_string(),
        );

        let container = ContainerInspectResponse {
            id: Some("xyz456".to_string()),
            name: Some("/web".to_string()),
            state: Some(bollard::models::ContainerState {
                running: Some(true),
                ..Default::default()
            }),
            config: Some(bollard::models::ContainerConfig {
                labels: Some(labels),
                ..Default::default()
            }),
            network_settings: Some(bollard::models::NetworkSettings {
                ports: Some(HashMap::new()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = extract_container_info(container);
        assert!(info.is_some());

        let container_info = info.unwrap();
        assert_eq!(container_info.name, "web");
        assert!(container_info
            .domain_names
            .contains(&"foo.local".to_string()));
        assert!(container_info
            .domain_names
            .contains(&"bar.local".to_string()));
    }

    #[test]
    fn test_extract_container_info_with_both_env_and_label() {
        let mut labels = HashMap::new();
        labels.insert(
            "dev.orbstack.domains".to_string(),
            "app.example.org".to_string(),
        );

        let container = ContainerInspectResponse {
            id: Some("multi789".to_string()),
            name: Some("/app".to_string()),
            state: Some(bollard::models::ContainerState {
                running: Some(true),
                ..Default::default()
            }),
            config: Some(bollard::models::ContainerConfig {
                env: Some(vec!["DOMAIN_NAME=legacy.com".to_string()]),
                labels: Some(labels),
                ..Default::default()
            }),
            network_settings: Some(bollard::models::NetworkSettings {
                ports: Some(HashMap::new()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = extract_container_info(container);
        assert!(info.is_some());

        let container_info = info.unwrap();
        assert_eq!(container_info.name, "app");
        assert!(container_info
            .domain_names
            .contains(&"legacy.com".to_string()));
        assert!(container_info
            .domain_names
            .contains(&"app.example.org".to_string()));
    }

    #[test]
    fn test_extract_container_info_with_env_template() {
        let container = ContainerInspectResponse {
            id: Some("envtmpl123".to_string()),
            name: Some("/webapp".to_string()),
            state: Some(bollard::models::ContainerState {
                running: Some(true),
                ..Default::default()
            }),
            config: Some(bollard::models::ContainerConfig {
                env: Some(vec![
                    "MY_VAR=custom".to_string(),
                    "DOMAIN_NAME={MY_VAR}-app.example.com,{MISSING}-domain.local".to_string(),
                ]),
                ..Default::default()
            }),
            network_settings: Some(bollard::models::NetworkSettings {
                ports: Some(HashMap::new()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = extract_container_info(container);
        assert!(info.is_some());
        let container_info = info.unwrap();

        // Ensure successful substitution
        assert!(container_info
            .domain_names
            .contains(&"custom-app.example.com".to_string()));
        // Ensure missing variables are left unmodified
        assert!(container_info
            .domain_names
            .contains(&"{MISSING}-domain.local".to_string()));
    }

    #[test]
    fn test_extract_container_info_with_compose_template() {
        let mut labels = HashMap::new();
        labels.insert(
            "com.docker.compose.project".to_string(),
            "myproject".to_string(),
        );

        let container = ContainerInspectResponse {
            id: Some("compose123".to_string()),
            name: Some("/myproject-web-1".to_string()),
            state: Some(bollard::models::ContainerState {
                running: Some(true),
                ..Default::default()
            }),
            config: Some(bollard::models::ContainerConfig {
                env: Some(vec![
                    "DOMAIN_NAME={COMPOSE_PROJECT_NAME}.example.com".to_string()
                ]),
                labels: Some(labels),
                ..Default::default()
            }),
            network_settings: Some(bollard::models::NetworkSettings {
                ports: Some(HashMap::new()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = extract_container_info(container);
        assert!(info.is_some());
        let container_info = info.unwrap();
        assert!(container_info
            .domain_names
            .contains(&"myproject.example.com".to_string()));
    }

    #[test]
    fn test_extract_container_info_with_compose_template_fallback() {
        let container = ContainerInspectResponse {
            id: Some("fallback123".to_string()),
            name: Some("/myworktree-web-1".to_string()),
            state: Some(bollard::models::ContainerState {
                running: Some(true),
                ..Default::default()
            }),
            config: Some(bollard::models::ContainerConfig {
                env: Some(vec![
                    "DOMAIN_NAME={COMPOSE_PROJECT_NAME}.example.com".to_string()
                ]),
                ..Default::default()
            }),
            network_settings: Some(bollard::models::NetworkSettings {
                ports: Some(HashMap::new()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = extract_container_info(container);
        assert!(info.is_some());
        let container_info = info.unwrap();
        // Fallback should extract "myworktree" from "/myworktree-web-1"
        assert!(container_info
            .domain_names
            .contains(&"myworktree.example.com".to_string()));
    }

    // ── debounce behaviour ────────────────────────────────────────────

    /// Helper: insert a single container with the given name and IP into
    /// `sync.active_containers`.
    async fn seed_container(sync: &Synchronizer, id: &str, name: &str, ip: &str) {
        let mut active = sync.active_containers.lock().await;
        active.insert(
            id.to_string(),
            ContainerInfo {
                id: id.to_string(),
                name: name.to_string(),
                ip_address: Some(ip.to_string()),
                networks: BTreeMap::new(),
                domain_names: vec![],
                running: true,
            },
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_debounce_delays_write() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        // 100 ms debounce window
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 100);
        seed_container(&sync, "c1", "nginx", "172.17.0.2").await;

        tokio::select! {
            result = sync.process_pending_writes() => { result.unwrap(); }
            () = async {
                sync.schedule_write();

                // 30 ms in — well before the 100 ms window; file untouched
                sleep(Duration::from_millis(30)).await;
                let content = fs::read_to_string(&path).unwrap();
                assert!(
                    !content.contains(START_TAG),
                    "file written before debounce window expired"
                );

                // 150 ms in — well after the window; file written
                sleep(Duration::from_millis(120)).await;
                let content = fs::read_to_string(&path).unwrap();
                assert!(
                    content.contains("172.17.0.2 nginx.docker"),
                    "file not written after debounce window"
                );
            } => {}
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_debounce_resets_on_new_event() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        // 100 ms debounce window
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 100);
        seed_container(&sync, "c1", "nginx", "172.17.0.2").await;

        tokio::select! {
            result = sync.process_pending_writes() => { result.unwrap(); }
            () = async {
                // t = 0   first event
                sync.schedule_write();

                // t = 60  still inside the first 100 ms window
                sleep(Duration::from_millis(60)).await;
                let content = fs::read_to_string(&path).unwrap();
                assert!(
                    !content.contains(START_TAG),
                    "file written before first debounce expired"
                );

                // t = 60  second event — resets window to t = 160
                sync.schedule_write();

                // t = 120 only 60 ms since the reset, still < 100 ms
                sleep(Duration::from_millis(60)).await;
                let content = fs::read_to_string(&path).unwrap();
                assert!(
                    !content.contains(START_TAG),
                    "file written before reset debounce expired"
                );

                // t = 220 — 160 ms after the reset, well past 100 ms
                sleep(Duration::from_millis(100)).await;
                let content = fs::read_to_string(&path).unwrap();
                assert!(
                    content.contains("172.17.0.2 nginx.docker"),
                    "file not written after reset debounce expired"
                );
            } => {}
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_idle_after_write_no_spurious_updates() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        // 50 ms debounce — short, so the test runs fast
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 50);
        seed_container(&sync, "a", "nginx", "172.17.0.2").await;

        tokio::select! {
            result = sync.process_pending_writes() => { result.unwrap(); }
            () = async {
                // Trigger and wait for the first debounced write to land
                sync.schedule_write();
                sleep(Duration::from_millis(100)).await;

                let content = fs::read_to_string(&path).unwrap();
                assert!(content.contains("172.17.0.2 nginx.docker"));

                // Silently swap in a different container — no schedule_write
                sync.active_containers.lock().await.clear();
                seed_container(&sync, "b", "redis", "172.17.0.3").await;

                // Wait well past a full debounce window
                sleep(Duration::from_millis(100)).await;

                // File still reflects the first write; processor was idle
                let content = fs::read_to_string(&path).unwrap();
                assert!(
                    content.contains("172.17.0.2 nginx.docker"),
                    "old entry should still be present"
                );
                assert!(
                    !content.contains("172.17.0.3"),
                    "new container written without schedule_write"
                );
            } => {}
        }
    }

    // ── hostname conflict resolution ──────────────────────────────────────

    /// Helper: claim all hostnames for `container` then insert it into `active_containers`.
    async fn seed_container_claimed(sync: &Synchronizer, id: &str, container: ContainerInfo) {
        sync.claim_hostnames(id, &container).await;
        let mut active = sync.active_containers.lock().await;
        active.insert(id.to_string(), container);
    }

    #[tokio::test]
    async fn test_hostname_conflict_first_wins() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 100);

        // Container A starts first and claims myapp.local
        let container_a = ContainerInfo {
            id: "aaa".to_string(),
            name: "container-a".to_string(),
            ip_address: Some("172.17.0.2".to_string()),
            networks: BTreeMap::new(),
            domain_names: vec!["myapp.local".to_string()],
            running: true,
        };
        seed_container_claimed(&sync, "aaa", container_a).await;

        // Container B starts second and tries to claim the same hostname
        let container_b = ContainerInfo {
            id: "bbb".to_string(),
            name: "container-b".to_string(),
            ip_address: Some("172.17.0.3".to_string()),
            networks: BTreeMap::new(),
            domain_names: vec!["myapp.local".to_string()],
            running: true,
        };
        seed_container_claimed(&sync, "bbb", container_b).await;

        sync.write_hosts_file_immediate().await.unwrap();
        let content = fs::read_to_string(&path).unwrap();

        // A's line: myapp.local appears before any comment marker
        let a_line = content
            .lines()
            .find(|l| l.starts_with("172.17.0.2"))
            .expect("A's IP line missing");
        let a_hosts_part = a_line.split('#').next().unwrap_or("");
        assert!(
            a_hosts_part.contains("myapp.local"),
            "A should own myapp.local"
        );

        // B's line: myapp.local is in the skip comment, not in the hostname part
        let b_line = content
            .lines()
            .find(|l| l.starts_with("172.17.0.3"))
            .expect("B's IP line missing");
        let b_hosts_part = b_line.split('#').next().unwrap_or("");
        assert!(
            !b_hosts_part.contains("myapp.local"),
            "B should not have myapp.local as an active hostname"
        );
        assert!(
            b_line.contains("# skipped: myapp.local"),
            "B's line should contain the skip comment"
        );
    }

    #[tokio::test]
    async fn test_hostname_conflict_order_independent() {
        // Container B claims first even though container-a sorts alphabetically earlier.
        // B should still win because it claimed first.
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 100);

        let container_a = ContainerInfo {
            id: "aaa".to_string(),
            name: "container-a".to_string(),
            ip_address: Some("172.17.0.2".to_string()),
            networks: BTreeMap::new(),
            domain_names: vec!["shared.local".to_string()],
            running: true,
        };
        let container_b = ContainerInfo {
            id: "bbb".to_string(),
            name: "container-b".to_string(),
            ip_address: Some("172.17.0.3".to_string()),
            networks: BTreeMap::new(),
            domain_names: vec!["shared.local".to_string()],
            running: true,
        };

        // B claims first (even though A sorts alphabetically earlier)
        seed_container_claimed(&sync, "bbb", container_b).await;
        // A claims second — should be rejected despite alphabetical precedence
        seed_container_claimed(&sync, "aaa", container_a).await;

        sync.write_hosts_file_immediate().await.unwrap();
        let content = fs::read_to_string(&path).unwrap();

        let b_line = content
            .lines()
            .find(|l| l.starts_with("172.17.0.3"))
            .expect("B's IP line missing");
        let b_hosts_part = b_line.split('#').next().unwrap_or("");
        assert!(
            b_hosts_part.contains("shared.local"),
            "B should own shared.local (claimed first)"
        );

        let a_line = content
            .lines()
            .find(|l| l.starts_with("172.17.0.2"))
            .expect("A's IP line missing");
        assert!(
            a_line.contains("# skipped: shared.local"),
            "A's line should have a skip comment"
        );
    }

    #[tokio::test]
    async fn test_hostname_released_on_stop() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 100);

        let container_a = ContainerInfo {
            id: "aaa".to_string(),
            name: "container-a".to_string(),
            ip_address: Some("172.17.0.2".to_string()),
            networks: BTreeMap::new(),
            domain_names: vec!["myapp.local".to_string()],
            running: true,
        };
        seed_container_claimed(&sync, "aaa", container_a.clone()).await;

        // A stops — release its hostnames then remove from active
        sync.release_hostnames("aaa", &container_a, None).await;
        sync.active_containers.lock().await.remove("aaa");

        // B can now claim myapp.local
        let container_b = ContainerInfo {
            id: "bbb".to_string(),
            name: "container-b".to_string(),
            ip_address: Some("172.17.0.3".to_string()),
            networks: BTreeMap::new(),
            domain_names: vec!["myapp.local".to_string()],
            running: true,
        };
        seed_container_claimed(&sync, "bbb", container_b).await;

        sync.write_hosts_file_immediate().await.unwrap();
        let content = fs::read_to_string(&path).unwrap();

        let b_line = content
            .lines()
            .find(|l| l.starts_with("172.17.0.3"))
            .expect("B's IP line missing");
        let b_hosts_part = b_line.split('#').next().unwrap_or("");
        assert!(
            b_hosts_part.contains("myapp.local"),
            "B should own myapp.local after A released it"
        );
        assert!(
            !b_line.contains("# skipped"),
            "B's line should have no skip comment"
        );
        assert!(!content.contains("172.17.0.2"), "A's entry should be gone");
    }

    #[tokio::test]
    async fn test_dynamic_hostnames_no_false_conflicts() {
        // Two containers with the same alias in different networks produce
        // distinct hostnames (alias.network1 vs alias.network2) — no conflict.
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 100);

        let mut networks_a = BTreeMap::new();
        networks_a.insert(
            "frontend".to_string(),
            NetworkInfo {
                ip_address: "172.18.0.2".to_string(),
                aliases: vec!["web".to_string()],
            },
        );
        let mut networks_b = BTreeMap::new();
        networks_b.insert(
            "backend".to_string(),
            NetworkInfo {
                ip_address: "172.19.0.2".to_string(),
                aliases: vec!["web".to_string()],
            },
        );

        seed_container_claimed(
            &sync,
            "aaa",
            ContainerInfo {
                id: "aaa".to_string(),
                name: "web-a".to_string(),
                ip_address: None,
                networks: networks_a,
                domain_names: vec![],
                running: true,
            },
        )
        .await;
        seed_container_claimed(
            &sync,
            "bbb",
            ContainerInfo {
                id: "bbb".to_string(),
                name: "web-b".to_string(),
                ip_address: None,
                networks: networks_b,
                domain_names: vec![],
                running: true,
            },
        )
        .await;

        sync.write_hosts_file_immediate().await.unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert!(content.contains("172.18.0.2"), "A's IP should be present");
        assert!(content.contains("172.19.0.2"), "B's IP should be present");
        assert!(
            !content.contains("# skipped"),
            "No skip comments for distinct dynamic hostnames"
        );
    }

    #[tokio::test]
    async fn test_all_hostnames_skipped_writes_comment_only_line() {
        // When every hostname for an IP is claimed by another container,
        // only a comment line should appear (no bare IP entry).
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 100);

        // A wins "clash-app.docker" and "clash.local"
        seed_container_claimed(
            &sync,
            "aaa",
            ContainerInfo {
                id: "aaa".to_string(),
                name: "clash-app".to_string(),
                ip_address: Some("172.17.0.2".to_string()),
                networks: BTreeMap::new(),
                domain_names: vec!["clash.local".to_string()],
                running: true,
            },
        )
        .await;

        // B has the same name and same domain — both its hostnames are already claimed by A
        seed_container_claimed(
            &sync,
            "bbb",
            ContainerInfo {
                id: "bbb".to_string(),
                name: "clash-app".to_string(),
                ip_address: Some("172.17.0.99".to_string()),
                networks: BTreeMap::new(),
                domain_names: vec!["clash.local".to_string()],
                running: true,
            },
        )
        .await;

        sync.write_hosts_file_immediate().await.unwrap();
        let content = fs::read_to_string(&path).unwrap();

        // B's IP must not appear as a real hosts entry (no line starting with it)
        assert!(
            !content.lines().any(|l| l.starts_with("172.17.0.99")),
            "B's IP should not appear as a hostname entry"
        );
        // But it must appear in a comment line
        assert!(
            content.contains("172.17.0.99"),
            "B's IP should appear in a comment"
        );
        assert!(
            content.contains("all hostnames skipped"),
            "Comment should say all hostnames were skipped"
        );
    }

    #[tokio::test]
    async fn test_write_hosts_file_unresolved_variables_skipped() {
        // When a hostname contains unresolved templated variables like {MISSING},
        // it should be put into the skipped comment rather than written as a valid hostname.
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 100);

        seed_container_claimed(
            &sync,
            "ccc",
            ContainerInfo {
                id: "ccc".to_string(),
                name: "app".to_string(),
                ip_address: Some("172.18.0.5".to_string()),
                networks: BTreeMap::new(),
                domain_names: vec![
                    "{APP_SECRET_FILE}.app2.local".to_string(),
                    "valid.local".to_string(),
                ],
                running: true,
            },
        )
        .await;

        sync.write_hosts_file_immediate().await.unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert!(
            content.contains("172.18.0.5 app.docker valid.local"),
            "Valid hostnames should be written normally"
        );
        assert!(
            content.contains("# skipped: {APP_SECRET_FILE}.app2.local"),
            "Unresolved variables should be pushed to the # skipped comment"
        );
    }

    #[tokio::test]
    async fn test_write_hosts_file_sorts_by_ip() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true, 100);

        // Add containers with IPs in an unsorted order
        seed_container_claimed(
            &sync,
            "ccc",
            ContainerInfo {
                id: "ccc".to_string(),
                name: "c-app".to_string(),
                ip_address: Some("172.17.0.2".to_string()),
                networks: BTreeMap::new(),
                domain_names: vec![],
                running: true,
            },
        )
        .await;

        seed_container_claimed(
            &sync,
            "aaa",
            ContainerInfo {
                id: "aaa".to_string(),
                name: "a-app".to_string(),
                ip_address: Some("10.0.0.2".to_string()),
                networks: BTreeMap::new(),
                domain_names: vec![],
                running: true,
            },
        )
        .await;

        seed_container_claimed(
            &sync,
            "bbb",
            ContainerInfo {
                id: "bbb".to_string(),
                name: "b-app".to_string(),
                ip_address: Some("10.0.0.1".to_string()),
                networks: BTreeMap::new(),
                domain_names: vec![],
                running: true,
            },
        )
        .await;

        sync.write_hosts_file_immediate().await.unwrap();
        let content = fs::read_to_string(&path).unwrap();

        // Extract just the IPs from the managed section
        let mut ips_found = Vec::new();
        let mut in_section = false;
        for line in content.lines() {
            let t = line.trim();
            if t == START_TAG {
                in_section = true;
                continue;
            } else if t == END_TAG {
                in_section = false;
                continue;
            }
            if in_section {
                // Ignore empty lines
                if !t.is_empty() {
                    // Extract the IP part (first word)
                    if let Some(ip) = t.split_whitespace().next() {
                        ips_found.push(ip);
                    }
                }
            }
        }

        let expected_order = vec!["10.0.0.1", "10.0.0.2", "172.17.0.2"];
        assert_eq!(
            ips_found, expected_order,
            "IP strings should appear in sorted order"
        );
    }

    // ── partial release on network disconnect ─────────────────────────────

    /// Builds a `ContainerInfo` resembling the urq dev setup: a `web` container
    /// attached to a project-private `*_default` network (where it owns the
    /// project's local dev domain via a `default:hostname` env entry) and to a
    /// shared `public` network (used by an external proxy / tunnel).
    fn multi_network_web(
        id: &str,
        name: &str,
        default_net: &str,
        default_ip: &str,
        public_ip: &str,
        dev_domain: &str,
    ) -> ContainerInfo {
        let mut networks = BTreeMap::new();
        networks.insert(
            default_net.to_string(),
            NetworkInfo {
                ip_address: default_ip.to_string(),
                aliases: vec!["web".to_string()],
            },
        );
        networks.insert(
            "public".to_string(),
            NetworkInfo {
                ip_address: public_ip.to_string(),
                aliases: vec!["web".to_string()],
            },
        );
        ContainerInfo {
            id: id.to_string(),
            name: name.to_string(),
            ip_address: None,
            networks,
            domain_names: vec![format!("default:{dev_domain}")],
            running: true,
        }
    }

    #[tokio::test]
    async fn test_release_hostnames_with_network_filter_only_drops_that_networks_claims() {
        let temp_file = NamedTempFile::new().unwrap();
        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(
            docker,
            temp_file.path().to_path_buf(),
            ".docker".to_string(),
            true,
            100,
        );

        let main = multi_network_web(
            "main-id",
            "urq-web-1",
            "urq_default",
            "172.22.0.3",
            "172.21.0.3",
            "dkarlovi-dev.urq.app",
        );
        seed_container_claimed(&sync, "main-id", main.clone()).await;

        // Sanity: every hostname is claimed by main.
        {
            let claims: HashMap<String, (String, String)> =
                sync.hostname_claims.lock().await.clone();
            for h in ["dkarlovi-dev.urq.app", "web.urq_default", "web.public"] {
                assert_eq!(
                    claims.get(h).map(|(id, _)| id.as_str()),
                    Some("main-id"),
                    "precondition: {h} owned by main"
                );
            }
        }

        // Simulate a `disconnect` from the shared `public` network only.
        sync.release_hostnames("main-id", &main, Some("public"))
            .await;

        let claims: HashMap<String, (String, String)> = sync.hostname_claims.lock().await.clone();
        assert_eq!(
            claims.get("web.public").map(|(id, _)| id.as_str()),
            None,
            "the public-bound hostname is released"
        );
        assert_eq!(
            claims
                .get("dkarlovi-dev.urq.app")
                .map(|(id, _)| id.as_str()),
            Some("main-id"),
            "the dev domain stays with main — it lives on urq_default, not public"
        );
        assert_eq!(
            claims.get("web.urq_default").map(|(id, _)| id.as_str()),
            Some("main-id"),
            "the urq_default alias also stays"
        );
    }

    #[tokio::test]
    async fn test_release_hostnames_none_releases_every_claim() {
        let temp_file = NamedTempFile::new().unwrap();
        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(
            docker,
            temp_file.path().to_path_buf(),
            ".docker".to_string(),
            true,
            100,
        );

        let main = multi_network_web(
            "main-id",
            "urq-web-1",
            "urq_default",
            "172.22.0.3",
            "172.21.0.3",
            "dkarlovi-dev.urq.app",
        );
        seed_container_claimed(&sync, "main-id", main.clone()).await;

        sync.release_hostnames("main-id", &main, None).await;

        let claims: HashMap<String, (String, String)> = sync.hostname_claims.lock().await.clone();
        for h in ["dkarlovi-dev.urq.app", "web.urq_default", "web.public"] {
            assert!(
                !claims.contains_key(h),
                "full release should drop {h}, but it's still claimed"
            );
        }
    }

    /// Encodes the originally hypothesised race: while main is up and owns the
    /// dev domain on its `_default` network, a sibling worktree project comes
    /// up and disturbs the shared `public` network. Before the fix, a `public`
    /// disconnect was treated as a full container teardown and released every
    /// claim main held — including the dev domain on `_default`. A sibling
    /// whose env still listed the main dev domain could then take it. After
    /// the fix, the dev domain stays with main regardless of what happens on
    /// `public`.
    #[tokio::test]
    async fn test_network_disconnect_does_not_let_sibling_steal_other_networks_hostname() {
        let temp_file = NamedTempFile::new().unwrap();
        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(
            docker,
            temp_file.path().to_path_buf(),
            ".docker".to_string(),
            true,
            100,
        );

        let main = multi_network_web(
            "main-id",
            "urq-web-1",
            "urq_default",
            "172.22.0.3",
            "172.21.0.3",
            "dkarlovi-dev.urq.app",
        );
        let worktree = multi_network_web(
            "wt-id",
            "urq-fix-worktrees-web-1",
            "urq-fix-worktrees_default",
            "172.25.0.3",
            "172.21.0.4",
            "dkarlovi-dev.urq.app",
        );

        seed_container_claimed(&sync, "main-id", main.clone()).await;

        sync.release_hostnames("main-id", &main, Some("public"))
            .await;
        sync.claim_hostnames("wt-id", &worktree).await;

        let claims: HashMap<String, (String, String)> = sync.hostname_claims.lock().await.clone();
        assert_eq!(
            claims
                .get("dkarlovi-dev.urq.app")
                .map(|(id, _)| id.as_str()),
            Some("main-id"),
            "main must keep the dev domain through a `public` disconnect — \
             otherwise /etc/hosts would flip to the worktree"
        );
    }

    #[tokio::test]
    async fn test_network_event_targets_extracts_container_and_network_from_attributes() {
        let mut attrs = HashMap::new();
        attrs.insert("container".to_string(), "abc123".to_string());
        attrs.insert("name".to_string(), "public".to_string());
        attrs.insert("type".to_string(), "bridge".to_string());
        let actor = EventActor {
            id: Some("net-id".to_string()),
            attributes: Some(attrs),
        };
        assert_eq!(
            network_event_targets(Some(&actor)),
            Some(("abc123".to_string(), "public".to_string()))
        );

        let actor_missing = EventActor {
            id: Some("net-id".to_string()),
            attributes: Some(HashMap::new()),
        };
        assert_eq!(network_event_targets(Some(&actor_missing)), None);
        assert_eq!(network_event_targets(None), None);
    }

    // ---- regression tests for the 2026-08-07 incident ----------------------
    //
    // A transient ENOSPC on the host truncated the user's /etc/hosts to zero
    // bytes and killed the daemon. Three distinct defects were involved; each
    // gets a test below.

    #[test]
    fn test_probe_reservation_support_on_a_normal_file() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        let original = "127.0.0.1 localhost\n10.0.0.5 keep-me\n";
        fs::write(&path, original).unwrap();

        let support = probe_reservation_support(&path);
        if cfg!(target_os = "linux") {
            assert_eq!(
                support,
                ReservationSupport::Available,
                "a regular file on a normal filesystem should support reservation"
            );
        }

        // The probe must never disturb the file it is asked about.
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original,
            "probing changed the file's contents"
        );
    }

    /// The hosts file was left at zero bytes by the incident, so the empty case
    /// is not hypothetical: probing it must not make it non-empty.
    #[test]
    fn test_probe_reservation_support_leaves_an_empty_file_empty() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        fs::write(&path, "").unwrap();

        let support = probe_reservation_support(&path);
        if cfg!(target_os = "linux") {
            assert_eq!(support, ReservationSupport::Available);
        }

        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            0,
            "probing an empty hosts file must leave it empty"
        );
    }

    #[test]
    fn test_probe_reservation_support_reports_unwritable_target() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-hosts-file");

        let support = probe_reservation_support(&missing);
        assert!(
            matches!(support, ReservationSupport::NotWritable(_)),
            "a file that cannot be opened for writing must be reported as such, got {support:?}"
        );
    }

    /// A read-only target is a misconfiguration, not merely a weaker guarantee,
    /// so it must be distinguishable from an unsupported filesystem.
    #[test]
    fn test_probe_reservation_support_distinguishes_read_only_target() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let mut perms = fs::metadata(&path).unwrap().permissions();
        #[cfg(unix)]
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o444);
        #[cfg(not(unix))]
        perms.set_readonly(true);
        fs::set_permissions(&path, perms).unwrap();

        let support = probe_reservation_support(&path);

        // Restore permissions so the temp file can be cleaned up.
        let mut restore = fs::metadata(&path).unwrap().permissions();
        #[cfg(unix)]
        std::os::unix::fs::PermissionsExt::set_mode(&mut restore, 0o644);
        #[cfg(not(unix))]
        restore.set_readonly(false);
        fs::set_permissions(&path, restore).unwrap();

        assert!(
            matches!(support, ReservationSupport::NotWritable(_)),
            "a read-only hosts file must be reported as not writable, got {support:?}"
        );
    }

    fn test_container(id: &str, name: &str, ip: &str) -> ContainerInfo {
        ContainerInfo {
            id: id.to_string(),
            name: name.to_string(),
            ip_address: Some(ip.to_string()),
            networks: BTreeMap::new(),
            domain_names: vec![],
            running: true,
        }
    }

    fn new_sync(path: PathBuf, write_enabled: bool, debounce_ms: u64) -> Synchronizer {
        let docker = Docker::connect_with_socket_defaults().unwrap();
        Synchronizer::new(
            docker,
            path,
            ".docker".to_string(),
            write_enabled,
            debounce_ms,
        )
    }

    /// Defect 3: `handle_container_up` runs for both the `container start` and
    /// the `network connect` event, so `claim_hostnames` is called twice for the
    /// same container. The second pass must not warn that the container stole
    /// the hostname from itself.
    #[tokio::test]
    async fn test_reclaim_by_same_container_reports_no_conflict() {
        let sync = new_sync(PathBuf::from("/dev/null"), false, 100);
        let info = test_container("abc123", "lucid_heisenberg", "172.17.0.2");

        // container start
        let first = sync.claim_hostnames("abc123", &info).await;
        // network connect for the very same container
        let second = sync.claim_hostnames("abc123", &info).await;

        assert!(
            first.is_empty(),
            "the initial claim must not conflict, got: {first:?}"
        );
        assert!(
            second.is_empty(),
            "a container re-claiming its own hostname must not be reported as a conflict, got: {second:?}"
        );

        let owner = {
            let claims = sync.hostname_claims.lock().await;
            claims
                .get("lucid_heisenberg.docker")
                .map(|(id, _)| id.clone())
        };
        assert_eq!(
            owner.as_deref(),
            Some("abc123"),
            "the container must still own its hostname after re-claiming"
        );
    }

    /// Guards the fix above from over-correcting: a genuine conflict between two
    /// different containers must still be reported.
    #[tokio::test]
    async fn test_conflict_between_different_containers_is_reported() {
        let sync = new_sync(PathBuf::from("/dev/null"), false, 100);
        let first = test_container("aaa", "shared", "172.17.0.2");
        let second = test_container("bbb", "shared", "172.17.0.3");

        assert_eq!(sync.claim_hostnames("aaa", &first).await, []);
        let conflicts = sync.claim_hostnames("bbb", &second).await;

        assert_eq!(
            conflicts,
            vec![HostnameConflict {
                hostname: "shared.docker".to_string(),
                owner_name: "shared".to_string(),
            }],
            "a real conflict between two different containers must still be reported"
        );

        let owner = {
            let claims = sync.hostname_claims.lock().await;
            claims.get("shared.docker").map(|(id, _)| id.clone())
        };
        assert_eq!(
            owner.as_deref(),
            Some("aaa"),
            "the first claimant must keep ownership"
        );
    }

    /// Defect 2: a write failure propagated out of `process_pending_writes`,
    /// which aborted `listen_events` and exited `main`. One transient ENOSPC
    /// killed the daemon permanently. The loop must survive a failing write.
    #[tokio::test]
    async fn test_write_error_does_not_terminate_the_loop() {
        let dir = tempfile::tempdir().unwrap();
        // Path inside a directory that does not exist: every write attempt fails.
        let unwritable = dir.path().join("missing-dir").join("hosts");

        let sync = new_sync(unwritable, true, 20);
        seed_container(&sync, "abc123", "nginx", "172.17.0.2").await;

        sync.schedule_write();

        let outcome =
            tokio::time::timeout(Duration::from_millis(400), sync.process_pending_writes()).await;

        assert!(
            outcome.is_err(),
            "process_pending_writes must keep looping after a failed write, but it returned: {outcome:?}"
        );
    }

    /// Byte ceiling imposed on the child process below. Small enough that the
    /// rendered managed section cannot fit, large enough for the starting file.
    #[cfg(target_os = "linux")]
    const FSIZE_LIMIT: u64 = 4096;

    #[cfg(target_os = "linux")]
    const FSIZE_LIMIT_VAR: &str = "HOSTMANAGER_FSIZE_LIMIT";

    /// Caps the file size this process may create, so a large write fails partway
    /// exactly as it does on a full filesystem.
    ///
    /// `RLIMIT_FSIZE` is used rather than a real small filesystem because it needs
    /// no privileges: mounting even a rootless tmpfs requires user namespaces,
    /// which Ubuntu 23.10+ (and therefore GitHub Actions) blocks by default via
    /// `kernel.apparmor_restrict_unprivileged_userns`. The kernel path is the same
    /// one ENOSPC takes: `fallocate` refuses up front with `EFBIG` and leaves the
    /// file untouched, while a plain write truncates first and fails halfway.
    #[cfg(target_os = "linux")]
    #[expect(
        unsafe_code,
        reason = "signal(2) and setrlimit(2) have no safe wrappers in libc"
    )]
    fn apply_fsize_limit(limit: u64) {
        let rlimit = libc::rlimit {
            rlim_cur: limit,
            rlim_max: limit,
        };
        // Exceeding the limit also raises SIGXFSZ, which would kill the
        // process before the write could return an error to inspect.
        // SAFETY: takes plain scalars and affects only this process, which
        // exists solely to run this one test.
        unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) };
        // SAFETY: takes a scalar and a pointer to the local above, and affects
        // only this process, which exists solely to run this one test.
        unsafe {
            libc::setrlimit(libc::RLIMIT_FSIZE, &raw const rlimit);
        }
    }

    /// Re-runs a single test in a child process under the file size limit.
    ///
    /// The limit is process-wide, so it cannot be set in the test process itself
    /// without breaking every other test writing temporary files in parallel.
    #[cfg(target_os = "linux")]
    fn reexec_under_fsize_limit(test_name: &str) -> std::process::Output {
        let exe = std::env::current_exe().expect("cannot locate the test binary");
        std::process::Command::new(exe)
            .args(["--exact", test_name, "--nocapture"])
            .env(FSIZE_LIMIT_VAR, FSIZE_LIMIT.to_string())
            .output()
            .expect("failed to re-exec the test binary")
    }

    /// Defect 1, the one that cost the user their `/etc/hosts`.
    ///
    /// Linux-only: the guarantee under test comes from `fallocate(2)`, and the
    /// harness leans on `RLIMIT_FSIZE`. Both are Linux specifics.
    ///
    /// `fs::write` is `File::create` + `write_all`, i.e. `O_TRUNC` first and
    /// write second. When the write fails the file is already truncated, so a
    /// transient ENOSPC leaves a zero-byte hosts file — or, with a little space
    /// left, one cut off mid-line. The previous contents must survive intact.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_failed_write_preserves_existing_hosts_file() {
        let Some(limit) = std::env::var(FSIZE_LIMIT_VAR)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        else {
            // Parent side: hand the work to a child process, because the file
            // size limit it needs would otherwise apply to every test.
            let output = reexec_under_fsize_limit(
                "synchronizer::tests::test_failed_write_preserves_existing_hosts_file",
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success(),
                "the write-safety regression failed:\n{}\n--- stdout ---\n{stdout}",
                String::from_utf8_lossy(&output.stderr)
            );
            // A name that no longer matches would run zero tests and still exit
            // 0, silently turning this into a test that checks nothing.
            assert!(
                stdout.contains("1 passed"),
                "the child ran no test — has this test been renamed?\n{stdout}"
            );
            return;
        };

        apply_fsize_limit(limit);

        let dir = tempfile::tempdir().unwrap();
        let hosts = dir.path().join("hosts");
        let original = "127.0.0.1 localhost localhost.localdomain\n\
                        ::1 localhost localhost.localdomain\n\
                        10.0.0.5 my-precious-manual-entry\n";
        fs::write(&hosts, original).unwrap();

        let sync = new_sync(hosts.clone(), true, 100);

        // Enough containers that the rendered section far exceeds the limit.
        for i in 0..150 {
            seed_container(
                &sync,
                &format!("container{i}"),
                &format!("a-fairly-long-container-name-number-{i}"),
                &format!("172.17.{}.{}", i / 256, i % 256),
            )
            .await;
        }

        let result = sync.write_hosts_file_immediate().await;
        assert!(
            result.is_err(),
            "the write was expected to fail against the {limit} byte file size limit"
        );

        let after = fs::read_to_string(&hosts).unwrap();
        assert!(
            after.contains("my-precious-manual-entry"),
            "a failed write destroyed the user's hosts file; it is now {} bytes",
            after.len()
        );
        assert!(
            after == original,
            "a failed write corrupted the hosts file: {} bytes now, {} before.\nIt now ends with: {:?}",
            after.len(),
            original.len(),
            after.get(after.len().saturating_sub(60)..).unwrap_or("")
        );
    }
}
