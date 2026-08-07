use anyhow::{Context, Result};
use bollard::models::{ContainerInspectResponse, EventActor, EventMessage, EventMessageTypeEnum};
use bollard::query_parameters::{EventsOptions, InspectContainerOptions, ListContainersOptions};
use bollard::Docker;
use colored::Colorize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tokio::time::{sleep, Duration};
use tokio_stream::StreamExt;
use tracing::{debug, error, info, warn};

use crate::health::HealthState;
use crate::types::{ContainerInfo, NetworkInfo};

const START_TAG: &str = "## docker-hostmanager-start";
const END_TAG: &str = "## docker-hostmanager-end";

/// How often the event loop refreshes its liveness timestamp when Docker is quiet.
const HEARTBEAT_SECS: u64 = 10;

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
fn write_in_place(path: &Path, content: &str, previous: &str) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::io::AsRawFd;

    let bytes = content.as_bytes();
    let reserve = i64::try_from(bytes.len()).context("hosts file content too large")?;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("Failed to open {} for writing", path.display()))?;

    // The step that must fail on a full filesystem, while the old contents
    // are still there. Note this deliberately does not truncate first.
    // SAFETY: `file` owns a valid open file descriptor for the whole call, and
    // `fallocate` only inspects the fd and the two integer arguments.
    let rc = unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, reserve) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            // Not every filesystem implements fallocate. Rather than refuse to
            // write at all, carry on with the weaker guarantee.
            Some(libc::EOPNOTSUPP | libc::ENOSYS) => {
                debug!(
                    "fallocate unsupported on {}, writing without a space reservation",
                    path.display()
                );
            }
            _ => {
                return Err(err).with_context(|| {
                    format!(
                        "Failed to reserve {} bytes for {}; leaving it unchanged",
                        bytes.len(),
                        path.display()
                    )
                });
            }
        }
    }

    let write = |file: &mut fs::File, data: &[u8]| -> std::io::Result<()> {
        file.seek(SeekFrom::Start(0))?;
        file.write_all(data)?;
        // Trim whatever the previous, longer contents left behind.
        file.set_len(data.len() as u64)?;
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

/// A hostname a container could not claim because another container owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostnameConflict {
    pub hostname: String,
    pub owner_name: String,
}

pub struct Synchronizer {
    docker: Docker,
    hosts_file: PathBuf,
    tld: String,
    write_enabled: bool,
    debounce_ms: u64,
    active_containers: Arc<Mutex<HashMap<String, ContainerInfo>>>,
    /// Maps each hostname to the (`container_id`, `container_name`) of the first claimant.
    /// Dynamic network-alias hostnames are also tracked here; they just never conflict
    /// because they always include a unique network name.
    hostname_claims: Arc<Mutex<HashMap<String, (String, String)>>>,
    write_notify: Notify,
    health: Arc<HealthState>,
}

impl Synchronizer {
    pub fn new(
        docker: Docker,
        hosts_file: PathBuf,
        tld: String,
        write_enabled: bool,
        debounce_ms: u64,
    ) -> Self {
        Self {
            docker,
            hosts_file,
            tld,
            write_enabled,
            debounce_ms,
            active_containers: Arc::new(Mutex::new(HashMap::new())),
            hostname_claims: Arc::new(Mutex::new(HashMap::new())),
            write_notify: Notify::new(),
            health: Arc::new(HealthState::new()),
        }
    }

    /// Shared liveness state, for the health socket served alongside the event loop.
    #[must_use]
    pub fn health(&self) -> Arc<HealthState> {
        Arc::clone(&self.health)
    }

