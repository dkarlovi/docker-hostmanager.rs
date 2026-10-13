use anyhow::{Context, Result};
use bollard::Docker;
use clap::{Parser, Subcommand};
use colored::Colorize;
use std::path::PathBuf;
use tokio::signal;

mod engine;
mod health;
mod synchronizer;
mod types;

use synchronizer::{probe_reservation_support, ReservationSupport, Synchronizer};

// Version from git tag at build time
const VERSION: &str = env!("GIT_VERSION");
const PACKAGE_NAME: &str = env!("CARGO_PKG_NAME");

/// How long a single Docker API request may take, in seconds.
///
/// Generous because the initial sync inspects every running container in turn
/// on hosts that may be busy; bollard's own default is the same two minutes.
const DOCKER_TIMEOUT_SECS: u64 = 120;

/// How long the `health` subcommand waits for the daemon to answer.
///
/// Matches the image's `HEALTHCHECK --timeout=5s`: Docker kills the probe at
/// that point anyway, and answering sooner yields a clearer failure message.
const HEALTH_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Parser, Debug)]
#[command(author, version = VERSION, about = "Docker Host Manager - Automatically update /etc/hosts with container hostnames", long_about = None)]
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

    /// Path to the unix socket used for health probes
    #[arg(
        long,
        env = "HEALTH_SOCKET",
        default_value = "/tmp/hostmanager.sock",
        global = true
    )]
    health_socket: PathBuf,

    /// Treat the instance as unhealthy if the event loop has been idle this long
    #[arg(long, env = "HEALTH_MAX_AGE_SECS", default_value = "90", global = true)]
    health_max_age_secs: u64,

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
    /// Probe a running instance over its health socket (used by HEALTHCHECK)
    Health,
    /// Show version information
    Version,
}

/// Resolves when the process is asked to stop: Ctrl+C, or SIGTERM from
/// `docker stop`. The handlers are installed before this returns, so a signal
/// arriving before the future is first polled is not lost.
#[cfg(unix)]
fn shutdown_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    use signal::unix::{signal, SignalKind};

    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
    })
}

#[cfg(not(unix))]
fn shutdown_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    Ok(async {
        if let Err(e) = signal::ctrl_c().await {
            tracing::error!("Failed to listen for Ctrl+C: {e}");
        }
    })
}

#[tokio::main(flavor = "current_thread")]
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

    // Answer health probes and version queries before anything else: neither
    // may print the banner, nor need a Docker connection.
    if matches!(args.command, Some(Commands::Health)) {
        let summary = health::probe(
            &args.health_socket,
            args.health_max_age_secs,
            HEALTH_PROBE_TIMEOUT,
        )
        .await
        .context("health check failed")?;
        println!("{summary}");
        return Ok(());
    }
    if matches!(args.command, Some(Commands::Version)) {
        println!("dkarlovi/{PACKAGE_NAME} {VERSION}");
        return Ok(());
    }

    println!("{}", "Docker Host Manager".bright_cyan().bold());
    println!("{}", "===================".bright_cyan());
    println!("{} {}", "Version:".bright_black(), VERSION.bright_white());
    println!();

    // Connect to Docker
    println!(
        "{} {}",
        "Connecting to Docker at".bright_blue(),
        args.socket.bright_white()
    );
    let docker = Docker::connect_with_socket(
        &args.socket,
        DOCKER_TIMEOUT_SECS,
        bollard::API_DEFAULT_VERSION,
    )
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

    let (hosts_file, write_enabled, once) = match command {
        // Already handled above, before the Docker connection was made.
        Commands::Health | Commands::Version => return Ok(()),
        Commands::Watch { once } => {
            println!(
                "{} Watch mode - displaying hostname changes only",
                "ℹ".bright_blue()
            );
            // No reservation line here: watch mode never writes a hosts file.
            (PathBuf::from("/etc/hosts"), false, once) // Unused in watch mode
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
            match probe_reservation_support(&hosts_file) {
                ReservationSupport::Available => println!(
                    "{} Space reservation active - a failed write cannot damage the file",
                    "✓".bright_green()
                ),
                ReservationSupport::Unavailable(reason) => println!(
                    "{} Space reservation unavailable ({}) - falling back to restore-on-failure",
                    "⚠".bright_yellow(),
                    reason.bright_white()
                ),
                ReservationSupport::NotWritable(reason) => println!(
                    "{} Cannot open {} for writing ({}) - every update will fail",
                    "✗".bright_red(),
                    hosts_file.display(),
                    reason.bright_white()
                ),
            }
            (hosts_file, true, once)
        }
    };

    println!(
        "{} Health socket: {}",
        "ℹ".bright_blue(),
        args.health_socket.display().to_string().bright_white()
    );
    println!();

    // Installed before the initial sync, so a `docker stop` during startup is
    // honoured instead of ignored (as PID 1 there is no default action).
    let shutdown = shutdown_signal().context("Failed to install signal handlers")?;

    let sync = Synchronizer::new(
        docker,
        hosts_file,
        args.tld,
        write_enabled,
        args.debounce_ms,
    );
    let health_state = sync.health();

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
        () = health::serve(health_state, args.health_socket) => {}
        () = shutdown => {
            println!();
            println!("{}", "Received shutdown signal, exiting gracefully...".bright_yellow());
        }
    }

    Ok(())
}

#[cfg(all(test, unix))]
#[expect(clippy::expect_used, reason = "tests should fail loudly")]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_shutdown_signal_resolves_on_sigterm() {
        // `docker stop` sends SIGTERM, and as PID 1 the process gets no default
        // action for it: without a handler, every stop waits out the kill timeout.
        let shutdown = shutdown_signal().expect("signal handlers should install");

        let status = std::process::Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .expect("kill should run");
        assert!(status.success(), "kill should deliver SIGTERM");

        tokio::time::timeout(std::time::Duration::from_secs(5), shutdown)
            .await
            .expect("SIGTERM should trigger shutdown");
    }
}
