use std::time::Duration;

use anyhow::{bail, Context};
use async_trait::async_trait;
use holochain_moq_mesh::{
    ConnectionKind, DeliveryMode, LinkInfo, Role, TrackName, TrackSource, TransportSession,
};
use iroh::endpoint::{Connection, VarInt};
use moq_net::origin;

use crate::{spawn_driver, track, versions};

/// How long a subscribe waits for the remote's broadcast to be announced.
const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(10);

/// One MoQ session (both roles) over one iroh connection.
pub struct IrohMoqSession {
    conn: Connection,
    session: moq_net::Session,
    /// The remote's broadcasts arrive here; only `remote_agent` is ever read.
    subscribe: origin::Producer,
    remote_agent: String,
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
        Ok(Self {
            conn,
            session,
            subscribe,
            remote_agent: remote_agent.to_string(),
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
        match paths.iter().find(|p| p.is_selected()) {
            Some(p) => LinkInfo {
                kind: Some(if p.is_relay() {
                    ConnectionKind::Relayed
                } else {
                    ConnectionKind::Direct
                }),
                rtt_ms: Some(p.rtt().as_secs_f64() * 1000.0),
            },
            None => LinkInfo {
                kind: None,
                rtt_ms: None,
            },
        }
    }

    async fn subscribe(
        &self,
        track: &TrackName,
        mode: DeliveryMode,
    ) -> anyhow::Result<Box<dyn TrackSource>> {
        // Only the remote's own broadcast is consulted, so a peer can never
        // serve tracks under another agent's name over its session.
        let consumer = self.subscribe.consume();
        let mut announced = consumer.announced();
        let prefix = loop {
            let update = tokio::time::timeout(ANNOUNCE_TIMEOUT, announced.next())
                .await
                .with_context(|| format!("{} never announced its broadcast", self.remote_agent))?
                .context("session ended before the remote announced")?;
            if update.kind.is_active() && update.prefix.as_str() == self.remote_agent {
                break update.prefix;
            }
        };
        let broadcast = consumer
            .request_broadcast(&prefix)
            .await
            .context("request broadcast")?;
        let sub = broadcast
            .track(track.as_str())
            .with_context(|| format!("track {track}"))?
            .subscribe(None)
            .await
            .with_context(|| format!("subscribe {}/{track}", self.remote_agent))?;
        if self.conn.close_reason().is_some() {
            bail!("session closed while subscribing");
        }
        Ok(Box::new(track::spawn_reader(sub, mode)))
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
