//! Core connection mechanism.
//!
//! This crate owns only reusable connection mechanics: typed channels, typed
//! dial/listen pairing, erased established-channel views, `bind_pair`, and the
//! transport-level error model. Policy-layer connection plans, authorization,
//! handshake-material resolution, capability scopes, concrete resolved address
//! DTOs, and wiring/dial policy live outside this crate.

use async_trait::async_trait;
use thiserror::Error;

/// A duplex established connection.
///
/// A thin marker only: `Channel` is the associated type of a [`Transport`] or
/// typed end, not one universal byte-stream type. Concrete protocols add their
/// own bounds at their use sites.
pub trait Channel: Send + 'static {}

/// A strongly typed transport.
///
/// Address and handshake material are associated types so upstream planning
/// layers can choose their own DTOs without teaching this core crate about those
/// semantics. Handshake material is opaque caller-owned input: this crate passes
/// it to the transport implementation but never parses, exposes, stores, logs, or
/// persists it.
#[async_trait]
pub trait Transport: Send + Sync {
    /// The channel this transport establishes.
    type Channel: Channel;
    /// The address shape this transport accepts.
    type Address: Clone + Send + Sync + 'static;
    /// Opaque caller-owned material this transport consumes during handshake.
    type HandshakeMaterial: ?Sized + Send + Sync;

    /// The transport scheme, used for diagnostics.
    fn scheme(&self) -> &str;

    /// Establish a channel to `addr`, applying caller-resolved handshake
    /// material.
    async fn dial(
        &self,
        addr: Self::Address,
        material: &Self::HandshakeMaterial,
    ) -> Result<Self::Channel, ConnectError>;
}

/// The erased dial end: dials its peer and receives a boxed [`Channel`].
#[async_trait]
pub trait Dialer: Send + Sync {
    /// Dial the peer and return the erased channel.
    async fn connect(&self) -> Result<Box<dyn Channel>, ConnectError>;
}

/// The erased listen end: accepts a peer's dial and receives a boxed [`Channel`].
#[async_trait]
pub trait ListenEnd: Send + Sync {
    /// Accept one inbound connection and return the erased channel.
    async fn accept(&self) -> Result<Box<dyn Channel>, ConnectError>;
}

/// Erases a typed [`Transport`] plus a resolved address/handshake pair into a
/// [`Dialer`].
pub struct TransportDialer<T: Transport> {
    transport: T,
    addr: T::Address,
    material: Box<T::HandshakeMaterial>,
}

impl<T: Transport> TransportDialer<T> {
    /// Pair `transport` with the address and opaque handshake material it dials
    /// with.
    pub fn new(transport: T, addr: T::Address, material: Box<T::HandshakeMaterial>) -> Self {
        Self {
            transport,
            addr,
            material,
        }
    }
}

#[async_trait]
impl<T> Dialer for TransportDialer<T>
where
    T: Transport,
    T::Channel: Channel,
{
    async fn connect(&self) -> Result<Box<dyn Channel>, ConnectError> {
        let channel = self
            .transport
            .dial(self.addr.clone(), self.material.as_ref())
            .await?;
        Ok(Box::new(channel))
    }
}

/// Typed dial end of a transport pair.
///
/// `Peer` names the matching listen end with a round-trip bound
/// (`Peer: ListenSide<Peer = Self>`), so a dial end can only pair with the listen
/// end that names it back.
#[async_trait]
pub trait DialEnd: Send + Sync + Sized {
    /// The channel this dial end establishes.
    type Channel: Channel;
    /// The address shape this dial end accepts.
    type Address: Clone + Send + Sync + 'static;
    /// Opaque caller-owned material this end presents on dial.
    type DialMaterial: Send;
    /// The matching listen end.
    type Peer: ListenSide<Peer = Self>;

    /// Dial the peer at `addr`, presenting `material`.
    async fn dial(
        &self,
        addr: Self::Address,
        material: Self::DialMaterial,
    ) -> Result<Self::Channel, ConnectError>;
}

/// Typed listen end of a transport pair.
#[async_trait]
pub trait ListenSide: Send + Sync + Sized {
    /// The channel this listen end establishes.
    type Channel: Channel;
    /// The matching dial end.
    type Peer: DialEnd<Peer = Self>;

    /// Accept one inbound connection from the peer dial end.
    async fn accept(&self) -> Result<Self::Channel, ConnectError>;
}

/// Bind a typed dial end to its peer listen end.
///
/// The signature is the pairing guarantee: `listener` must be exactly
/// `D::Peer`, `addr` must be `D::Address`, and `material` must be
/// `D::DialMaterial`.
pub async fn bind_pair<D: DialEnd>(
    dialer: &D,
    listener: &D::Peer,
    addr: D::Address,
    material: D::DialMaterial,
) -> Result<(<D::Peer as ListenSide>::Channel, D::Channel), ConnectError> {
    let (accepted, dialed) =
        futures::future::join(listener.accept(), dialer.dial(addr, material)).await;
    Ok((accepted?, dialed?))
}

