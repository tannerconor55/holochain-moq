use anyhow::Context;
use async_trait::async_trait;
use holochain_moq_mesh::{
    ConnectionKind, DeliveryMode, LinkCounters, LinkInfo, Role, TrackPath, TrackSource,
    TransportSession,
};
use iroh::endpoint::{Connection, VarInt};
use moq_net::origin;

use crate::{spawn_driver, track, versions};

/// One MoQ session (both roles) over one iroh connection.
pub struct IrohMoqSession {
    conn: Connection,
    session: moq_net::Session,
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
                .subscribe(track::subscription(mode))
                .await
                .with_context(|| format!("subscribe {broadcast}/{track}"))?;
            Ok((consumer, sub))
        };
        Ok(Box::new(track::spawn_reader(open, mode)))
    }

    async fn closed(&self) {
        tokio::select! {
            _ = self.conn.closed() => {}
            _ = self.session.closed() => {}
        }
    }

    fn close(&self) {
        self.conn.close(VarInt::from_u32(0), b"mesh session closed");
    }
}
