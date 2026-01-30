use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub ip_address: Option<String>,
    pub networks: HashMap<String, NetworkInfo>,
    pub domain_names: Vec<String>,
    pub running: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInfo {
    pub ip_address: String,
    pub aliases: Vec<String>,
}

impl ContainerInfo {
    pub fn has_exposed_ports(&self) -> bool {
        // A container is considered exposed if it's running and has an IP address
        self.running && (self.ip_address.is_some() || !self.networks.is_empty())
    }

    pub fn get_hostnames(&self, tld: &str) -> Vec<(String, Vec<String>)> {
        let mut result = Vec::new();

        // Global IP address with simple hostname
        if let Some(ip) = &self.ip_address {
            let mut hosts = vec![format!("{}{}", self.name, tld)];
            hosts.extend(self.domain_names.clone());
            result.push((ip.clone(), hosts));
        }

        // Network-specific IP addresses with network-qualified hostnames
        for (network_name, network_info) in &self.networks {
            let mut hosts = Vec::new();

            // Add container name with network suffix
            hosts.push(format!("{}.{}", self.name, network_name));

            // Add all aliases with network suffix
            for alias in &network_info.aliases {
                hosts.push(format!("{alias}.{network_name}"));
            }

            // Also support DOMAIN_NAME env var with network prefix (network:hostname format)
            for domain in &self.domain_names {
                if let Some((net, hostname)) = domain.split_once(':') {
                    // Match if network name is exactly the same, or ends with _<net>
                    // This allows "default:hostname" to match "project_default" network
                    let matches = network_name == net
                        || network_name.ends_with(&format!("_{net}"))
                        || network_name.ends_with(&format!("-{net}"));

                    if matches {
                        hosts.push(hostname.to_string());
                    }
                } else if self.ip_address.is_none() {
                    // If no global IP, add plain domain names to all networks
                    hosts.push(domain.clone());
                }
            }

            if !hosts.is_empty() {
                result.push((network_info.ip_address.clone(), hosts));
            }
        }

        result
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

    #[test]
    fn test_container_has_exposed_ports() {
        let container = ContainerInfo {
            id: "abc123".to_string(),
            name: "test".to_string(),
            ip_address: Some("172.17.0.2".to_string()),
            networks: HashMap::new(),
            domain_names: vec![],
            running: true,
        };
        assert!(container.has_exposed_ports());

        let container_not_running = ContainerInfo {
            id: "abc123".to_string(),
            name: "test".to_string(),
            ip_address: Some("172.17.0.2".to_string()),
            networks: HashMap::new(),
            domain_names: vec![],
            running: false,
        };
        assert!(!container_not_running.has_exposed_ports());

        let container_no_ip = ContainerInfo {
            id: "abc123".to_string(),
            name: "test".to_string(),
            ip_address: None,
            networks: HashMap::new(),
            domain_names: vec![],
            running: true,
        };
        assert!(!container_no_ip.has_exposed_ports());
    }

    #[test]
    fn test_get_hostnames_simple() {
        let container = ContainerInfo {
            id: "abc123".to_string(),
            name: "nginx".to_string(),
            ip_address: Some("172.17.0.2".to_string()),
            networks: HashMap::new(),
            domain_names: vec![],
            running: true,
        };

        let hostnames = container.get_hostnames(".docker");
        assert_eq!(hostnames.len(), 1);
        assert_eq!(hostnames[0].0, "172.17.0.2");
        assert_eq!(hostnames[0].1, vec!["nginx.docker"]);
    }

    #[test]
    fn test_get_hostnames_with_domain_names() {
        let container = ContainerInfo {
            id: "abc123".to_string(),
            name: "web".to_string(),
            ip_address: Some("172.17.0.2".to_string()),
            networks: HashMap::new(),
            domain_names: vec!["example.com".to_string(), "www.example.com".to_string()],
            running: true,
        };

        let hostnames = container.get_hostnames(".docker");
        assert_eq!(hostnames.len(), 1);
        assert_eq!(hostnames[0].0, "172.17.0.2");
        assert!(hostnames[0].1.contains(&"web.docker".to_string()));
        assert!(hostnames[0].1.contains(&"example.com".to_string()));
        assert!(hostnames[0].1.contains(&"www.example.com".to_string()));
    }

    #[test]
    fn test_get_hostnames_with_network() {
        let mut networks = HashMap::new();
        networks.insert(
            "myapp".to_string(),
            NetworkInfo {
                ip_address: "172.18.0.2".to_string(),
                aliases: vec!["web".to_string(), "www".to_string()],
            },
        );

        let container = ContainerInfo {
            id: "abc123".to_string(),
            name: "web".to_string(),
            ip_address: None,
            networks,
            domain_names: vec![],
            running: true,
        };

        let hostnames = container.get_hostnames(".docker");
        assert_eq!(hostnames.len(), 1);
        assert_eq!(hostnames[0].0, "172.18.0.2");
        assert!(hostnames[0].1.contains(&"web.myapp".to_string()));
        assert!(hostnames[0].1.contains(&"www.myapp".to_string()));
    }

    #[test]
    fn test_get_hostnames_with_network_specific_domain() {
        let mut networks = HashMap::new();
        networks.insert(
            "myapp".to_string(),
            NetworkInfo {
                ip_address: "172.18.0.2".to_string(),
                aliases: vec!["web".to_string()],
            },
        );

        let container = ContainerInfo {
            id: "abc123".to_string(),
            name: "web".to_string(),
            ip_address: None,
            networks,
            domain_names: vec![
                "myapp:api.local".to_string(),
                "myapp:admin.local".to_string(),
            ],
            running: true,
        };

        let hostnames = container.get_hostnames(".docker");
        assert_eq!(hostnames.len(), 1);
        assert_eq!(hostnames[0].0, "172.18.0.2");
        assert!(hostnames[0].1.contains(&"web.myapp".to_string()));
        assert!(hostnames[0].1.contains(&"api.local".to_string()));
        assert!(hostnames[0].1.contains(&"admin.local".to_string()));
    }

    #[test]
    fn test_get_hostnames_with_default_network_domains() {
        let mut networks = HashMap::new();
        networks.insert(
            "urq_default".to_string(), // Full network name from Docker
            NetworkInfo {
                ip_address: "172.18.0.2".to_string(),
                aliases: vec!["urq-app".to_string()],
            },
        );

        let container = ContainerInfo {
            id: "abc123".to_string(),
            name: "urq-app".to_string(),
            ip_address: None,
            networks,
            domain_names: vec![
                "default:urq.app.local".to_string(), // Simple network name in env var
                "default:urq.example.com".to_string(),
            ],
            running: true,
        };

        let hostnames = container.get_hostnames(".docker");
        assert_eq!(hostnames.len(), 1);
        assert_eq!(hostnames[0].0, "172.18.0.2");

        // Should contain all these hostnames
        assert!(hostnames[0].1.contains(&"urq-app.urq_default".to_string()));
        assert!(hostnames[0].1.contains(&"urq.app.local".to_string()));
        assert!(hostnames[0].1.contains(&"urq.example.com".to_string()));
    }

    #[test]
    fn test_network_name_matching_with_underscore_suffix() {
        // Test that "default" matches "project_default"
        let mut networks = HashMap::new();
        networks.insert(
            "myproject_default".to_string(),
            NetworkInfo {
                ip_address: "172.20.0.5".to_string(),
                aliases: vec!["web".to_string()],
            },
        );

        let container = ContainerInfo {
            id: "xyz789".to_string(),
            name: "web".to_string(),
            ip_address: None,
            networks,
            domain_names: vec!["default:api.example.com".to_string()],
            running: true,
        };

        let hostnames = container.get_hostnames(".docker");
        assert_eq!(hostnames.len(), 1);
        assert!(
            hostnames[0].1.contains(&"api.example.com".to_string()),
            "Should match 'default' in env to 'myproject_default' network"
        );
    }

    #[test]
    fn test_network_name_matching_with_hyphen_suffix() {
        // Test that "default" matches "project-default"
        let mut networks = HashMap::new();
        networks.insert(
            "stack-default".to_string(),
            NetworkInfo {
                ip_address: "172.21.0.3".to_string(),
                aliases: vec!["db".to_string()],
            },
        );

        let container = ContainerInfo {
            id: "def456".to_string(),
            name: "db".to_string(),
            ip_address: None,
            networks,
            domain_names: vec!["default:postgres.local".to_string()],
            running: true,
        };

        let hostnames = container.get_hostnames(".docker");
        assert_eq!(hostnames.len(), 1);
        assert!(
            hostnames[0].1.contains(&"postgres.local".to_string()),
            "Should match 'default' in env to 'stack-default' network"
        );
    }

    #[test]
    fn test_network_name_exact_match_takes_precedence() {
        // Test that exact match works when network is actually named "default"
        let mut networks = HashMap::new();
        networks.insert(
            "default".to_string(),
            NetworkInfo {
                ip_address: "172.22.0.2".to_string(),
                aliases: vec!["app".to_string()],
            },
        );

        let container = ContainerInfo {
            id: "exact123".to_string(),
            name: "app".to_string(),
            ip_address: None,
            networks,
            domain_names: vec!["default:exact-match.test".to_string()],
            running: true,
        };

        let hostnames = container.get_hostnames(".docker");
        assert_eq!(hostnames.len(), 1);
        assert!(
            hostnames[0].1.contains(&"exact-match.test".to_string()),
            "Should match exact network name"
        );
    }

    #[test]
    fn test_network_name_no_false_positives() {
        // Test that "default" doesn't match "mydefault" or "default_app"
        let mut networks = HashMap::new();
        networks.insert(
            "mydefault".to_string(),
            NetworkInfo {
                ip_address: "172.23.0.2".to_string(),
                aliases: vec!["app".to_string()],
            },
        );

        let container = ContainerInfo {
            id: "false123".to_string(),
            name: "app".to_string(),
            ip_address: None,
            networks,
            domain_names: vec!["default:shouldnot.match".to_string()],
            running: true,
        };

        let hostnames = container.get_hostnames(".docker");
        assert_eq!(hostnames.len(), 1);
        assert!(
            !hostnames[0].1.contains(&"shouldnot.match".to_string()),
            "Should NOT match 'default' to 'mydefault' (no separator)"
        );
    }

    #[test]
    fn test_multiple_networks_with_domain_name_matching() {
        // Test container in multiple networks with DOMAIN_NAME targeting specific ones
        let mut networks = HashMap::new();
        networks.insert(
            "frontend_default".to_string(),
            NetworkInfo {
                ip_address: "172.24.0.2".to_string(),
                aliases: vec!["web".to_string()],
            },
        );
        networks.insert(
            "backend_internal".to_string(),
            NetworkInfo {
                ip_address: "172.25.0.2".to_string(),
                aliases: vec!["web".to_string()],
            },
        );

        let container = ContainerInfo {
            id: "multi123".to_string(),
            name: "web".to_string(),
            ip_address: None,
            networks,
            domain_names: vec![
                "default:public.example.com".to_string(),
                "internal:private.local".to_string(),
            ],
            running: true,
        };

        let hostnames = container.get_hostnames(".docker");
        assert_eq!(hostnames.len(), 2);

        // Find the frontend network entry
        let frontend = hostnames
            .iter()
            .find(|(ip, _)| ip == "172.24.0.2")
            .expect("Should have frontend network");
        assert!(
            frontend.1.contains(&"public.example.com".to_string()),
            "Frontend should have public domain"
        );

        // Find the backend network entry
        let backend = hostnames
            .iter()
            .find(|(ip, _)| ip == "172.25.0.2")
            .expect("Should have backend network");
        assert!(
            backend.1.contains(&"private.local".to_string()),
            "Backend should have private domain"
        );
    }

    #[test]
    fn test_get_hostnames_multiple_networks() {
        let mut networks = HashMap::new();
        networks.insert(
            "frontend".to_string(),
            NetworkInfo {
                ip_address: "172.18.0.2".to_string(),
                aliases: vec!["web".to_string()],
            },
        );
        networks.insert(
            "backend".to_string(),
            NetworkInfo {
                ip_address: "172.19.0.2".to_string(),
                aliases: vec!["web".to_string(), "api".to_string()],
            },
        );

        let container = ContainerInfo {
            id: "abc123".to_string(),
            name: "web".to_string(),
            ip_address: None,
            networks,
            domain_names: vec![],
            running: true,
        };

        let hostnames = container.get_hostnames(".docker");
        assert_eq!(hostnames.len(), 2);

        // Check both IPs are present
        let ips: Vec<String> = hostnames.iter().map(|(ip, _)| ip.clone()).collect();
        assert!(ips.contains(&"172.18.0.2".to_string()));
        assert!(ips.contains(&"172.19.0.2".to_string()));
    }
}
