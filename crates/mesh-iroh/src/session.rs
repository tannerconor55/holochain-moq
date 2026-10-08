use anyhow::Context;
use async_trait::async_trait;
use holochain_moq_mesh::{
    ConnectionKind, DeliveryMode, LinkCounters, LinkInfo, Role, SessionEnd, TrackPath, TrackSource,
    TransportSession,
};
use iroh::endpoint::{Connection, ConnectionError, VarInt};
use moq_net::origin;

use crate::{spawn_driver, track, versions};

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
    ) -> anyhow::Result<Self> {
        let (subscribe, driver) = origin::Producer::new(origin::Config::default());
        spawn_driver(driver);
        let now = std::time::Instant::now();
        let (session, driver) = match role {
            Role::Dialer => {
                // moq-net is poll-only; web-transport-iroh 0.7 is async-only,
                // so it goes through moq-tokio's adapter (as the WP1 spike did).
                let wt = moq_tokio::transport::Session::new(web_transport_iroh::Session::raw(
                    conn.clone(),
                ));
                moq_net::Client::new()
                    .with_versions(versions())
                    .with_publisher(publish)
                    .with_subscriber(subscribe.clone())
                    .connect(now, wt)
                    .await
                    .context("moq client handshake")?
            }
            Role::Acceptor => {
                let wt = moq_tokio::transport::Session::new(
                    web_transport_iroh::QuicRequest::accept(conn.clone()).ok(),
                );
                moq_net::Server::new()
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
        tokio::select! {
            _ = self.conn.closed() => {}
            _ = self.session.closed() => {}
        }
    }

    fn close(&self) {
        self.conn
            .close(VarInt::from_u32(LEAVE_CODE), b"mesh session closed");
    }

    fn abort(&self) {
        self.conn
            .close(VarInt::from_u32(ABORT_CODE), b"mesh session aborted");
    }

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