    pub async fn synchronize(&self) -> Result<()> {
        info!("Fetching running containers...");

        let containers = self
            .docker
            .list_containers(Some(ListContainersOptions {
                all: false,
                ..Default::default()
            }))
            .await
            .context("Failed to list containers")?;

        info!("Found {} running containers", containers.len());

        // Clear all state for a clean rebuild
        {
            let mut active = self.active_containers.lock().await;
            active.clear();
        }
        {
            let mut claims = self.hostname_claims.lock().await;
            claims.clear();
        }

        // Inspect all containers, collecting start times for deterministic ordering
        let mut inspected: Vec<(Option<String>, String, ContainerInfo)> = Vec::new();
        for container in containers {
            let id = container.id.unwrap_or_default();
            if id.is_empty() {
                continue;
            }

            match self.inspect_container_with_start_time(&id).await {
                Ok((started_at, Some(info))) => {
                    if info.has_exposed_ports() {
                        inspected.push((started_at, id, info));
                    }
                }
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
            let short_id = id.get(..12).unwrap_or(&id);
            debug!("Adding container: {} ({})", info.name, short_id);
            self.claim_hostnames(&id, &info).await;
            let mut active = self.active_containers.lock().await;
            active.insert(id, info);
        }

        // As in `process_pending_writes`, a failure here is logged rather than
        // fatal: the daemon stays up and retries on the next container event.
        match self.write_hosts_file_immediate().await {
            Ok(()) => self.health.set_write_ok(true),
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
                            Ok(()) => self.health.set_write_ok(true),
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
        let mut filters = HashMap::new();
        filters.insert(
            "type".to_string(),
            vec!["container".to_string(), "network".to_string()],
        );

        let mut events = self.docker.events(Some(EventsOptions {
            filters: Some(filters),
            ..Default::default()
        }));

        tokio::select! {
            result = async {
                while let Some(event_result) = events.next().await {
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
                Ok(())
            } => result,
            result = self.process_pending_writes() => result,
            result = self.mark_alive() => result,
        }
    }

    /// Keeps the liveness timestamp fresh on a quiet Docker host, where hours can
    /// pass without a single container event. Because the runtime is
    /// `current_thread`, this stops advancing the moment anything blocks the
    /// event loop, which is exactly what the health probe needs to see.
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

        let short_actor_id = actor_id.get(..12).unwrap_or(actor_id);
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
                if let Some((container_id, _network_name)) = Self::network_event_targets(actor_ref)
                {
                    self.handle_container_up(&container_id).await?;
                }
            }
            (Some(EventMessageTypeEnum::NETWORK), "disconnect") => {
                if let Some((container_id, network_name)) = Self::network_event_targets(actor_ref) {
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
            if !info.has_exposed_ports() {
                return Ok(());
            }
            let short_actor_id = container_id.get(..12).unwrap_or(container_id);
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
            let short_actor_id = container_id.get(..12).unwrap_or(container_id);
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
        match self.inspect_container(container_id).await {
            Ok(Some(refreshed)) if refreshed.has_exposed_ports() => {
                let mut active = self.active_containers.lock().await;
                active.insert(container_id.to_string(), refreshed);
            }
            _ => {
                let mut active = self.active_containers.lock().await;
                active.remove(container_id);
            }
        }

        self.schedule_write();
    }

    fn network_event_targets(actor: Option<&EventActor>) -> Option<(String, String)> {
        let attrs = actor?.attributes.as_ref()?;
        let container_id = attrs.get("container")?.clone();
        let network_name = attrs.get("name")?.clone();
        if container_id.is_empty() || network_name.is_empty() {
            return None;
        }
        Some((container_id, network_name))
    }

    async fn inspect_container(&self, id: &str) -> Result<Option<ContainerInfo>> {
        let container = self
            .docker
            .inspect_container(id, None::<InspectContainerOptions>)
            .await
            .context("Failed to inspect container")?;

        Ok(Self::extract_container_info(container))
    }

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

        let mut networks = HashMap::new();
        if let Some(nets) = network_settings.networks {
            for (network_name, network) in nets {
                if let Some(ip) = network.ip_address {
                    if !ip.is_empty() {
                        let mut aliases = network.aliases.unwrap_or_default();
                        // Always include the container name as an alias
                        if !aliases.contains(&name) {
                            aliases.push(name.clone());
                        }

                        networks.insert(
                            network_name,
                            NetworkInfo {
                                ip_address: ip,
                                aliases,
                            },
                        );
                    }
                }
            }
        }

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
            let proj_name = compose_project
                .unwrap_or_else(|| name.split('-').next().unwrap_or(&name).to_string());
            vars.insert("COMPOSE_PROJECT_NAME".to_string(), proj_name);
        }

        let replace_vars = |input: &str, vars: &HashMap<String, String>| -> String {
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
                    if closed && !var_name.is_empty() {
                        if let Some(val) = vars.get(&var_name) {
                            result.push_str(val);
                        } else {
                            result.push('{');
                            result.push_str(&var_name);
                            result.push('}');
                        }
                    } else {
                        result.push('{');
                        result.push_str(&var_name);
                    }
                } else {
                    result.push(c);
                }
            }
            result
        };

