use anyhow::Context;
use async_trait::async_trait;
use holochain_moq_mesh::{
    ConnectionKind, DeliveryMode, LinkCounters, LinkInfo, Role, SessionEnd, TrackPath, TrackSource,
    TransportSession,
};
use iroh::endpoint::{Connection, ConnectionError, VarInt};
use moq_net::origin;

use crate::{spawn_driver, track, versions};

/// The web-transport session moq-net drives, with one change: `close` is a
/// no-op once the connection is already closed. noq's `Connection::close`
/// overwrites the recorded close reason with `LocallyClosed`, so moq-net
/// closing its side in reaction to the peer's close would erase the peer's
/// application code - the only way to tell a deliberate leave (`LEAVE_CODE`)
/// from an abort that must be reconnected (`ABORT_CODE`). A closed connection
/// sends nothing more, so skipping the call changes nothing on the wire.
#[derive(Clone)]
struct ClosePreserving {
    inner: web_transport_iroh::Session,
    conn: Connection,
}

impl web_transport_trait::Session for ClosePreserving {
    type SendStream = <web_transport_iroh::Session as web_transport_trait::Session>::SendStream;
    type RecvStream = <web_transport_iroh::Session as web_transport_trait::Session>::RecvStream;
    type Error = <web_transport_iroh::Session as web_transport_trait::Session>::Error;

    async fn accept_uni(&self) -> Result<Self::RecvStream, Self::Error> {
        web_transport_trait::Session::accept_uni(&self.inner).await
    }

    async fn accept_bi(&self) -> Result<(Self::SendStream, Self::RecvStream), Self::Error> {
        web_transport_trait::Session::accept_bi(&self.inner).await
    }

    async fn open_bi(&self) -> Result<(Self::SendStream, Self::RecvStream), Self::Error> {
        web_transport_trait::Session::open_bi(&self.inner).await
    }

    async fn open_uni(&self) -> Result<Self::SendStream, Self::Error> {
        web_transport_trait::Session::open_uni(&self.inner).await
    }

    fn close(&self, code: u32, reason: &str) {
        if self.conn.close_reason().is_none() {
            web_transport_trait::Session::close(&self.inner, code, reason);
        }
    }

    async fn closed(&self) -> Self::Error {
        web_transport_trait::Session::closed(&self.inner).await
    }

    fn send_datagram(&self, data: bytes::Bytes) -> Result<(), Self::Error> {
        web_transport_trait::Session::send_datagram(&self.inner, data)
    }

    async fn recv_datagram(&self) -> Result<bytes::Bytes, Self::Error> {
        web_transport_trait::Session::recv_datagram(&self.inner).await
    }

    fn max_datagram_size(&self) -> usize {
        web_transport_trait::Session::max_datagram_size(&self.inner)
    }

    fn protocol(&self) -> Option<&str> {
        web_transport_trait::Session::protocol(&self.inner)
    }

    fn stats(&self) -> impl web_transport_trait::Stats {
        web_transport_trait::Session::stats(&self.inner)
    }
}

/// One MoQ session (both roles) over one iroh connection.
/// Application close code of a deliberate leave (also what iroh's
/// `Endpoint::close` sends). Anything else ending a session is a loss.
pub const LEAVE_CODE: u32 = 0;
/// Application close code of [`TransportSession::abort`] (superseded or
/// severed): the remote must treat it as a loss and reconnect.
pub const ABORT_CODE: u32 = 0x4d10;

pub struct IrohMoqSession {
    conn: Connection,
    session: moq_net::Session,
    reliable_max_age: std::time::Duration,
    /// The remote's broadcasts arrive here; a subscription reads only the one
    /// broadcast its path names.
    subscribe: origin::Producer,
}

