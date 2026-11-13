//! Owned Docker fixture for the real NATS transport tests.
use std::{
    io,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) struct NatsFixture {
    _container: DockerNatsGuard,
    id: String,
    pub(super) url: String,
}

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

// The original fixture remains the only lifecycle owner. A std thread
// drains Docker setup before cleanup: killing a CLI future cannot prove
// that the daemon did not create a late container. No extra Tokio runtime
// or global reaper survives the test. SIGKILL/daemon loss are not guaranteed.
struct DockerNatsGuard {
    stop: std::sync::mpsc::Sender<()>,
    owner: Option<std::thread::JoinHandle<Result<(), String>>>,
    created: Option<tokio::sync::oneshot::Receiver<Result<(std::time::Instant, String), String>>>,
    started: Option<tokio::sync::oneshot::Receiver<Result<String, String>>>,
}

impl DockerNatsGuard {
    const LABEL: &str = "io.awaken.foundation-test-owner";

    fn begin(entrypoint: Option<&str>, after_create: impl FnOnce(&str) + Send + 'static) -> Self {
        let name = format!(
            "foundation-nats-{}-{:x}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            uuid_like()
        );
        let token = name.clone();
        let entrypoint = entrypoint.map(str::to_owned);
        let (stop, stopping) = std::sync::mpsc::channel();
        let (created, created_rx) = tokio::sync::oneshot::channel();
        let (started, started_rx) = tokio::sync::oneshot::channel();
        let owner = std::thread::spawn(move || {
            let mut created = Some(created);
            let mut started = Some(started);
            let mut id = None;
            let setup = (|| {
                if !matches!(
                    stopping.try_recv(),
                    Err(std::sync::mpsc::TryRecvError::Empty)
                ) {
                    return Ok(());
                }
                let label = format!("{}={token}", Self::LABEL);
                let mut arguments = vec![
                    "create",
                    "--pull=missing",
                    "--name",
                    &name,
                    "--label",
                    &label,
                    "--publish",
                    "127.0.0.1::4222",
                ];
                if let Some(entrypoint) = &entrypoint {
                    arguments.extend(["--entrypoint", entrypoint.as_str()]);
                }
                arguments.push("nats:2.10-alpine");
                let created_id = Self::docker(&arguments)?;
                if !Self::valid_id(&created_id) {
                    return Err(
                        "Docker create did not return one immutable container ID".to_owned()
                    );
                }
                id = Some(created_id);
                let cid = id.as_deref().unwrap();
                eprintln!("NATS fixture created name={name} label={token} cid={cid}");
                let acknowledged_at = std::time::Instant::now();
                after_create(cid);
                if created
                    .take()
                    .unwrap()
                    .send(Ok((acknowledged_at, cid.to_owned())))
                    .is_err()
                    || !matches!(
                        stopping.try_recv(),
                        Err(std::sync::mpsc::TryRecvError::Empty)
                    )
                {
                    return Ok(());
                }
                Self::docker(&["start", cid])?;
                let port = Self::docker(&["port", cid, "4222/tcp"])?;
                let address: std::net::SocketAddr = port
                    .parse()
                    .map_err(|e| format!("Docker loopback port: {e}"))?;
                if !address.is_ipv4() || !address.ip().is_loopback() || address.port() == 0 {
                    return Err(
                        "Docker did not publish an IPv4 loopback ephemeral NATS port".to_owned(),
                    );
                }
                if started
                    .take()
                    .unwrap()
                    .send(Ok(format!("nats://{address}")))
                    .is_ok()
                {
                    let _ = stopping.recv();
                }
                Ok(())
            })();
            if let Err(error) = setup {
                if let Some(created) = created {
                    let _ = created.send(Err(error.clone()));
                }
                if let Some(started) = started {
                    let _ = started.send(Err(error.clone()));
                }
                eprintln!("NATS fixture startup failed name={name}: {error}");
            }
            Self::cleanup(&name, &token, id.as_deref())
        });
        Self {
            stop,
            owner: Some(owner),
            created: Some(created_rx),
            started: Some(started_rx),
        }
    }

