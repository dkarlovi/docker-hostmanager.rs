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