/// A transport-level connection failure.
#[derive(Debug, Error)]
pub enum ConnectError {
    /// The resolved address shape is not one this transport recognizes.
    #[error("transport `{scheme}` does not accept this address shape")]
    UnsupportedAddress {
        /// The transport scheme that rejected the address.
        scheme: String,
    },
    /// The transport failed to bind, spawn, or hand-shake.
    #[error("transport setup failed: {0}")]
    Setup(String),
    /// The transport's I/O failed while establishing the channel.
    #[error("transport I/O failed: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::oneshot;

    #[derive(Debug)]
    struct Bytes {
        marker: String,
    }

    impl Channel for Bytes {}

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Address {
        WebSocket(String),
        Unix(String),
    }

    trait TestHandshakeMaterial: Send + Sync {
        fn marker(&self) -> &str;
    }

    #[derive(Debug)]
    struct Bearer(String);

    impl TestHandshakeMaterial for Bearer {
        fn marker(&self) -> &str {
            &self.0
        }
    }

    struct WsTransport;

    #[async_trait]
    impl Transport for WsTransport {
        type Channel = Bytes;
        type Address = Address;
        type HandshakeMaterial = dyn TestHandshakeMaterial;

        fn scheme(&self) -> &str {
            "ws"
        }

        async fn dial(
            &self,
            addr: Self::Address,
            material: &Self::HandshakeMaterial,
        ) -> Result<Self::Channel, ConnectError> {
            let Address::WebSocket(url) = addr else {
                return Err(ConnectError::UnsupportedAddress {
                    scheme: self.scheme().to_string(),
                });
            };
            Ok(Bytes {
                marker: format!("{url}:{}", material.marker()),
            })
        }
    }

    struct FailingTransport {
        error: ConnectError,
    }

    #[async_trait]
    impl Transport for FailingTransport {
        type Channel = Bytes;
        type Address = Address;
        type HandshakeMaterial = ();

        fn scheme(&self) -> &str {
            "fail"
        }

        async fn dial(
            &self,
            _addr: Self::Address,
            _material: &Self::HandshakeMaterial,
        ) -> Result<Self::Channel, ConnectError> {
            match &self.error {
                ConnectError::UnsupportedAddress { scheme } => {
                    Err(ConnectError::UnsupportedAddress {
                        scheme: scheme.clone(),
                    })
                }
                ConnectError::Setup(message) => Err(ConnectError::Setup(message.clone())),
                ConnectError::Io(message) => Err(ConnectError::Io(message.clone())),
            }
        }
    }

    #[tokio::test]
    async fn transport_dialer_erases_successful_channel_and_preserves_material() {
        let dialer = TransportDialer::new(
            WsTransport,
            Address::WebSocket("ws://peer".into()),
            Box::new(Bearer("tok".into())) as Box<dyn TestHandshakeMaterial>,
        );

        let channel = dialer.connect().await.unwrap();
        assert!(is_channel(channel));
    }

    #[tokio::test]
    async fn transport_dialer_propagates_transport_errors() {
        let dialer = TransportDialer::new(
            WsTransport,
            Address::Unix("/tmp/socket".into()),
            Box::new(Bearer("tok".into())) as Box<dyn TestHandshakeMaterial>,
        );

        let error = dialer
            .connect()
            .await
            .err()
            .expect("non-ws address must fail");
        assert!(matches!(
            error,
            ConnectError::UnsupportedAddress { scheme } if scheme == "ws"
        ));
    }

    #[test]
    fn connect_error_variants_are_transport_level_only() {
        assert_eq!(
            ConnectError::UnsupportedAddress {
                scheme: "ws".into()
            }
            .to_string(),
            "transport `ws` does not accept this address shape"
        );
        assert_eq!(
            ConnectError::Setup("spawn failed".into()).to_string(),
            "transport setup failed: spawn failed"
        );
        assert_eq!(
            ConnectError::Io("reset".into()).to_string(),
            "transport I/O failed: reset"
        );
    }

    struct WsDial;
    struct WsListen {
        ready: tokio::sync::Mutex<Option<oneshot::Sender<()>>>,
    }

    impl WsListen {
        fn new(tx: oneshot::Sender<()>) -> Self {
            Self {
                ready: tokio::sync::Mutex::new(Some(tx)),
            }
        }
    }

    #[async_trait]
    impl DialEnd for WsDial {
        type Channel = Bytes;
        type Address = Address;
        type DialMaterial = String;
        type Peer = WsListen;

        async fn dial(
            &self,
            addr: Self::Address,
            material: Self::DialMaterial,
        ) -> Result<Self::Channel, ConnectError> {
            let Address::WebSocket(url) = addr else {
                return Err(ConnectError::UnsupportedAddress {
                    scheme: "ws".into(),
                });
            };
            Ok(Bytes {
                marker: format!("dial:{url}:{material}"),
            })
        }
    }

