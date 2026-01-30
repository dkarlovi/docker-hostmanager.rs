use anyhow::{Context, Result};
use bollard::Docker;
use clap::Parser;
use colored::*;
use std::path::PathBuf;
use tracing::{error, info};

mod types;
mod synchronizer;

use synchronizer::Synchronizer;

#[derive(Parser, Debug)]
#[command(author, version, about = "Docker Host Manager - Automatically update /etc/hosts with container hostnames", long_about = None)]
struct Args {
    /// Path to the hosts file
    #[arg(short = 'f', long, env = "HOSTS_FILE", default_value = "/etc/hosts")]
    hosts_file: PathBuf,

    /// Top-level domain to use for containers without networks
    #[arg(short = 't', long, env = "TLD", default_value = ".docker")]
    tld: String,

    /// Docker socket path
    #[arg(short = 's', long, env = "DOCKER_SOCKET", default_value = "unix:///var/run/docker.sock")]
    socket: String,

    /// Run once and exit (don't listen for events)
    #[arg(long)]
    once: bool,

    /// Write to hosts file (default is dry-run mode that only displays output)
    #[arg(short = 'w', long)]
    write: bool,

    /// Verbose mode
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize logging
    let filter = if args.verbose {
        "docker_hostmanager=debug,bollard=info"
    } else {
        "docker_hostmanager=info,bollard=warn"
    };
    
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(true)
        .init();

    println!("{}", "Docker Host Manager".bright_cyan().bold());
    println!("{}", "===================".bright_cyan());
    println!();

    // Connect to Docker
    info!("{} {}", "Connecting to Docker at".bright_blue(), args.socket.bright_white());
    let docker = Docker::connect_with_socket(&args.socket, 120, bollard::API_DEFAULT_VERSION)
        .context("Failed to connect to Docker socket")?;

    // Verify connection
    let version = docker.version().await.context("Failed to verify Docker connection")?;
    info!("{} Docker {}", "✓".bright_green(), version.version.unwrap_or_default().bright_white());
    println!();

    // Create synchronizer
    let mut sync = Synchronizer::new(docker, args.hosts_file.clone(), args.tld.clone(), args.write);

    if args.write {
        // Check if hosts file is writable
        if !args.hosts_file.exists() {
            error!("{} Hosts file does not exist: {}", "✗".bright_red(), args.hosts_file.display());
            return Err(anyhow::anyhow!("Hosts file does not exist"));
        }
        info!("{} Write mode enabled - will update {}", "✓".bright_green(), args.hosts_file.display());
    } else {
        info!("{} Dry-run mode - will only display output (use --write to update hosts file)", "ℹ".bright_blue());
    }
    println!();

    // Initial synchronization
    info!("{}", "Performing initial synchronization...".bright_yellow());
    sync.synchronize().await?;
    info!("{} {}", "✓".bright_green(), "Initial synchronization complete".bright_white());

    if args.once {
        info!("{}", "Running in once mode, exiting...".bright_yellow());
        return Ok(());
    }

    // Listen for events
    info!("{}", "Listening for Docker events...".bright_yellow());
    sync.listen_events().await?;

    Ok(())
}
