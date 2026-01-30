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
                hosts.push(format!("{}.{}", alias, network_name));
            }

            // Also support DOMAIN_NAME env var with network prefix (network:hostname format)
            for domain in &self.domain_names {
                if let Some((net, hostname)) = domain.split_once(':') {
                    if net == network_name {
                        hosts.push(hostname.to_string());
                    }
                } else if self.ip_address.is_none() {
                    // If no global IP, add plain domain names to first network
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
            domain_names: vec!["myapp:api.local".to_string(), "myapp:admin.local".to_string()],
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