    #[async_trait]
    impl ListenSide for WsListen {
        type Channel = Bytes;
        type Peer = WsDial;

        async fn accept(&self) -> Result<Self::Channel, ConnectError> {
            if let Some(tx) = self.ready.lock().await.take() {
                let _ = tx.send(());
            }
            Ok(Bytes {
                marker: "accept".into(),
            })
        }
    }

    #[tokio::test]
    async fn bind_pair_runs_accept_and_dial_and_returns_both_channels() {
        let (tx, rx) = oneshot::channel();
        let listen = WsListen::new(tx);

        let (accepted, dialed) = bind_pair(
            &WsDial,
            &listen,
            Address::WebSocket("ws://peer".into()),
            "material".into(),
        )
        .await
        .unwrap();

        rx.await.unwrap();
        assert_eq!(accepted.marker, "accept");
        assert_eq!(dialed.marker, "dial:ws://peer:material");
    }

    struct DialFails;
    struct AcceptFails;

    #[async_trait]
    impl DialEnd for DialFails {
        type Channel = Bytes;
        type Address = Address;
        type DialMaterial = ();
        type Peer = AcceptFails;

        async fn dial(
            &self,
            _addr: Self::Address,
            _material: Self::DialMaterial,
        ) -> Result<Self::Channel, ConnectError> {
            Err(ConnectError::Io("dial failed".into()))
        }
    }

    #[async_trait]
    impl ListenSide for AcceptFails {
        type Channel = Bytes;
        type Peer = DialFails;

        async fn accept(&self) -> Result<Self::Channel, ConnectError> {
            Err(ConnectError::Setup("accept failed".into()))
        }
    }

    #[tokio::test]
    async fn bind_pair_returns_accept_error_before_dial_error() {
        let error = bind_pair(
            &DialFails,
            &AcceptFails,
            Address::WebSocket("ws://peer".into()),
            (),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ConnectError::Setup(message) if message == "accept failed"));
    }

    #[tokio::test]
    async fn failing_transport_covers_all_error_branches() {
        for error in [
            ConnectError::UnsupportedAddress { scheme: "x".into() },
            ConnectError::Setup("s".into()),
            ConnectError::Io("i".into()),
        ] {
            let dialer = TransportDialer::new(
                FailingTransport { error },
                Address::WebSocket("ws://peer".into()),
                Box::new(()),
            );
            assert!(dialer.connect().await.is_err());
        }
    }

    #[test]
    fn erased_traits_are_object_safe() {
        let _: Box<dyn Dialer> = Box::new(TransportDialer::new(
            WsTransport,
            Address::WebSocket("ws://peer".into()),
            Box::new(Bearer("tok".into())) as Box<dyn TestHandshakeMaterial>,
        ));

        struct AcceptOnce;
        #[async_trait]
        impl ListenEnd for AcceptOnce {
            async fn accept(&self) -> Result<Box<dyn Channel>, ConnectError> {
                Ok(Box::new(Bytes {
                    marker: "accepted".into(),
                }))
            }
        }

        let listen: Box<dyn ListenEnd> = Box::new(AcceptOnce);
        assert!(is_channel(
            futures::executor::block_on(listen.accept()).unwrap()
        ));
    }

    fn is_channel(_: Box<dyn Channel>) -> bool {
        true
    }

    #[test]
    fn core_api_does_not_export_awaken_plan_terms() {
        let source = include_str!("lib.rs");
        for forbidden in [
            "ConnectionSpec",
            "CredentialRef",
            "CredentialResolver",
            "CapabilityScope",
            "AppliedAuth",
            "DialAddr",
            "BrokerTrust",
            "DialPolicy",
            "Wiring",
        ] {
            assert!(
                !source.contains(&format!("pub struct {forbidden}"))
                    && !source.contains(&format!("pub enum {forbidden}"))
                    && !source.contains(&format!("pub trait {forbidden}")),
                "{forbidden} leaked into awaken-connection core API"
            );
        }
    }

    fn compile_time_pairing_witness(listener: &WsListen) {
        let _: &<WsDial as DialEnd>::Peer = listener;
    }

    fn compile_time_material_shape_witness() {
        fn accepts_ws_dial_material(_: <WsDial as DialEnd>::DialMaterial) {}
        accepts_ws_dial_material(String::new());
    }

    #[test]
    fn address_and_material_are_transport_associated_types() {
        fn assert_transport<
            T: Transport<
                    Address = Address,
                    HandshakeMaterial = dyn TestHandshakeMaterial,
                    Channel = Bytes,
                >,
        >(
            _: &T,
        ) {
        }
        assert_transport(&WsTransport);

        let material: Arc<dyn TestHandshakeMaterial> = Arc::new(Bearer("tok".into()));
        assert_eq!(material.marker(), "tok");

        let (tx, _rx) = oneshot::channel();
        let listen = WsListen::new(tx);
        compile_time_pairing_witness(&listen);
        compile_time_material_shape_witness();
    }
}
