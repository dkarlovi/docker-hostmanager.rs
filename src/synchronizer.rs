use anyhow::{Context, Result};
use bollard::container::{InspectContainerOptions, ListContainersOptions};
use bollard::models::{ContainerInspectResponse, EventMessage};
use bollard::system::EventsOptions;
use bollard::Docker;
use colored::*;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
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
    active_containers: HashMap<String, ContainerInfo>,
}

impl Synchronizer {
    pub fn new(docker: Docker, hosts_file: PathBuf, tld: String, write_enabled: bool) -> Self {
        Self {
            docker,
            hosts_file,
            tld,
            write_enabled,
            active_containers: HashMap::new(),
        }
    }

    pub async fn synchronize(&mut self) -> Result<()> {
        info!("Fetching running containers...");
        
        let containers = self
            .docker
            .list_containers(Some(ListContainersOptions::<String> {
                all: false,
                ..Default::default()
            }))
            .await
            .context("Failed to list containers")?;

        info!("Found {} running containers", containers.len());

        self.active_containers.clear();

        for container in containers {
            let id = container.id.unwrap_or_default();
            if id.is_empty() {
                continue;
            }

            match self.inspect_container(&id).await {
                Ok(Some(info)) => {
                    if info.has_exposed_ports() {
                        debug!("Adding container: {} ({})", info.name.bright_white(), id[..12].bright_black());
                        self.active_containers.insert(id, info);
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    warn!("Failed to inspect container {}: {}", id, e);
                }
            }
        }

        self.write_hosts_file()?;
        
        if !self.write_enabled {
            println!();
            println!("{} To write to hosts file, run with --write flag", "ℹ".bright_blue());
        }
        
        Ok(())
    }

    pub async fn listen_events(&mut self) -> Result<()> {
        let mut filters = HashMap::new();
        filters.insert("type".to_string(), vec!["container".to_string()]);
        
        let mut events = self.docker.events(Some(EventsOptions::<String> {
            filters,
            ..Default::default()
        }));

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
    }

    async fn handle_event(&mut self, event: EventMessage) -> Result<()> {
        let action = event.action.as_deref().unwrap_or("");
        let actor_id = event
            .actor
            .as_ref()
            .and_then(|a| a.id.as_deref())
            .unwrap_or("");

        if actor_id.is_empty() {
            return Ok(());
        }

        debug!(
            "Event: {} {} ({})",
            action,
            &actor_id[..12],
            event.typ.map(|t| format!("{:?}", t)).as_deref().unwrap_or("unknown")
        );

        // React to container lifecycle events
        match action {
            "start" | "unpause" | "connect" => {
                match self.inspect_container(actor_id).await? {
                    Some(info) if info.has_exposed_ports() => {
                        println!(
                            "{} Container {} ({})",
                            "▶".bright_green(),
                            info.name.bright_white(),
                            actor_id[..12].bright_black()
                        );
                        self.active_containers.insert(actor_id.to_string(), info);
                        self.write_hosts_file()?;
                    }
                    _ => {}
                }
            }
            "die" | "stop" | "kill" | "pause" | "disconnect" | "destroy" => {
                if let Some(info) = self.active_containers.remove(actor_id) {
                    println!(
                        "{} Container {} ({})",
                        "■".bright_red(),
                        info.name.bright_white(),
                        actor_id[..12].bright_black()
                    );
                    self.write_hosts_file()?;
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

        Ok(self.extract_container_info(container))
    }

    fn extract_container_info(&self, container: ContainerInspectResponse) -> Option<ContainerInfo> {
        let id = container.id?;
        let name = container.name?.trim_start_matches('/').to_string();
        
        let state = container.state?;
        let running = state.running.unwrap_or(false);

        let network_settings = container.network_settings?;
        
        // Check if container has exposed ports
        let has_ports = network_settings
            .ports
            .map(|p| !p.is_empty())
            .unwrap_or(false);

        if !has_ports && !running {
            return None;
        }

        let ip_address = network_settings
            .ip_address
            .filter(|ip| !ip.is_empty());

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
        }

        Some(ContainerInfo {
            id,
            name,
            ip_address,
            networks,
            domain_names,
            running,
        })
    }

    fn write_hosts_file(&self) -> Result<()> {
        // Build new hosts entries
        let mut managed_lines = vec![START_TAG.to_string()];
        
        let mut container_count = 0;
        let mut hostname_count = 0;
        
        for (_id, container) in &self.active_containers {
            let hostnames = container.get_hostnames(&self.tld);
            
            for (ip, hosts) in hostnames {
                if !hosts.is_empty() {
                    managed_lines.push(format!("{} {}", ip, hosts.join(" ")));
                    hostname_count += hosts.len();
                }
            }
            container_count += 1;
        }
        
        managed_lines.push(END_TAG.to_string());

        // Display the output
        println!();
        if self.write_enabled {
            println!("{} Hosts entries to be written:", "→".bright_cyan());
        } else {
            println!("{} Generated hosts entries:", "→".bright_cyan());
        }
        
        for line in &managed_lines {
            if line != START_TAG && line != END_TAG {
                println!("  {}", line.bright_white());
            }
        }
        println!();

        if !self.write_enabled {
            println!(
                "{} {} containers, {} hostnames (dry-run mode)",
                "ℹ".bright_blue(),
                container_count.to_string().bright_white(),
                hostname_count.to_string().bright_white()
            );
            return Ok(());
        }

        // Write mode: actually update the file
        let content = fs::read_to_string(&self.hosts_file)
            .context("Failed to read hosts file")?;

        let lines: Vec<&str> = content.lines().collect();
        
        // Find the managed section
        let start_idx = lines
            .iter()
            .position(|line| line.trim() == START_TAG)
            .unwrap_or(lines.len());
        
        let end_idx = lines
            .iter()
            .position(|line| line.trim() == END_TAG)
            .unwrap_or(lines.len());

        // Reconstruct the file
        let mut new_lines = Vec::new();
        new_lines.extend(lines[..start_idx].iter().map(|s| s.to_string()));
        new_lines.extend(managed_lines);
        if end_idx + 1 < lines.len() {
            new_lines.extend(lines[end_idx + 1..].iter().map(|s| s.to_string()));
        }

        let new_content = new_lines.join("\n") + "\n";
        
        fs::write(&self.hosts_file, new_content)
            .context("Failed to write hosts file")?;

        println!(
            "{} Updated hosts file: {} containers, {} hostnames",
            "✓".bright_green(),
            container_count.to_string().bright_white(),
            hostname_count.to_string().bright_white()
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::NamedTempFile;

    #[test]
    fn test_write_hosts_file_creates_managed_section() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        
        // Write initial content
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true);
        
        sync.write_hosts_file().unwrap();
        
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains(START_TAG));
        assert!(content.contains(END_TAG));
        assert!(content.contains("127.0.0.1 localhost"));
    }

    #[test]
    fn test_write_hosts_file_updates_existing_section() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        
        // Write initial content with existing managed section
        fs::write(
            &path,
            format!(
                "127.0.0.1 localhost\n{}\n172.17.0.2 old.container\n{}\n192.168.1.1 server\n",
                START_TAG, END_TAG
            ),
        )
        .unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let mut sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), true);
        