        // Extract DOMAIN_NAME environment variable
        let mut domain_names = Vec::new();
        if let Some(config) = container.config {
            if let Some(env_vars) = config.env {
                for env in env_vars {
                    if let Some(domain_value) = env.strip_prefix("DOMAIN_NAME=") {
                        let replaced_domain = replace_vars(domain_value, &vars);
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
                    let replaced_domains = replace_vars(orbstack_domains, &vars);
                    domain_names.extend(
                        replaced_domains
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty()),
                    );
                }
            }
        }

        Some(ContainerInfo {
            id,
            name,
            ip_address: None,
            networks,
            domain_names,
            running,
        })
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
    async fn inspect_container_with_start_time(
        &self,
        id: &str,
    ) -> Result<(Option<String>, Option<ContainerInfo>)> {
        let container = self
            .docker
            .inspect_container(id, None::<InspectContainerOptions>)
            .await
            .context("Failed to inspect container")?;
        let started_at = container.state.as_ref().and_then(|s| s.started_at.clone());
        Ok((started_at, Self::extract_container_info(container)))
    }

    async fn write_hosts_file_immediate(&self) -> Result<()> {
        // Snapshot both maps so we don't hold locks during file I/O.
        let active_containers: HashMap<String, ContainerInfo> =
            self.active_containers.lock().await.clone();
        let claims: HashMap<String, (String, String)> = self.hostname_claims.lock().await.clone();

        // Build new hosts entries, filtering out hostnames claimed by other containers.
        let mut host_entries_with_ip = Vec::new();
        let mut container_count = 0;
        let mut hostname_count = 0;

        for (container_id, container) in &active_containers {
            let hostnames = container.get_hostnames(&self.tld);

            for (ip, hosts) in hostnames {
                let mut kept = Vec::new();
                let mut skipped = Vec::new();

                for h in hosts {
                    // Unresolved variables e.g. {MISSING_VAR} shouldn't be added as hostnames.
                    if h.contains('{') && h.contains('}') {
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
                    hostname_count += kept.len();
                }
            }

            container_count += 1;
        }

        host_entries_with_ip.sort_by(|a, b| a.0.cmp(&b.0));
        let host_entries: Vec<String> = host_entries_with_ip
            .into_iter()
            .map(|(_, line)| line)
            .collect();

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
            return Ok(());
        }

        // Write mode: actually update the file
        let content = fs::read_to_string(&self.hosts_file).context("Failed to read hosts file")?;

        let lines: Vec<&str> = content.lines().collect();

        // Find the managed section
        let start_idx = lines.iter().position(|line| line.trim() == START_TAG);

        let end_idx = lines.iter().position(|line| line.trim() == END_TAG);

        let mut new_lines = Vec::new();
        // Every line the user owns, i.e. everything outside our managed section.
        // Checked against the rendered result below so we can never drop one.
        let mut preserved: Vec<&str> = Vec::new();

        match (start_idx, end_idx) {
            (Some(start), Some(end)) if start < end => {
                // Managed section exists - replace it
                if let Some(before_managed) = lines.get(..start) {
                    preserved.extend(before_managed.iter().copied());
                    new_lines.extend(before_managed.iter().map(std::string::ToString::to_string));
                }

                if !host_entries.is_empty() {
                    // Add our managed section
                    new_lines.push(START_TAG.to_string());
                    new_lines.extend(host_entries);
                    new_lines.push(END_TAG.to_string());
                }
                // Note: if host_entries is empty, we don't add the tags (removes empty section)

                if end + 1 < lines.len() {
                    if let Some(after_managed) = lines.get(end + 1..) {
                        preserved.extend(after_managed.iter().copied());
                        new_lines
                            .extend(after_managed.iter().map(std::string::ToString::to_string));
                    }
                }
            }
            _ => {
                // No valid managed section - append to end
                preserved.extend(lines.iter().copied());
                new_lines.extend(lines.iter().map(std::string::ToString::to_string));

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
        }

        let new_content = new_lines.join("\n") + "\n";

        // Last line of defence before touching the file: whatever happens to the
        // managed section, not one line the user wrote may go missing.
        if let Some(dropped) = preserved
            .iter()
            .find(|line| !new_lines.iter().any(|kept| kept == *line))
        {
            return Err(anyhow::anyhow!(
                "refusing to write {}: it would drop the existing line {dropped:?}",
                self.hosts_file.display()
            ));
        }

        write_in_place(&self.hosts_file, &new_content, &content)?;

        if container_count == 0 {
            println!(
                "{} Removed empty managed section from hosts file",
                "✓".bright_green()
            );
        } else {
            println!(
                "{} Updated hosts file: {} containers, {} hostnames",
                "✓".bright_green(),
                container_count.to_string().bright_white(),
                hostname_count.to_string().bright_white()
            );
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use std::fs;
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
        {
            let mut active = sync.active_containers.lock().await;
            active.insert(
                "test123".to_string(),
                ContainerInfo {
                    id: "test123".to_string(),
                    name: "nginx".to_string(),
                    ip_address: Some("172.17.0.2".to_string()),
                    networks: HashMap::new(),
                    domain_names: vec![],
                    running: true,
                },
            );
        }

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
        let mut networks = HashMap::new();
        networks.insert(
            "testnet".to_string(),
            NetworkInfo {
                ip_address: "172.18.0.2".to_string(),
                aliases: vec!["web".to_string()],
            },
        );

        {
            let mut active = sync.active_containers.lock().await;
            active.insert(
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
        }

        sync.write_hosts_file_immediate().await.unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("127.0.0.1 localhost"));
        assert!(content.contains("192.168.1.1 server"));
        assert!(content.contains("172.18.0.2 web.testnet"));
        assert!(!content.contains("172.17.0.2 old.container"));
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
        let mut networks = HashMap::new();
        networks.insert(
            "testnet".to_string(),
            NetworkInfo {
                ip_address: "172.18.0.2".to_string(),
                aliases: vec!["web".to_string()],
            },
        );

        {
            let mut active = sync.active_containers.lock().await;
            active.insert(
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
        }

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

        let info = Synchronizer::extract_container_info(container);
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

        let info = Synchronizer::extract_container_info(container);
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

        let info = Synchronizer::extract_container_info(container);
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

        let info = Synchronizer::extract_container_info(container);
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

        let info = Synchronizer::extract_container_info(container);
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

        let info = Synchronizer::extract_container_info(container);
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
                networks: HashMap::new(),
                domain_names: vec![],
                running: true,
            },
        );
    }

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
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
                {
                    let mut active = sync.active_containers.lock().await;
                    active.clear();
                }
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
            networks: HashMap::new(),
            domain_names: vec!["myapp.local".to_string()],
            running: true,
        };
        seed_container_claimed(&sync, "aaa", container_a).await;

