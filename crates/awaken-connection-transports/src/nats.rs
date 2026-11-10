//! NATS mediated byte-duplex channel.
//!
//! This module keeps NATS as a transport substrate only. NATS subjects carry
//! opaque ordered byte chunks for one binding direction; they do not become a
//! brain<->hand or connector protocol.

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll};

use async_nats::{Client, HeaderMap, client::FlushError};
use async_trait::async_trait;
use awaken_connection::{Channel, ConnectError, Transport};
use awaken_connection_auth::HeaderAuthMaterial;
use bytes::Bytes;
use futures::{StreamExt as _, ready};
use opentelemetry::{
    Context as OtelContext, global,
    propagation::{Extractor, Injector},
};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{Instrument as _, Span, info_span};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

/// Error returned while establishing a NATS-mediated channel.
#[derive(Debug, Error)]
pub enum NatsChannelError {
    /// NATS subscription failed.
    #[error("failed to subscribe to NATS subject '{subject}': {source}")]
    Subscribe {
        /// Subject this side reads from.
        subject: String,
        /// Underlying NATS error.
        source: async_nats::SubscribeError,
    },
    /// NATS subscription could not be flushed to the server.
    #[error("failed to flush NATS subscription for subject '{subject}': {source}")]
    Flush {
        /// Subject this side reads from.
        subject: String,
        /// Underlying NATS error.
        source: FlushError,
    },
}

/// A byte-duplex over two NATS subjects.
///
/// `inbox_subject` is subscribed by this endpoint and `outbox_subject` is
/// published by this endpoint. The peer must use the same pair reversed. The
/// channel implements byte-stream semantics by concatenating received message
/// payloads; upper framing remains the caller's responsibility.
pub struct NatsDuplex {
    inbound: mpsc::UnboundedReceiver<Bytes>,
    inbound_buf: VecDeque<u8>,
    outbound: mpsc::UnboundedSender<OutboundChunk>,
    pending_publishes: VecDeque<oneshot::Receiver<Result<(), String>>>,
    closed: Arc<AtomicBool>,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl Channel for NatsDuplex {}

/// Address for one side of a NATS mediated byte-duplex channel.
///
/// `inbox_subject` is subscribed by this endpoint and `outbox_subject` is
/// published by this endpoint. The peer must use the same pair reversed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NatsAddress {
    /// NATS server URL.
    pub url: String,
    /// Subject this side reads from.
    pub inbox_subject: String,
    /// Subject this side publishes to.
    pub outbox_subject: String,
}

impl NatsAddress {
    /// Build a NATS address.
    #[must_use]
    pub fn new(
        url: impl Into<String>,
        inbox_subject: impl Into<String>,
        outbox_subject: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            inbox_subject: inbox_subject.into(),
            outbox_subject: outbox_subject.into(),
        }
    }
}

/// NATS transport implementation for the neutral connection seam.
///
/// NATS remains a transport detail: callers provide a resolved [`NatsAddress`]
/// and receive a byte-duplex channel.
#[derive(Debug, Clone, Default)]
pub struct NatsTransport;

#[async_trait]
impl Transport for NatsTransport {
    type Channel = NatsDuplex;
    type Address = NatsAddress;
    type HandshakeMaterial = HeaderAuthMaterial;

    fn scheme(&self) -> &str {
        "nats"
    }

    async fn dial(
        &self,
        addr: NatsAddress,
        _material: &HeaderAuthMaterial,
    ) -> Result<Self::Channel, ConnectError> {
        let NatsAddress {
            url,
            inbox_subject,
            outbox_subject,
        } = addr;
        let client = async_nats::connect(url)
            .await
            .map_err(|error| ConnectError::Setup(format!("failed to connect NATS: {error}")))?;
        Self::Channel::connect(client, inbox_subject, outbox_subject)
            .await
            .map_err(|error| ConnectError::Setup(error.to_string()))
    }
}

struct OutboundChunk {
    payload: Bytes,
    ack: oneshot::Sender<Result<(), String>>,
    parent_span: Span,
}

struct NatsHeadersInjector<'a> {
    headers: &'a mut HeaderMap,
}

impl Injector for NatsHeadersInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        self.headers.insert(key, value.as_str());
    }
}

struct NatsHeadersExtractor<'a> {
    headers: &'a HeaderMap,
}