        // Add a test container
        let mut networks = HashMap::new();
        networks.insert(
            "testnet".to_string(),
            NetworkInfo {
                ip_address: "172.18.0.2".to_string(),
                aliases: vec!["web".to_string()],
            },
        );
        
        sync.active_containers.insert(
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
        
        sync.write_hosts_file().unwrap();
        
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("127.0.0.1 localhost"));
        assert!(content.contains("192.168.1.1 server"));
        assert!(content.contains("172.18.0.2 web.testnet"));
        assert!(!content.contains("172.17.0.2 old.container"));
    }

    #[test]
    fn test_write_hosts_file_dry_run_mode() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();
        
        // Write initial content
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();

        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, path.clone(), ".docker".to_string(), false);
        
        sync.write_hosts_file().unwrap();
        
        // In dry-run mode, file should not be modified
        let content = fs::read_to_string(&path).unwrap();
        assert!(!content.contains(START_TAG));
        assert!(!content.contains(END_TAG));
        assert_eq!(content, "127.0.0.1 localhost\n");
    }

    #[test]
    fn test_extract_container_info() {
        let docker = Docker::connect_with_socket_defaults().unwrap();
        let sync = Synchronizer::new(docker, PathBuf::from("/tmp/hosts"), ".docker".to_string(), true);
        
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
                ip_address: Some("172.17.0.2".to_string()),
                ports: Some(HashMap::new()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let info = sync.extract_container_info(container);
        assert!(info.is_some());
        
        let info = info.unwrap();
        assert_eq!(info.name, "nginx");
        assert_eq!(info.ip_address, Some("172.17.0.2".to_string()));
        assert!(info.domain_names.contains(&"example.com".to_string()));
        assert!(info.domain_names.contains(&"www.example.com".to_string()));
    }
}