    fn docker(arguments: &[&str]) -> Result<String, String> {
        let output = docker(arguments).map_err(|e| format!("Docker {}: {e}", arguments[0]))?;
        if !output.status.success() {
            return Err(format!(
                "Docker {} exited {}: {}",
                arguments[0],
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        String::from_utf8(output.stdout)
            .map(|s| s.trim().to_owned())
            .map_err(|e| e.to_string())
    }

    fn valid_id(id: &str) -> bool {
        id.len() == 64
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    fn exists(id: &str) -> Result<bool, String> {
        let filter = format!("id={id}");
        let rows = Self::docker(&[
            "ps",
            "--all",
            "--no-trunc",
            "--filter",
            &filter,
            "--format",
            "{{.ID}}",
        ])?;
        if rows.is_empty() {
            return Ok(false);
        }
        if rows == id {
            return Ok(true);
        }
        Err("Docker returned an unexpected identity for exact container lookup".to_owned())
    }

    fn cleanup(name: &str, token: &str, id: Option<&str>) -> Result<(), String> {
        let label = format!("label={}={token}", Self::LABEL);
        let filter = format!("name=^/{name}$");
        let rows = Self::docker(&[
            "ps",
            "--all",
            "--no-trunc",
            "--filter",
            &filter,
            "--filter",
            &label,
            "--format",
            "{{.ID}}",
        ])?;
        if rows.is_empty() {
            if let Some(id) = id
                && Self::exists(id)?
            {
                return Err(format!(
                    "NATS cleanup retained changed ownership name={name} cid={id}"
                ));
            }
            return Ok(());
        }
        if !Self::valid_id(&rows) || id.is_some_and(|id| id != rows) {
            return Err(format!(
                "NATS cleanup refused a foreign identity name={name}"
            ));
        }
        Self::docker(&["rm", "--force", "--volumes", &rows])?;
        if Self::exists(&rows)? {
            return Err(format!("NATS cleanup did not remove cid={rows}"));
        }
        eprintln!("NATS fixture cleanup confirmed name={name} label={token} cid={rows} absent");
        Ok(())
    }
}

impl Drop for DockerNatsGuard {
    fn drop(&mut self) {
        // Close undelivered replies before joining a pending owner.
        drop(self.created.take());
        drop(self.started.take());
        let _ = self.stop.send(());
        if let Some(owner) = self.owner.take() {
            let result = owner
                .join()
                .unwrap_or_else(|_| Err("NATS owner panicked; cleanup is unproven".to_owned()));
            if let Err(error) = result {
                eprintln!("NATS fixture cleanup failed: {error}");
                if !std::thread::panicking() {
                    panic!("NATS fixture cleanup failed: {error}");
                }
            }
        }
    }
}

impl NatsFixture {
    pub(super) async fn start() -> Self {
        Self::start_observed(None, |_| {})
            .await
            .expect("start owned NATS fixture")
    }

    async fn start_observed(
        entrypoint: Option<&str>,
        after_create: impl FnOnce(&str) + Send + 'static,
    ) -> Result<Self, String> {
        // The guard exists before the first await. Cancelling setup drains
        // its blocking create owner and joins exact cleanup, even before
        // the acknowledgement has been delivered to this future.
        let mut container = DockerNatsGuard::begin(entrypoint, after_create);
        let (created_at, id) = container
            .created
            .take()
            .unwrap()
            .await
            .map_err(|_| "NATS create owner stopped without a result".to_owned())??;
        // testcontainers 0.27.3 async_runner.rs starts this same 60s budget
        // AFTER create/pull acknowledgement; setup retains its original scope.
        let url =
            tokio::time::timeout_at(
                tokio::time::Instant::from_std(created_at) + Duration::from_secs(60),
                async {
                    let url =
                        container.started.take().unwrap().await.map_err(|_| {
                            "NATS start owner stopped without a result".to_owned()
                        })??;
                    // Startup readiness, not a test retry: the existing client
                    // connects within the original deadline and flush awaits
                    // the server's real protocol response before delivery.
                    let client = async_nats::ConnectOptions::new()
                        .retry_on_initial_connect()
                        .connect(url.clone())
                        .await
                        .map_err(|e| e.to_string())?;
                    client.flush().await.map_err(|e| e.to_string())?;
                    client.drain().await.map_err(|e| e.to_string())?;
                    Ok::<_, String>(url)
                },
            )
            .await
            .map_err(|_| "NATS readiness exceeded the original 60s budget".to_owned())??;
        Ok(Self {
            _container: container,
            id,
            url,
        })
    }
}

fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{}", NEXT.fetch_add(1, Ordering::Relaxed))
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
        let first_id = first.id.clone();
        let second_id = second.id.clone();
        assert!(running(&first_id) && running(&second_id));
        drop(first);
        assert!(!running(&first_id), "drop must remove its own container");
        assert!(running(&second_id), "another fixture must remain running");
        // The fixture is moved into the unwind and never observed afterwards.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = second;
            panic!("exercise fixture cleanup after a test failure");
        }));
        assert!(result.is_err());
        assert!(
            !running(&second_id),
            "unwind must also remove the owned container"
        );
    }
    #[tokio::test]
    async fn canceled_startup_drains_the_created_container_before_reply_delivery() {
        // L1 actual Docker create has acknowledged an immutable CID, but the
        // startup reply is held. Drop the real pending start future: its guard
        // must drain/join the owner and prove that exact CID absent. The hook
        // controls scheduling only; it fabricates neither a container nor ACK.
        // L2 startup failure and readiness timeout use the same guard/finalizer;
        // the adjacent OCI failure covers early error, while the inherited 60s
        // timer never substitutes for a cleanup acknowledgement.
        use std::future::Future as _;
        let (seen, observed) = tokio::sync::oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let mut starting = Box::pin(NatsFixture::start_observed(None, move |id| {
            let _ = seen.send(id.to_owned());
            let _ = held.recv();
        }));
        std::future::poll_fn(|cx| {
            assert!(starting.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let id = observed.await.unwrap();
        // Release the schedule gate before any fallible assertion, so an
        // assertion unwind can still drain/join the owner instead of deadlock.
        release.send(()).unwrap();
        let was_present = DockerNatsGuard::exists(&id).unwrap();
        drop(starting);
        assert!(was_present, "L1 actual created target");
        assert!(
            !DockerNatsGuard::exists(&id).unwrap(),
            "L1 joined exact cleanup"
        );
    }

    #[tokio::test]
    async fn early_start_failure_cleans_only_its_created_container() {
        // L3 the real image is created, but an intentionally missing entrypoint
        // makes Docker start fail before any NATS handshake. The original setup
        // error remains an error and the same guard synchronously confirms the
        // owned CID absent; no server/foreign-container cleanup is inferred.
        let (seen, observed) = tokio::sync::oneshot::channel();
        let result = NatsFixture::start_observed(
            Some("/foundation-fixture-intentionally-missing"),
            move |id| {
                let _ = seen.send(id.to_owned());
            },
        )
        .await;
        assert!(result.is_err(), "L3 the OCI start must fail");
        let id = observed.await.unwrap();
        assert!(
            !DockerNatsGuard::exists(&id).unwrap(),
            "L3 joined exact cleanup"
        );
    }
}