impl Extractor for NatsHeadersExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.headers.get(key).map(|value| value.as_str())
    }

    fn keys(&self) -> Vec<&str> {
        self.headers.iter().map(|(key, _)| key.as_ref()).collect()
    }
}

fn inject_trace_context(headers: &mut HeaderMap, context: &OtelContext) {
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(context, &mut NatsHeadersInjector { headers });
    });
}

fn extract_trace_context(headers: Option<&HeaderMap>) -> OtelContext {
    let Some(headers) = headers else {
        return OtelContext::new();
    };
    global::get_text_map_propagator(|propagator| {
        propagator.extract(&NatsHeadersExtractor { headers })
    })
}

fn traceparent(headers: Option<&HeaderMap>) -> Option<&str> {
    headers.and_then(|headers| headers.get("traceparent").map(|value| value.as_str()))
}

impl NatsDuplex {
    /// Connect one side of a mediated channel.
    pub async fn connect(
        client: Client,
        inbox_subject: impl Into<String>,
        outbox_subject: impl Into<String>,
    ) -> Result<Self, NatsChannelError> {
        let inbox_subject = inbox_subject.into();
        let outbox_subject = outbox_subject.into();
        let mut subscriber = client
            .subscribe(inbox_subject.clone())
            .await
            .map_err(|source| NatsChannelError::Subscribe {
                subject: inbox_subject.clone(),
                source,
            })?;
        client
            .flush()
            .await
            .map_err(|source| NatsChannelError::Flush {
                subject: inbox_subject.clone(),
                source,
            })?;

        let (inbound_tx, inbound) = mpsc::unbounded_channel();
        let (outbound, mut outbound_rx) = mpsc::unbounded_channel::<OutboundChunk>();
        let closed = Arc::new(AtomicBool::new(false));

        let reader_closed = closed.clone();
        let reader = tokio::spawn(async move {
            while let Some(message) = subscriber.next().await {
                let payload_len = message.payload.len() as u64;
                let trace_context = traceparent(message.headers.as_ref()).unwrap_or("absent");
                let span = info_span!(
                    target: "awaken::connection_transports::nats",
                    "nats.consume",
                    messaging.system = "nats",
                    messaging.destination = %inbox_subject,
                    messaging.operation = "consume",
                    trace_context = %trace_context,
                    payload_bytes = payload_len,
                );
                span.set_parent(extract_trace_context(message.headers.as_ref()));
                let send_result = async { inbound_tx.send(message.payload) }
                    .instrument(span)
                    .await;
                if send_result.is_err() {
                    break;
                }
            }
            reader_closed.store(true, Ordering::Release);
        });

        let writer_closed = closed.clone();
        let writer_client = client;
        let writer = tokio::spawn(async move {
            while let Some(chunk) = outbound_rx.recv().await {
                let mut headers = HeaderMap::new();
                let payload_len = chunk.payload.len() as u64;
                let span = info_span!(
                    target: "awaken::connection_transports::nats",
                    parent: &chunk.parent_span,
                    "nats.publish",
                    messaging.system = "nats",
                    messaging.destination = %outbox_subject,
                    messaging.operation = "publish",
                    trace_context = tracing::field::Empty,
                    payload_bytes = payload_len,
                );
                inject_trace_context(&mut headers, &span.context());
                span.record(
                    "trace_context",
                    traceparent(Some(&headers)).unwrap_or("absent"),
                );
                let publish_result = writer_client
                    .publish_with_headers(outbox_subject.clone(), headers, chunk.payload)
                    .instrument(span)
                    .await;
                if publish_result.is_err() {
                    let _ = chunk
                        .ack
                        .send(Err("failed to publish to NATS subject".to_string()));
                    writer_closed.store(true, Ordering::Release);
                    break;
                }
                let _ = chunk.ack.send(Ok(()));
            }
        });

        Ok(Self {
            inbound,
            inbound_buf: VecDeque::new(),
            outbound,
            pending_publishes: VecDeque::new(),
            closed,
            reader,
            writer,
        })
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn poll_pending_publishes(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while let Some(receiver) = self.pending_publishes.front_mut() {
            match ready!(Pin::new(receiver).poll(cx)) {
                Ok(Ok(())) => {
                    self.pending_publishes.pop_front();
                }
                Ok(Err(error)) => {
                    self.closed.store(true, Ordering::Release);
                    return Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, error)));
                }
                Err(_) => {
                    self.closed.store(true, Ordering::Release);
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "NATS writer stopped before publish completed",
                    )));
                }
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl Drop for NatsDuplex {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        self.reader.abort();
        self.writer.abort();
    }
}

