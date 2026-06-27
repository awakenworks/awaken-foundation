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
    use testcontainers::{ContainerAsync, GenericImage, core::WaitFor, runners::AsyncRunner};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::time::{Duration, sleep, timeout};
    use tracing_subscriber::prelude::*;

    struct NatsFixture {
        _container: ContainerAsync<GenericImage>,
        url: String,
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
            let image = GenericImage::new("nats", "2.10-alpine")
                .with_wait_for(WaitFor::message_on_stderr("Server is ready"));
            let container = image.start().await.expect("failed to start nats container");
            let host_port = container.get_host_port_ipv4(4222).await.expect("nats port");
            let url = format!("nats://127.0.0.1:{host_port}");
            sleep(Duration::from_millis(250)).await;
            Self {
                _container: container,
                url,
            }
        }
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
