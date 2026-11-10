//! Owned Docker fixture for the real NATS transport tests.
use std::{
    io,
    net::SocketAddr,
    process::{Command, Output, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::time::{sleep, timeout};

const OWNER_LABEL: &str = "io.awaken.foundation-test-owner";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) struct NatsFixture {
    _container: OwnedContainer,
    pub(super) url: String,
}

struct OwnedContainer {
    name: String,
    id: Option<String>,
}

fn container_id(value: &str) -> Option<String> {
    (value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then(|| value.to_owned())
}

// Bound CLI failure as well as protocol readiness. Stderr is inherited so a
// pull cannot fill a pipe while the parent waits for the short stdout response.
fn docker(args: &[&str]) -> io::Result<Output> {
    let mut child = Command::new("docker")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child.try_wait()? {
            Some(_) => return child.wait_with_output(),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Docker fixture command timed out",
                ));
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

impl OwnedContainer {
    fn id(&self) -> &str {
        self.id
            .as_deref()
            .expect("the owned container ID was verified")
    }

    // A failed or cancelled create may have reached Docker before returning its
    // ID. Recover only this unique name with the exact per-fixture owner label.
    fn recover_id(&self) -> Option<String> {
        let output = docker(&["inspect", "--format", "{{json .}}", &self.name]).ok()?;
        if !output.status.success() {
            return None;
        }
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
        if value["Config"]["Labels"][OWNER_LABEL].as_str() != Some(&self.name) {
            return None;
        }
        container_id(value["Id"].as_str()?)
    }
}

impl Drop for OwnedContainer {
    fn drop(&mut self) {
        let Some(id) = self.id.clone().or_else(|| self.recover_id()) else {
            return;
        };
        if !docker(&["rm", "--force", &id]).is_ok_and(|output| output.status.success()) {
            eprintln!("failed to remove an owned NATS test container");
        }
    }
}

impl NatsFixture {
    pub(super) async fn start() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_nanos();
        let mut container = OwnedContainer {
            name: format!("awaken-foundation-nats-{}-{nonce}", std::process::id()),
            id: None,
        };
        let label = format!("{OWNER_LABEL}={}", container.name);
        let output = docker(&[
            "create",
            "--rm",
            "--name",
            &container.name,
            "--label",
            &label,
            "--publish",
            "127.0.0.1::4222",
            "nats:2.10-alpine",
        ])
        .expect("Docker is required for the real NATS transport tests");
        assert!(
            output.status.success(),
            "owned NATS fixture creation failed"
        );
        let id = String::from_utf8(output.stdout).expect("Docker container ID is UTF-8");
        container.id = Some(container_id(id.trim()).expect("one exact Docker container ID"));
        let started = docker(&["start", container.id()]).expect("start the owned NATS container");
        assert!(
            started.status.success(),
            "owned NATS container failed to start"
        );
        let port = docker(&["port", container.id(), "4222/tcp"]).expect("read the owned NATS port");
        assert!(port.status.success(), "owned NATS port lookup failed");
        let address: SocketAddr = String::from_utf8(port.stdout)
            .expect("Docker port is UTF-8")
            .trim()
            .parse()
            .expect("one published NATS socket address");
        assert!(
            address.ip().is_loopback() && address.port() != 0,
            "the fixture must expose only a dynamic loopback port"
        );
        let fixture = Self {
            _container: container,
            url: format!("nats://{address}"),
        };
        // Keep the former fixture's startup budget; require the real protocol.
        timeout(Duration::from_secs(60), async {
            loop {
                if async_nats::connect(fixture.url.clone()).await.is_ok() {
                    break;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("owned NATS fixture readiness timed out");
        fixture
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(id: &str) -> bool {
        let output = docker(&["inspect", "--format", "{{.State.Running}}", id])
            .expect("inspect the exact owned container");
        output.status.success() && output.stdout == b"true\n"
    }

    #[tokio::test]
    async fn cleanup_removes_only_its_container_and_runs_during_unwind() {
        let first = NatsFixture::start().await;
        let second = NatsFixture::start().await;
        let first_id = first._container.id().to_owned();
        let second_id = second._container.id().to_owned();
        assert!(running(&first_id) && running(&second_id));
        drop(first);
        assert!(!running(&first_id), "drop must remove its own container");
        assert!(running(&second_id), "another fixture must remain running");
        let result = std::panic::catch_unwind(move || {
            let _guard = second;
            panic!("exercise fixture cleanup after a test failure");
        });
        assert!(result.is_err());
        assert!(
            !running(&second_id),
            "unwind must also remove the owned container"
        );
    }
}
