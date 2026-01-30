use anyhow::{Context, Result};
use bollard::Docker;
use clap::{Parser, Subcommand};
use colored::*;
use std::path::PathBuf;
use tokio::signal;

mod synchronizer;
mod types;

use synchronizer::Synchronizer;

#[derive(Parser, Debug)]
#[command(author, version, about = "Docker Host Manager - Automatically update /etc/hosts with container hostnames", long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Docker socket path
    #[arg(
        short = 's',
        long,
        env = "DOCKER_SOCKET",
        default_value = "unix:///var/run/docker.sock",
        global = true
    )]
    socket: String,

    /// Top-level domain to use for containers without networks
    #[arg(
        short = 't',
        long,
        env = "TLD",
        default_value = ".docker",
        global = true
    )]
    tld: String,

    /// Debounce delay in milliseconds before writing to hosts file (allows multiple containers to start)
    #[arg(long, env = "DEBOUNCE_MS", default_value = "100", global = true)]
    debounce_ms: u64,

    /// Verbose mode
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Watch Docker events and display hostname changes (read-only, no file modifications)
    Watch {
        /// Run once and exit (don't listen for events)
        #[arg(long)]
        once: bool,
    },
    /// Synchronize container hostnames to hosts file (writes to file)
    Sync {
        /// Path to the hosts file to update
        #[arg(value_name = "HOSTS_FILE")]
        hosts_file: PathBuf,

        /// Run once and exit (don't listen for events)
        #[arg(long)]
        once: bool,
    },
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
    println!(
        "{} {}",
        "Connecting to Docker at".bright_blue(),
        args.socket.bright_white()
    );
    let docker = Docker::connect_with_socket(&args.socket, 120, bollard::API_DEFAULT_VERSION)
        .context("Failed to connect to Docker socket")?;

    // Verify connection
    let version = docker
        .version()
        .await
        .context("Failed to verify Docker connection")?;
    println!(
        "{} Docker {}",
        "✓".bright_green(),
        version.version.unwrap_or_default().bright_white()
    );
    println!();

    // Determine command (default to watch)
    let command = args.command.unwrap_or(Commands::Watch { once: false });

    match command {
        Commands::Watch { once } => {
            println!(
                "{} Watch mode - displaying hostname changes only",
                "ℹ".bright_blue()
            );
            println!();

            let mut sync = Synchronizer::new(
                docker,
                PathBuf::from("/etc/hosts"), // Unused in watch mode
                args.tld.clone(),
                false, // Never write in watch mode
                args.debounce_ms,
            );

            println!(
                "{}",
                "Performing initial synchronization...".bright_yellow()
            );
            sync.synchronize().await?;
            println!(
                "{} {}",
                "✓".bright_green(),
                "Initial synchronization complete".bright_white()
            );

            if once {
                println!("{}", "Running in once mode, exiting...".bright_yellow());
                return Ok(());
            }

            println!();
            println!("{}", "Listening for Docker events...".bright_yellow());
            println!("{}", "(Press Ctrl+C to stop)".bright_black());

            tokio::select! {
                result = sync.listen_events() => {
                    result?;
                }
                _ = signal::ctrl_c() => {
                    println!();
                    println!("{}", "Received shutdown signal, exiting gracefully...".bright_yellow());
                }
            }
        }
        Commands::Sync { hosts_file, once } => {
            if !hosts_file.exists() {
                eprintln!(
                    "{} Hosts file does not exist: {}",
                    "✗".bright_red(),
                    hosts_file.display()
                );
                return Err(anyhow::anyhow!("Hosts file does not exist"));
            }

            println!(
                "{} Sync mode - will update {}",
                "✓".bright_green(),
                hosts_file.display()
            );
            println!();

            let mut sync = Synchronizer::new(
                docker,
                hosts_file.clone(),
                args.tld.clone(),
                true, // Always write in sync mode
                args.debounce_ms,
            );

            println!(
                "{}",
                "Performing initial synchronization...".bright_yellow()
            );
            sync.synchronize().await?;
            println!(
                "{} {}",
                "✓".bright_green(),
                "Initial synchronization complete".bright_white()
            );

            if once {
                println!("{}", "Running in once mode, exiting...".bright_yellow());
                return Ok(());
            }

            println!();
            println!("{}", "Listening for Docker events...".bright_yellow());
            println!("{}", "(Press Ctrl+C to stop)".bright_black());

            tokio::select! {
                result = sync.listen_events() => {
                    result?;
                }
                _ = signal::ctrl_c() => {
                    println!();
                    println!("{}", "Received shutdown signal, exiting gracefully...".bright_yellow());
                }
            }
        }
    }

    Ok(())
}
