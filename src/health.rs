//! Liveness reporting over a unix socket.
//!
//! Docker only probes containers it still considers running, and health status
//! lives in the same state blob that failed to persist during the 2026-08-07
//! ENOSPC incident — so this endpoint would not have caught that outage, and is
//! not meant to. It covers the complementary failure: a process that is alive
//! and reported running, but no longer doing its job.
//!
//! A signal (`kill -USR1`) cannot express this. `kill(2)` returns once the
//! signal is *queued*, not once it is *handled*, so a deadlocked process accepts
//! it exactly like a healthy one — that is `kill -0` with extra steps, and it
//! only detects "process gone", which the restart policy already handles. A
//! socket gives a real request/response round-trip, and because the runtime is
//! `current_thread`, answering it proves the same thread that runs the event
//! loop is still scheduling work. The reply carries the age of the last activity
//! and the last write result, so a wedged-but-parked loop is visible too.
//!
//! Both ends are this binary, so it works in a distroless image with no shell.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::warn;

#[cfg(unix)]
use anyhow::Context;
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
#[cfg(unix)]
use tracing::{debug, error};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Liveness shared between the event loop and the health socket.
#[derive(Debug)]
pub struct HealthState {
    last_activity: AtomicU64,
    last_write_ok: AtomicBool,
}

impl Default for HealthState {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            last_activity: AtomicU64::new(now_secs()),
            last_write_ok: AtomicBool::new(true),
        }
    }

    /// Records that the event loop is still turning.
    pub fn touch(&self) {
        self.last_activity.store(now_secs(), Ordering::Relaxed);
    }

    /// Records the outcome of the most recent hosts file write.
    pub fn set_write_ok(&self, ok: bool) {
        self.last_write_ok.store(ok, Ordering::Relaxed);
    }

    // Only the unix health socket reads these back out.
    #[cfg_attr(not(unix), allow(dead_code))]
    #[must_use]
    pub fn age_secs(&self) -> u64 {
        now_secs().saturating_sub(self.last_activity.load(Ordering::Relaxed))
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    #[must_use]
    pub fn write_ok(&self) -> bool {
        self.last_write_ok.load(Ordering::Relaxed)
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    fn report(&self) -> String {
        format!("age={} write_ok={}\n", self.age_secs(), self.write_ok())
    }
}

/// Serves the health socket for as long as the process runs.
///
/// A bind failure is logged and the future then parks forever rather than
/// returning: taking the daemon down over its own health endpoint would cause
/// exactly the kind of outage the endpoint exists to reveal.
#[cfg(unix)]
pub async fn serve(state: Arc<HealthState>, path: PathBuf) {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                warn!(
                    "Could not create {} for the health socket: {e}",
                    parent.display()
                );
            }
        }
    }

    // A socket left behind by a previous run makes bind fail with EADDRINUSE.
    if path.exists() {
        if let Err(e) = std::fs::remove_file(&path) {
            warn!(
                "Could not remove stale health socket {}: {e}",
                path.display()
            );
        }
    }

    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(e) => {
            error!("Health socket unavailable at {}: {e}", path.display());
            std::future::pending::<()>().await;
            return;
        }
    };
    debug!("Health socket listening on {}", path.display());

    loop {
        match listener.accept().await {
            Ok((mut stream, _)) => {
                let report = state.report();
                if let Err(e) = stream.write_all(report.as_bytes()).await {
                    debug!("Failed to answer a health probe: {e}");
                }
            }
            Err(e) => {
                warn!("Health socket accept failed: {e}");
                // Don't spin hot if the listener is persistently unhappy.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// The health endpoint is built on unix domain sockets, which Windows lacks an
/// equivalent of here. The daemon still runs; only the probe is unavailable.
#[cfg(not(unix))]
pub async fn serve(_state: Arc<HealthState>, _path: PathBuf) {
    warn!("Health socket is not supported on this platform; probes will fail");
    std::future::pending::<()>().await;
}

/// Queries a running instance over its health socket.
///
/// Returns a human-readable summary when healthy, and an error describing the
/// problem otherwise.
#[cfg(unix)]
pub async fn probe(path: &Path, max_age_secs: u64, timeout: Duration) -> Result<String> {
    let mut stream = tokio::time::timeout(timeout, UnixStream::connect(path))
        .await
        .with_context(|| format!("timed out connecting to {}", path.display()))?
        .with_context(|| format!("could not connect to {}", path.display()))?;

    let mut buf = String::new();
    tokio::time::timeout(timeout, stream.read_to_string(&mut buf))
        .await
        .context("timed out reading the health response")?
        .context("failed to read the health response")?;

    let mut parsed_age = None;
    let mut parsed_write_ok = None;
    for field in buf.split_whitespace() {
        if let Some(v) = field.strip_prefix("age=") {
            parsed_age = v.parse::<u64>().ok();
        } else if let Some(v) = field.strip_prefix("write_ok=") {
            parsed_write_ok = v.parse::<bool>().ok();
        }
    }

    let (Some(age), Some(write_ok)) = (parsed_age, parsed_write_ok) else {
        bail!("unrecognised health response: {buf:?}");
    };

    if age > max_age_secs {
        bail!("event loop stalled: no activity for {age}s (limit {max_age_secs}s)");
    }
    if !write_ok {
        bail!("the most recent hosts file write failed");
    }

    Ok(format!(
        "healthy: last activity {age}s ago, last write succeeded"
    ))
}

#[cfg(not(unix))]
pub async fn probe(_path: &Path, _max_age_secs: u64, _timeout: Duration) -> Result<String> {
    bail!("health probes are not supported on this platform")
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn socket_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hostmanager-health-test-{name}"));
        let _cleaned = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("health.sock")
    }

    async fn spawn_server(state: Arc<HealthState>, path: PathBuf) {
        tokio::spawn(serve(state, path.clone()));
        // Wait for the socket to appear rather than sleeping a fixed amount.
        for _ in 0..100 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("health socket never appeared at {}", path.display());
    }

    #[tokio::test]
    async fn test_probe_reports_healthy() {
        let path = socket_path("healthy");
        let state = Arc::new(HealthState::new());
        spawn_server(Arc::clone(&state), path.clone()).await;

        let result = probe(&path, 90, Duration::from_secs(5)).await;
        assert!(result.is_ok(), "expected healthy, got: {result:?}");
    }

    #[tokio::test]
    async fn test_probe_fails_when_activity_is_stale() {
        let path = socket_path("stale");
        let state = Arc::new(HealthState::new());
        // Backdate the last activity well past the threshold.
        state
            .last_activity
            .store(now_secs().saturating_sub(600), Ordering::Relaxed);
        spawn_server(Arc::clone(&state), path.clone()).await;

        let err = probe(&path, 90, Duration::from_secs(5))
            .await
            .expect_err("a stalled event loop must be reported unhealthy");
        assert!(
            err.to_string().contains("stalled"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_probe_fails_when_last_write_failed() {
        let path = socket_path("writefail");
        let state = Arc::new(HealthState::new());
        state.set_write_ok(false);
        spawn_server(Arc::clone(&state), path.clone()).await;

        let err = probe(&path, 90, Duration::from_secs(5))
            .await
            .expect_err("a failed hosts file write must be reported unhealthy");
        assert!(
            err.to_string().contains("write failed"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_probe_fails_when_nothing_is_listening() {
        let path = socket_path("absent");
        let err = probe(&path, 90, Duration::from_millis(500))
            .await
            .expect_err("probing a dead instance must fail");
        assert!(
            err.to_string().contains("could not connect"),
            "unexpected error: {err}"
        );
    }
}