        // Container B starts second and tries to claim the same hostname
        let container_b = ContainerInfo {
            id: "bbb".to_string(),
            name: "container-b".to_string(),
            ip_address: Some("172.17.0.3".to_string()),
            networks: HashMap::new(),
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
            networks: HashMap::new(),
            domain_names: vec!["shared.local".to_string()],
            running: true,
        };
        let container_b = ContainerInfo {
            id: "bbb".to_string(),
            name: "container-b".to_string(),
            ip_address: Some("172.17.0.3".to_string()),
            networks: HashMap::new(),
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
            networks: HashMap::new(),
            domain_names: vec!["myapp.local".to_string()],
            running: true,
        };
        seed_container_claimed(&sync, "aaa", container_a.clone()).await;

        // A stops — release its hostnames then remove from active
        sync.release_hostnames("aaa", &container_a, None).await;
        {
            let mut active = sync.active_containers.lock().await;
            active.remove("aaa");
        }

        // B can now claim myapp.local
        let container_b = ContainerInfo {
            id: "bbb".to_string(),
            name: "container-b".to_string(),
            ip_address: Some("172.17.0.3".to_string()),
            networks: HashMap::new(),
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

        let mut networks_a = HashMap::new();
        networks_a.insert(
            "frontend".to_string(),
            NetworkInfo {
                ip_address: "172.18.0.2".to_string(),
                aliases: vec!["web".to_string()],
            },
        );
        let mut networks_b = HashMap::new();
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
                networks: HashMap::new(),
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
                networks: HashMap::new(),
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
                networks: HashMap::new(),
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
                networks: HashMap::new(),
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
                networks: HashMap::new(),
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
                networks: HashMap::new(),
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
        let mut networks = HashMap::new();
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
            Synchronizer::network_event_targets(Some(&actor)),
            Some(("abc123".to_string(), "public".to_string()))
        );

