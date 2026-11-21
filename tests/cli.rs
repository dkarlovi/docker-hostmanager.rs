//! Runs the compiled binary for behaviour that lives in `main`.
#![expect(clippy::expect_used, reason = "tests should fail loudly")]

use std::process::Command;

#[test]
fn version_does_not_need_docker() {
    let output = Command::new(env!("CARGO_BIN_EXE_docker-hostmanager"))
        .arg("version")
        .env("DOCKER_SOCKET", "unix:///nonexistent/docker.sock")
        .output()
        .expect("the binary should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "version must work without a Docker daemon; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        [format!(
            "dkarlovi/docker-hostmanager {}",
            env!("GIT_VERSION")
        )],
        "version prints only the version line"
    );
}

#[cfg(unix)]
#[expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests should fail loudly"
)]
mod unix {
    use std::path::Path;
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;
    use tokio::process::Command;
    use tokio::sync::mpsc;

    /// Serve the Docker endpoints used at startup, optionally leaving the
    /// container list request unanswered to simulate a busy daemon.
    fn docker_stub(socket: &Path, stall_list: bool) -> mpsc::UnboundedReceiver<String> {
        let listener = UnixListener::bind(socket).unwrap();
        let (requests, received) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request_bytes = Vec::new();
                while !request_bytes.ends_with(b"\r\n\r\n") {
                    let byte = stream.read_u8().await.unwrap();
                    request_bytes.push(byte);
                }
                let request = String::from_utf8(request_bytes).unwrap();
                let path = request.split_whitespace().nth(1).unwrap().to_string();
                requests.send(path.clone()).unwrap();
                let endpoint = path.split('?').next().unwrap();
                let (status, body) = if endpoint.starts_with("/v1.")
                    && !endpoint.starts_with("/v1.41/")
                {
                    (
                        "400 Bad Request",
                        r#"{"message":"client API version is too new"}"#,
                    )
                } else if endpoint.ends_with("/version") {
                    (
                        "200 OK",
                        r#"{"Version":"test-daemon","ApiVersion":"1.41","MinAPIVersion":"1.24"}"#,
                    )
                } else if endpoint.ends_with("/containers/json") {
                    if stall_list {
                        std::future::pending::<()>().await;
                    }
                    ("200 OK", "[]")
                } else if endpoint.ends_with("/events") {
                    std::future::pending::<()>().await;
                    return;
                } else {
                    panic!("unexpected Docker request: {path}");
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        received
    }

    #[tokio::test]
    async fn watch_once_works_with_older_docker_api() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("docker.sock");
        let _requests = docker_stub(&socket, false);
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            command(&socket, &dir.path().join("health.sock"))
                .args(["watch", "--once"])
                .output(),
        )
        .await
        .unwrap()
        .unwrap();

        assert!(
            output.status.success(),
            "watch must work with a daemon supporting API 1.41; stdout: {}; stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn command(socket: &Path, health_socket: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_docker-hostmanager"));
        command
            .args(["--socket", &format!("unix://{}", socket.display())])
            .arg("--health-socket")
            .arg(health_socket)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    #[tokio::test]
    async fn sync_once_fails_when_hosts_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("docker.sock");
        let _requests = docker_stub(&socket, false);
        // A directory exists, but cannot be read or written as a hosts file.
        // This also works when the test runs as root, unlike chmod-based tests.
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            command(&socket, &dir.path().join("health.sock"))
                .arg("sync")
                .arg(dir.path())
                .arg("--once")
                .output(),
        )
        .await
        .unwrap()
        .unwrap();

        assert!(
            !output.status.success(),
            "sync --once must report failure when no hosts file was written; stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[tokio::test]
    async fn sigterm_interrupts_initial_container_listing() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("docker.sock");
        let mut requests = docker_stub(&socket, true);
        let mut child = command(&socket, &dir.path().join("health.sock"))
            .arg("watch")
            .spawn()
            .unwrap();

        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(request) = requests.recv().await {
                if request.starts_with("/containers/json") {
                    return;
                }
            }
            panic!("the daemon never received the initial container list request");
        })
        .await
        .unwrap();

        let delivered = Command::new("kill")
            .args(["-TERM", &child.id().unwrap().to_string()])
            .status()
            .await
            .unwrap();
        assert!(delivered.success(), "SIGTERM should be delivered");

        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .expect("SIGTERM must cancel startup without waiting for Docker")
            .unwrap();
        assert!(status.success(), "shutdown should exit cleanly: {status}");
    }
}
