use anyhow::{Context, Result};
use bollard::models::{ContainerInspectResponse, EventMessage};
use bollard::query_parameters::{EventsOptions, InspectContainerOptions, ListContainersOptions};
use bollard::Docker;
use colored::Colorize;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tokio::time::{sleep, Duration};
use tokio_stream::StreamExt;
use tracing::{debug, error, info, warn};

use crate::types::{ContainerInfo, NetworkInfo};

const START_TAG: &str = "## docker-hostmanager-start";
const END_TAG: &str = "## docker-hostmanager-end";

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
        }
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

        self.write_hosts_file_immediate().await?;

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
                        self.write_hosts_file_immediate().await?;
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
        filters.insert("type".to_string(), vec!["container".to_string()]);

        let mut events = self.docker.events(Some(EventsOptions {
            filters: Some(filters),
            ..Default::default()
        }));

        tokio::select! {
            result = async {
                while let Some(event_result) = events.next().await {
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
        }
    }

    async fn handle_event(&self, event: EventMessage) -> Result<()> {
        let action = event.action.as_deref().unwrap_or("");
        let actor_id = event
            .actor
            .as_ref()
            .and_then(|a| a.id.as_deref())
            .unwrap_or("");

        if actor_id.is_empty() {
            return Ok(());
        }

        let short_actor_id = actor_id.get(..12).unwrap_or(actor_id);
        debug!(
            "Event: {} {} ({})",
            action,
            short_actor_id,
            event
                .typ
                .map(|t| format!("{t:?}"))
                .as_deref()
                .unwrap_or("unknown")
        );

        // React to container lifecycle events
        match action {
            "start" | "unpause" | "connect" => match self.inspect_container(actor_id).await? {
                Some(info) if info.has_exposed_ports() => {
                    let short_actor_id = actor_id.get(..12).unwrap_or(actor_id);
                    println!(
                        "{} Container {} ({})",
                        "▶".bright_green(),
                        info.name.bright_white(),
                        short_actor_id.bright_black()
                    );
                    self.claim_hostnames(actor_id, &info).await;
                    let mut active = self.active_containers.lock().await;
                    active.insert(actor_id.to_string(), info);
                    drop(active);
                    self.schedule_write();
                }
                _ => {}
            },
            "die" | "stop" | "kill" | "pause" | "disconnect" | "destroy" => {
                let mut active = self.active_containers.lock().await;
                if let Some(info) = active.remove(actor_id) {
                    drop(active);
                    self.release_hostnames(actor_id, &info).await;
                    let short_actor_id = actor_id.get(..12).unwrap_or(actor_id);
                    println!(
                        "{} Container {} ({})",
                        "■".bright_red(),
                        info.name.bright_white(),
                        short_actor_id.bright_black()
                    );
                    self.schedule_write();
                }
            }
            _ => {}
        }

        Ok(())
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

        // Extract DOMAIN_NAME environment variable
        let mut domain_names = Vec::new();
        if let Some(config) = container.config {
            if let Some(env_vars) = config.env {
                for env in env_vars {
                    if let Some(domain_value) = env.strip_prefix("DOMAIN_NAME=") {
                        domain_names.extend(
                            domain_value
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
                    domain_names.extend(
                        orbstack_domains
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
    async fn claim_hostnames(&self, container_id: &str, container: &ContainerInfo) {
        let all_hostnames: Vec<String> = container
            .get_hostnames(&self.tld)
            .into_iter()
            .flat_map(|(_, hosts)| hosts)
            .collect();

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
                std::collections::hash_map::Entry::Occupied(e) => {
                    let (_, owner_name) = e.get();
                    warn!(
                        "Hostname \"{}\" already claimed by \"{}\", skipping for \"{}\"",
                        hostname, owner_name, container.name
                    );
                }
            }
        }
    }

    /// Releases all hostname claims held by `container_id`.
    async fn release_hostnames(&self, container_id: &str, container: &ContainerInfo) {
        let all_hostnames: Vec<String> = container
            .get_hostnames(&self.tld)
            .into_iter()
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
                    if claims
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

        match (start_idx, end_idx) {
            (Some(start), Some(end)) if start < end => {
                // Managed section exists - replace it
                if let Some(before_managed) = lines.get(..start) {
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
                        new_lines
                            .extend(after_managed.iter().map(std::string::ToString::to_string));
                    }
                }
            }
            _ => {
                // No valid managed section - append to end
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

        fs::write(&self.hosts_file, new_content).context("Failed to write hosts file")?;

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
        sync.release_hostnames("aaa", &container_a).await;
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
}
