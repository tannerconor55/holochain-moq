//! MoQ-backed implementation of the publish role, playing the *QUIC server*
//! side of the connection: a publisher listens, a subscriber dials in. This
//! is deliberately relay-free (see ARCHITECTURE.md section 4/14 - a relay is
//! introduced later, once direct peer-to-peer works).

use crate::transport::{Publication, PublishTransport};
use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use moq_native_ietf::{quic, tls};
use moq_transport::{
    coding::TrackNamespace,
    serve::{self, Subgroup, SubgroupWriter},
    session::{Publisher, Session},
};
use std::{net::SocketAddr, path::Path};
use tokio::task::JoinHandle;

/// A publishing node: binds a UDP/QUIC socket, presents a TLS certificate,
/// and accepts one MoQ session at a time in the Publisher role.
pub struct MoqPublishNode {
    server: quic::Server,
    local_addr: SocketAddr,
    namespace: String,
    active: Option<(Session, Publisher)>,
}

impl MoqPublishNode {
    /// Bind and listen on `addr`, using the self-signed cert/key at
    /// `cert_path`/`key_path` (see [`crate::certs::generate_localhost_cert`]).
    /// Tracks published from this node live under `namespace`.
    pub fn bind(
        addr: SocketAddr,
        cert_path: &Path,
        key_path: &Path,
        namespace: &str,
    ) -> Result<Self> {
        let tls_args = tls::Args {
            cert: vec![cert_path.to_path_buf()],
            key: vec![key_path.to_path_buf()],
            ..Default::default()
        };
        let tls = tls_args.load().context("loading TLS server config")?;
        let config = quic::Config::new(addr, None, tls).context("building QUIC config")?;
        let endpoint = quic::Endpoint::new(config).context("creating QUIC endpoint")?;
        let local_addr = endpoint
            .client
            .local_addr()
            .context("reading local address")?;
        let server = endpoint.server.context(
            "no server TLS config was set up (cert/key missing) - cannot accept connections",
        )?;

        Ok(Self {
            server,
            local_addr,
            namespace: namespace.to_string(),
            active: None,
        })
    }

    /// The address peers should dial to reach this node.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

#[async_trait]
impl PublishTransport for MoqPublishNode {
    type Publication = MoqPublication;

    async fn connect(&mut self) -> Result<()> {
        let (session, conn_info) = self
            .server
            .accept()
            .await
            .context("QUIC endpoint closed before a peer connected")?;
        tracing::info!(peer = %conn_info.remote_address, "accepted QUIC connection");

        let (session, publisher) = Publisher::accept(session, conn_info.transport)
            .await
            .context("MoQT publisher handshake failed")?;
        tracing::info!("MoQT publisher session established");

        self.active = Some((session, publisher));
        Ok(())
    }

    async fn publish(&mut self, track: &str) -> Result<MoqPublication> {
        let (session, mut publisher) = self
            .active
            .take()
            .context("publish() called before connect()")?;

        let (mut tracks_writer, _tracks_request, tracks_reader) = serve::Tracks {
            namespace: TrackNamespace::from_utf8_path(&self.namespace),
        }
        .produce();

        let track_writer = tracks_writer
            .create(track)
            .context("track already published on this connection")?;
        let mut subgroups_writer = track_writer.subgroups().context("track already claimed")?;

        // Drive the QUIC session and announce our namespace/track catalog to
        // the peer for as long as the connection lives. This runs
        // independently of `MoqPublication::send`, which only touches the
        // object writer.
        //
        // `tracks_writer` has to be kept alive here too, even though we
        // never call anything on it again after `create()` above: dropping
        // it closes the underlying watch channel that `tracks_reader` (and
        // therefore `publish_namespace`) reads from, which the peer
        // observes as the announced catalog going stale - a real, easy to
        // hit race where an incoming SUBSCRIBE for a track we did create
        // gets rejected as "not found" because `publish()` had already
        // returned (and dropped `tracks_writer`) by the time it arrived.
        let task: JoinHandle<Result<()>> = tokio::spawn(async move {
            let _keep_tracks_writer_alive = tracks_writer;
            tokio::select! {
                res = session.run() => res.context("session error"),
                res = publisher.publish_namespace(tracks_reader) => res.context("failed to serve namespace"),
            }
        });

        // All objects for this milestone go into a single, long-lived
        // subgroup (i.e. a single QUIC stream). MoQ only orders objects
        // *within* a subgroup, not across independently-opened subgroups -
        // two subgroups opened back-to-back can legitimately be delivered
        // to the subscriber in either order (that is the ephemeral,
        // freshness-over-completeness model ARCHITECTURE.md section 16
        // describes). "hello-stream" wants "hello" then "world" then
        // timestamps in send order, so it needs the ordering guarantee a
        // single subgroup provides. A continuous media track (Phase 6+)
        // will have its own reason to open new groups (e.g. one per
        // keyframe) and should revisit this then rather than inherit it.
        let subgroup = subgroups_writer
            .create(Subgroup {
                group_id: 0,
                subgroup_id: 0,
                priority: 0,
            })
            .context("failed to open MoQ subgroup")?;

        Ok(MoqPublication {
            writer: subgroup,
            // moq-transport considers a track "stale" once the
            // `SubgroupsWriter` it was opened from is dropped, even while a
            // `SubgroupWriter` created from it is still alive and being
            // written to - so this has to live exactly as long as `writer`
            // above, not just long enough to call `create()` once.
            _subgroups_writer: subgroups_writer,
            _task: task,
        })
    }

    async fn disconnect(mut self) -> Result<()> {
        // Ownership of the live session moves into the `MoqPublication`
        // returned by `publish()`; once that is dropped the QUIC connection
        // is torn down. Nothing left to do here but stop accepting.
        self.active = None;
        Ok(())
    }
}

/// A single published track, ready to send ephemeral objects. All objects
/// sent through one `MoqPublication` arrive in send order (see the note in
/// `publish()` about why this is one subgroup rather than one-per-message).
pub struct MoqPublication {
    writer: SubgroupWriter,
    _subgroups_writer: serve::SubgroupsWriter,
    _task: JoinHandle<Result<()>>,
}

impl MoqPublication {
    /// Tear the QUIC/MoQ session down immediately, without finishing the
    /// track - what the subscriber sees is a failed connection, not a clean
    /// end of stream. (Merely dropping a `MoqPublication` detaches the
    /// session task and ends the track gracefully instead.)
    pub fn abort(self) {
        self._task.abort();
    }
}

#[async_trait]
impl Publication for MoqPublication {
    async fn send(&mut self, payload: Bytes) -> Result<()> {
        self.writer
            .write(payload)
            .context("failed to write object")?;
        Ok(())
    }
}