impl IrohMoqSession {
    pub(crate) async fn establish(
        conn: Connection,
        role: Role,
        remote_agent: &str,
        publish: &origin::Producer,
        reliable_max_age: std::time::Duration,
        stats: moq_net::stats::Session,
    ) -> anyhow::Result<Self> {
        // WP9: what the remote sends us is cached against a bounded target.
        let mut config = origin::Config::default();
        config.pool = moq_net::cache::Pool::new(
            moq_net::cache::Config::default().with_capacity(crate::SUBSCRIBE_CACHE_BYTES),
        );
        let (subscribe, driver) = origin::Producer::new(config);
        spawn_driver(driver);
        let now = std::time::Instant::now();
        let (session, driver) = match role {
            Role::Dialer => {
                // moq-net is poll-only; web-transport-iroh 0.7 is async-only,
                // so it goes through moq-tokio's adapter (as the WP1 spike did).
                let wt = moq_tokio::transport::Session::new(ClosePreserving {
                    inner: web_transport_iroh::Session::raw(conn.clone()),
                    conn: conn.clone(),
                });
                moq_net::Client::new()
                    .with_stats(stats)
                    .with_versions(versions())
                    .with_publisher(publish)
                    .with_subscriber(subscribe.clone())
                    .connect(now, wt)
                    .await
                    .context("moq client handshake")?
            }
            Role::Acceptor => {
                let wt = moq_tokio::transport::Session::new(ClosePreserving {
                    inner: web_transport_iroh::QuicRequest::accept(conn.clone()).ok(),
                    conn: conn.clone(),
                });
                moq_net::Server::new()
                    .with_stats(stats)
                    .with_versions(versions())
                    .with_publisher(publish)
                    .with_subscriber(subscribe.clone())
                    .accept(now, wt)
                    .await
                    .context("moq server handshake")?
            }
        };
        spawn_driver(driver);
        let _ = remote_agent;
        Ok(Self {
            conn,
            session,
            reliable_max_age,
            subscribe,
        })
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

#[async_trait]
impl TransportSession for IrohMoqSession {
    fn link_info(&self) -> LinkInfo {
        let paths = self.conn.paths();
        let selected = paths.iter().find(|p| p.is_selected());
        let st = self.conn.stats();
        LinkInfo {
            kind: selected.as_ref().map(|p| {
                if p.is_relay() {
                    ConnectionKind::Relayed
                } else {
                    ConnectionKind::Direct
                }
            }),
            rtt_ms: selected.as_ref().map(|p| p.rtt().as_secs_f64() * 1000.0),
            counters: Some(LinkCounters {
                tx_bytes: st.udp_tx.bytes,
                rx_bytes: st.udp_rx.bytes,
                tx_packets: st.udp_tx.datagrams,
                rx_packets: st.udp_rx.datagrams,
                lost_packets: st.lost_packets,
            }),
        }
    }

    fn subscribe(
        &self,
        path: &TrackPath,
        mode: DeliveryMode,
    ) -> anyhow::Result<Box<dyn TrackSource>> {
        self.subscribe_from(path, mode, 0)
    }

    fn subscribe_from(
        &self,
        path: &TrackPath,
        mode: DeliveryMode,
        next_seq: u64,
    ) -> anyhow::Result<Box<dyn TrackSource>> {
        let max_age = self.reliable_max_age;
        // Only the broadcast named by `path` is read from this session's
        // subscribe origin, so a peer can never serve another session's
        // sources, and no track is reachable outside the race.
        let consumer = self.subscribe.consume();
        let broadcast = path.broadcast();
        let track = path.track.as_str().to_string();
        let open = async move {
            let mut announced = consumer.announced();
            let prefix = loop {
                let update = announced
                    .next()
                    .await
                    .context("session ended before the broadcast was announced")?;
                if update.kind.is_active() && update.prefix.as_str() == broadcast {
                    break update.prefix;
                }
            };
            let bc = consumer
                .request_broadcast(&prefix)
                .await
                .context("request broadcast")?;
            // Resolves when the publisher accepts (immediately, or when it
            // creates a track that was requested early).
            let consumer = bc.track(&track).with_context(|| format!("track {track}"))?;
            let sub = consumer
                .subscribe(track::subscription(mode, max_age))
                .await
                .with_context(|| format!("subscribe {broadcast}/{track}"))?;
            Ok((consumer, sub))
        };
        let start = match mode {
            DeliveryMode::ReliableOrdered => next_seq,
            DeliveryMode::Latest => 0,
        };
        Ok(Box::new(track::spawn_reader(
            open,
            mode,
            start,
            self.conn.clone(),
        )))
    }

    async fn closed(&self) {
        let first = tokio::select! {
            e = self.conn.closed() => Some(e),
            _ = self.session.closed() => None,
        };
        let err = match first {
            Some(e) => Some(e),
            None => tokio::time::timeout(std::time::Duration::from_millis(250), self.conn.closed())
                .await
                .ok(),
        };
        let _ = err;
    }

    fn close(&self) {
        self.conn
            .close(VarInt::from_u32(LEAVE_CODE), b"mesh session closed");
    }

    fn abort(&self) {
        self.conn
            .close(VarInt::from_u32(ABORT_CODE), b"mesh session aborted");
    }

    /// A leave is exactly the peer's code-0 close. The peer's close reason is
    /// kept intact because this side's MoQ stack never closes an already
    /// closed connection ([`ClosePreserving`]); anything else (an abort, a
    /// timeout, a reset, a path failure, or this side closing first) is a loss.
    fn end_reason(&self) -> SessionEnd {
        match self.conn.close_reason() {
            Some(ConnectionError::ApplicationClosed(c))
                if c.error_code == VarInt::from_u32(LEAVE_CODE) =>
            {
                SessionEnd::Left
            }
            Some(e) => SessionEnd::Lost(e.to_string()),
            None => SessionEnd::Lost("MoQ session ended".into()),
        }
    }
}