impl AsyncRead for NatsDuplex {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.inbound_buf.is_empty() {
            match Pin::new(&mut self.inbound).poll_recv(cx) {
                Poll::Ready(Some(bytes)) => self.inbound_buf.extend(bytes),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }

        while buf.remaining() > 0 {
            let Some(byte) = self.inbound_buf.pop_front() else {
                break;
            };
            buf.put_slice(&[byte]);
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for NatsDuplex {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.is_closed() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "NATS mediated channel is closed",
            )));
        }
        if data.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let (ack, pending) = oneshot::channel();
        self.outbound
            .send(OutboundChunk {
                payload: Bytes::copy_from_slice(data),
                ack,
                parent_span: Span::current(),
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "NATS writer closed"))?;
        self.get_mut().pending_publishes.push_back(pending);
        Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.as_mut().get_mut().poll_pending_publishes(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.as_mut().poll_flush(cx))?;
        self.closed.store(true, Ordering::Release);
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_connection::Transport;
    use awaken_connection_auth::HeaderAuthMaterial;
    use futures::future::BoxFuture;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::{
        error::OTelSdkResult,
        propagation::TraceContextPropagator,
        trace::{SdkTracerProvider, SpanData, SpanExporter},
    };
    use std::sync::{Arc, Mutex, OnceLock};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::time::{Duration, timeout};
    use tracing_subscriber::prelude::*;

    struct NatsFixture {
        _container: DockerNatsGuard,
        url: String,
    }

    // The original fixture remains the only lifecycle owner. A std thread
    // drains Docker setup before cleanup: killing a CLI future cannot prove
    // that the daemon did not create a late container. No extra Tokio runtime
    // or global reaper survives the test. SIGKILL/daemon loss are not guaranteed.
    struct DockerNatsGuard {
        stop: std::sync::mpsc::Sender<()>,
        owner: Option<std::thread::JoinHandle<Result<(), String>>>,
        created: Option<tokio::sync::oneshot::Receiver<Result<std::time::Instant, String>>>,
        started: Option<tokio::sync::oneshot::Receiver<Result<String, String>>>,
    }

    impl DockerNatsGuard {
        const LABEL: &str = "awaken.foundation.nats-fixture";

        fn begin(
            entrypoint: Option<&str>,
            after_create: impl FnOnce(&str) + Send + 'static,
        ) -> Self {
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
                    if created.take().unwrap().send(Ok(acknowledged_at)).is_err()
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
                            "Docker did not publish an IPv4 loopback ephemeral NATS port"
                                .to_owned(),
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
            let output = std::process::Command::new("docker")
                .args(arguments)
                .output()
                .map_err(|e| format!("Docker {}: {e}", arguments[0]))?;
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

    #[derive(Clone, Debug, Default)]
    struct TestSpanExporter {
        spans: Arc<Mutex<Vec<SpanData>>>,
    }

    impl TestSpanExporter {
        fn reset(&self) {
            self.spans.lock().expect("span export lock").clear();
        }

        fn finished_spans(&self) -> Vec<SpanData> {
            self.spans.lock().expect("span export lock").clone()
        }
    }

    impl SpanExporter for TestSpanExporter {
        fn export(&mut self, mut batch: Vec<SpanData>) -> BoxFuture<'static, OTelSdkResult> {
            let spans = self.spans.clone();
            Box::pin(async move {
                spans.lock().expect("span export lock").append(&mut batch);
                Ok(())
            })
        }
    }

    impl NatsFixture {
        async fn start() -> Self {
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
            let created_at = container
                .created
                .take()
                .unwrap()
                .await
                .map_err(|_| "NATS create owner stopped without a result".to_owned())??;
            // testcontainers 0.27.3 async_runner.rs starts this same 60s budget
            // AFTER create/pull acknowledgement; setup retains its original scope.
            let url = tokio::time::timeout_at(
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
                url,
            })
        }
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

    #[tokio::test]
    async fn byte_duplex_round_trips_over_reversed_subject_pair() {
        let fixture = NatsFixture::start().await;
        let client_a = async_nats::connect(fixture.url.clone())
            .await
            .expect("side a nats client");
        let client_b = async_nats::connect(fixture.url)
            .await
            .expect("side b nats client");
        let prefix = format!("awaken.test.{}", uuid_like());
        let a_to_b = format!("{prefix}.a_to_b");
        let b_to_a = format!("{prefix}.b_to_a");

        let mut side_a = NatsDuplex::connect(client_a, b_to_a.clone(), a_to_b.clone())
            .await
            .expect("side a duplex");
        let mut side_b = NatsDuplex::connect(client_b, a_to_b, b_to_a)
            .await
            .expect("side b duplex");

        side_a.write_all(b"hello from a").await.expect("write a");
        side_a.flush().await.expect("flush a");
        let mut buffer = vec![0_u8; 12];
        timeout(Duration::from_secs(5), side_b.read_exact(&mut buffer))
            .await
            .expect("read b timeout")
            .expect("read b");
        assert_eq!(buffer, b"hello from a");

        side_b.write_all(b"hello from b").await.expect("write b");
        side_b.flush().await.expect("flush b");
        let mut buffer = vec![0_u8; 12];
        timeout(Duration::from_secs(5), side_a.read_exact(&mut buffer))
            .await
            .expect("read a timeout")
            .expect("read a");
        assert_eq!(buffer, b"hello from b");
    }

    #[tokio::test]
    async fn nats_transport_dials_from_typed_address() {
        let fixture = NatsFixture::start().await;
        let prefix = format!("awaken.test.transport.{}", uuid_like());
        let a_to_b = format!("{prefix}.a_to_b");
        let b_to_a = format!("{prefix}.b_to_a");
        let transport = NatsTransport;
        let material = HeaderAuthMaterial::none();

        let mut side_a = transport
            .dial(
                NatsAddress::new(fixture.url.clone(), b_to_a.clone(), a_to_b.clone()),
                &material,
            )
            .await
            .expect("side a transport dial");
        let mut side_b = transport
            .dial(NatsAddress::new(fixture.url, a_to_b, b_to_a), &material)
            .await
            .expect("side b transport dial");

        side_a.write_all(b"transport").await.expect("write");
        side_a.flush().await.expect("flush");
        let mut buffer = vec![0_u8; 9];
        timeout(Duration::from_secs(5), side_b.read_exact(&mut buffer))
            .await
            .expect("read timeout")
            .expect("read");
        assert_eq!(buffer, b"transport");
    }

    #[tokio::test]
    async fn nats_transport_reports_unreachable_broker_as_setup_error() {
        let result = NatsTransport
            .dial(
                NatsAddress::new("nats://127.0.0.1:9", "inbox", "outbox"),
                &HeaderAuthMaterial::none(),
            )
            .await;
        let Err(error) = result else {
            panic!("dialing an unreachable NATS broker must fail");
        };
        assert!(matches!(error, ConnectError::Setup(_)));
    }

    #[tokio::test]
    async fn nats_duplex_publishes_w3c_trace_context_headers() {
        let (_provider, exporter) = install_trace_context_propagation();
        exporter.reset();
        let fixture = NatsFixture::start().await;
        let observer = async_nats::connect(fixture.url.clone())
            .await
            .expect("observer nats client");
        let writer_client = async_nats::connect(fixture.url)
            .await
            .expect("writer nats client");
        let subject = format!("awaken.test.trace.{}", uuid_like());
        let mut observed = observer
            .subscribe(subject.clone())
            .await
            .expect("observe subject");
        observer.flush().await.expect("flush observer subscription");

        let mut duplex = NatsDuplex::connect(
            writer_client,
            format!("{subject}.unused_inbox"),
            subject.clone(),
        )
        .await
        .expect("duplex");

        let payload = b"trace-context-payload".as_slice();
        async {
            duplex.write_all(payload).await.expect("write payload");
            duplex.flush().await.expect("flush publish");
        }
        .instrument(tracing::info_span!("nats.trace.root"))
        .await;

        let message = timeout(Duration::from_secs(5), observed.next())
            .await
            .expect("message timeout")
            .expect("observed message");
        assert_eq!(message.payload.as_ref(), payload);
        let traceparent = message
            .headers
            .as_ref()
            .and_then(|headers| headers.get("traceparent"))
            .map(|value| value.as_str())
            .expect("traceparent header");
        assert_w3c_traceparent(traceparent);
    }

    #[tokio::test]
    async fn nats_duplex_exports_publish_and_consume_spans_with_same_trace_id() {
        let (provider, exporter) = install_trace_context_propagation();
        exporter.reset();
        let fixture = NatsFixture::start().await;
        let client_a = async_nats::connect(fixture.url.clone())
            .await
            .expect("brain nats client");
        let client_b = async_nats::connect(fixture.url)
            .await
            .expect("hand nats client");
        let prefix = format!("awaken.test.trace.spans.{}", uuid_like());
        let brain_to_hand = format!("{prefix}.brain_to_hand");
        let hand_to_brain = format!("{prefix}.hand_to_brain");
        let mut brain = NatsDuplex::connect(client_a, hand_to_brain.clone(), brain_to_hand.clone())
            .await
            .expect("brain duplex");
        let mut hand = NatsDuplex::connect(client_b, brain_to_hand.clone(), hand_to_brain)
            .await
            .expect("hand duplex");

        async {
            brain.write_all(b"span-link").await.expect("write payload");
            brain.flush().await.expect("flush publish");
            let mut buffer = [0_u8; 9];
            hand.read_exact(&mut buffer).await.expect("read payload");
            assert_eq!(&buffer, b"span-link");
        }
        .instrument(tracing::info_span!("nats.trace.root"))
        .await;

        provider.force_flush().expect("flush spans");
        let spans = exporter.finished_spans();
        let publish = spans
            .iter()
            .find(|span| {
                span.name == "nats.publish"
                    && span_has_attribute(span, "messaging.destination", &brain_to_hand)
            })
            .expect("publish span");
        let consume = spans
            .iter()
            .find(|span| {
                span.name == "nats.consume"
                    && span_has_attribute(span, "messaging.destination", &brain_to_hand)
            })
            .expect("consume span");
        assert_eq!(
            publish.span_context.trace_id(),
            consume.span_context.trace_id(),
            "consumer span must continue the producer trace"
        );
        assert_eq!(
            publish.span_context.span_id(),
            consume.parent_span_id,
            "consumer parent must be the propagated producer span"
        );
    }

    fn uuid_like() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        format!("{}", NEXT.fetch_add(1, Ordering::Relaxed))
    }

    fn install_trace_context_propagation() -> (&'static SdkTracerProvider, &'static TestSpanExporter)
    {
        static OTEL: OnceLock<(SdkTracerProvider, TestSpanExporter)> = OnceLock::new();
        let (provider, exporter) = OTEL.get_or_init(|| {
            opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
            let exporter = TestSpanExporter::default();
            let provider = SdkTracerProvider::builder()
                .with_simple_exporter(exporter.clone())
                .build();
            let tracer = provider.tracer("awaken-connection-transports-test");
            let subscriber = tracing_subscriber::registry()
                .with(tracing_opentelemetry::layer().with_tracer(tracer));
            let _ = tracing::subscriber::set_global_default(subscriber);
            (provider, exporter)
        });
        (provider, exporter)
    }

    fn assert_w3c_traceparent(traceparent: &str) {
        let parts = traceparent.split('-').collect::<Vec<_>>();
        assert_eq!(parts.len(), 4, "traceparent must have four fields");
        assert_eq!(parts[0], "00", "traceparent version");
        assert_eq!(parts[1].len(), 32, "trace id length");
        assert_ne!(parts[1], "00000000000000000000000000000000", "trace id");
        assert_eq!(parts[2].len(), 16, "span id length");
        assert_ne!(parts[2], "0000000000000000", "span id");
        assert_eq!(parts[3].len(), 2, "trace flags length");
        for part in parts {
            assert!(
                part.chars().all(|character| character.is_ascii_hexdigit()),
                "traceparent part must be hex: {part}"
            );
        }
    }

    fn span_has_attribute(
        span: &opentelemetry_sdk::trace::SpanData,
        key: &str,
        value: &str,
    ) -> bool {
        span.attributes
            .iter()
            .any(|attribute| attribute.key.as_str() == key && attribute.value.to_string() == value)
    }
}
