//! Reusable transport adapters built on `awaken-connection`.
//!
//! These adapters consume typed addresses and caller-owned handshake material.
//! They do not resolve logical connector references, fetch credentials, refresh
//! OAuth tokens, decide authorization, or perform lease checks.

pub mod http;
#[cfg(feature = "nats")]
pub mod nats;

pub use http::{
    HttpAddress, HttpChannel, HttpRequest, HttpResponse, HttpTransport, HttpTransportError,
};
#[cfg(feature = "nats")]
pub use nats::{NatsAddress, NatsChannelError, NatsDuplex, NatsTransport};