        let actor_missing = EventActor {
            id: Some("net-id".to_string()),
            attributes: Some(HashMap::new()),
        };
        assert_eq!(
            Synchronizer::network_event_targets(Some(&actor_missing)),
            None
        );
        assert_eq!(Synchronizer::network_event_targets(None), None);
    }

    // ---- regression tests for the 2026-08-07 incident ----------------------
    //
    // A transient ENOSPC on the host truncated the user's /etc/hosts to zero
    // bytes and killed the daemon. Three distinct defects were involved; each
    // gets a test below.

    fn test_container(id: &str, name: &str, ip: &str) -> ContainerInfo {
        ContainerInfo {
            id: id.to_string(),
            name: name.to_string(),
            ip_address: Some(ip.to_string()),
            networks: HashMap::new(),
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

        assert!(sync.claim_hostnames("aaa", &first).await.is_empty());
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

    /// Locates a small filesystem to exercise ENOSPC against. The test re-execs
    /// itself inside a user namespace with a tiny tmpfs; if that is unavailable
    /// (restricted CI sandbox, no `CONFIG_USER_NS`) the test skips cleanly.
    fn tiny_fs_dir() -> Option<PathBuf> {
        std::env::var("HOSTMANAGER_TINYFS").ok().map(PathBuf::from)
    }

    fn reexec_in_tiny_fs(test_name: &str) -> bool {
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        let script = "mkdir -p /tmp/tinyfs \
             && mount -t tmpfs -o size=128k tmpfs /tmp/tinyfs \
             && exec \"$0\" --exact \"$1\" --nocapture";
        let output = std::process::Command::new("unshare")
            .args(["--user", "--map-root-user", "--mount", "sh", "-c", script])
            .arg(exe)
            .arg(test_name)
            .env("HOSTMANAGER_TINYFS", "/tmp/tinyfs")
            .output();

        match output {
            Ok(o) if o.status.success() => true,
            Ok(o) => {
                // Surface the inner failure; the child's output is otherwise lost.
                let err = String::from_utf8_lossy(&o.stderr);
                let out = String::from_utf8_lossy(&o.stdout);
                let tail: String = out.lines().rev().take(15).collect::<Vec<_>>().join("\n");
                panic!("ENOSPC regression test failed inside the namespace:\n{err}\n--- stdout tail ---\n{tail}");
            }
            Err(_) => false,
        }
    }

    /// Defect 1, the one that cost the user their `/etc/hosts`.
    ///
    /// `fs::write` is `File::create` + `write_all`, i.e. `O_TRUNC` first and
    /// write second. When the write fails the file is already truncated, so a
    /// transient ENOSPC leaves a zero-byte hosts file. The previous contents
    /// must survive a failed write.
    #[tokio::test]
    async fn test_failed_write_preserves_existing_hosts_file() {
        let Some(dir) = tiny_fs_dir() else {
            if !reexec_in_tiny_fs(
                "synchronizer::tests::test_failed_write_preserves_existing_hosts_file",
            ) {
                eprintln!("SKIP: rootless tmpfs unavailable, cannot exercise ENOSPC");
            }
            return;
        };

        let hosts = dir.join("hosts");
        let original = "127.0.0.1 localhost localhost.localdomain\n\
                        ::1 localhost localhost.localdomain\n\
                        10.0.0.5 my-precious-manual-entry\n";
        fs::write(&hosts, original).unwrap();

        // Fill the filesystem to capacity so the upcoming write cannot fit,
        // even after the old contents are released by a truncation.
        let filler = dir.join("filler");
        {
            use std::io::Write as _;
            let mut f = fs::File::create(&filler).unwrap();
            let chunk = vec![0u8; 4096];
            while f.write_all(&chunk).is_ok() {}
            // Filling to capacity is the point here, so a failing flush is expected.
            let _flushed = f.flush();
        }
        assert!(
            fs::write(dir.join("probe"), vec![0u8; 8192]).is_err(),
            "the tiny filesystem still has free space; ENOSPC will not trigger"
        );

        let sync = new_sync(hosts.clone(), true, 100);

        // Enough containers that the rendered section dwarfs the free space.
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
            "the write was expected to fail with ENOSPC on the tiny filesystem"
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
