//! [`SessionDirectory`]: the control-plane interface for live application sessions
//! (Stage 14B), and its in-memory test implementation.
//!
//! A session is application metadata (see `holochain_moq_protocol::session`), kept
//! apart from [`crate::PeerDirectory`]'s peers/endpoints/streams so that existing
//! `PeerDirectory` implementations and callers are untouched: a directory that can
//! also serve sessions implements this extension trait. Like everything in this
//! crate it holds no realtime payload and knows nothing about MoQ or QUIC.

use crate::{directory::PeerDirectory, error::DirectoryError, memory::InMemoryPeerDirectory};
use async_trait::async_trait;
use holochain_moq_protocol::{Session, SessionId};

#[async_trait]
pub trait SessionDirectory: PeerDirectory {
    /// Publish a version of a session (create, start or end it: each is the session
    /// in its new state). `session.publisher` must be this handle's own agent
    /// (`DirectoryError::SessionAuthorMismatch`) and the session coherent
    /// (`DirectoryError::InvalidSession`). Versions are append-only; readers see the
    /// newest one the publisher moved forward to, and nothing follows `Ended`.
    async fn publish_session(&self, session: Session) -> Result<(), DirectoryError>;

    /// The current state of one session, whatever it is. `Ok(None)`: never published.
    async fn find_session(&self, id: &SessionId) -> Result<Option<Session>, DirectoryError>;

    /// Every session whose current state is `Live`. Ended sessions are not listed.
    async fn list_live_sessions(&self) -> Result<Vec<Session>, DirectoryError>;
}

#[async_trait]
impl SessionDirectory for InMemoryPeerDirectory {
    async fn publish_session(&self, session: Session) -> Result<(), DirectoryError> {
        self.publish_session_version(session).await
    }

    async fn find_session(&self, id: &SessionId) -> Result<Option<Session>, DirectoryError> {
        Ok(self
            .resolved_sessions()
            .await
            .into_iter()
            .find(|s| s.session_id == *id))
    }

    async fn list_live_sessions(&self) -> Result<Vec<Session>, DirectoryError> {
        Ok(self
            .resolved_sessions()
            .await
            .into_iter()
            .filter(|s| s.is_live())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holochain_moq_protocol::StreamKind;

    fn session(n: u128, who: &str) -> Session {
        Session::new(
            SessionId(uuid::Uuid::from_u128(n)),
            who.to_string(),
            StreamKind::Screen,
            1,
            1_000_000,
            "ns",
            "track",
            None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_session_is_listed_only_while_live_and_ending_removes_it_from_the_listing() {
        let alice = InMemoryPeerDirectory::new("alice");
        let bob = alice.as_agent("bob");
        let s = session(1, "alice");

        alice.publish_session(s.clone()).await.unwrap();
        assert!(
            bob.list_live_sessions().await.unwrap().is_empty(),
            "Created is not live"
        );

        let live = s.start().unwrap();
        alice.publish_session(live.clone()).await.unwrap();
        assert_eq!(bob.list_live_sessions().await.unwrap(), vec![live.clone()]);
        assert_eq!(
            bob.find_session(&live.session_id).await.unwrap(),
            Some(live.clone())
        );

        let ended = live.end(9).unwrap();
        alice.publish_session(ended.clone()).await.unwrap();
        assert!(bob.list_live_sessions().await.unwrap().is_empty());
        assert_eq!(
            bob.find_session(&ended.session_id).await.unwrap(),
            Some(ended.clone()),
            "an ended session is not deleted, only no longer live"
        );

        // Nothing revives it.
        alice
            .publish_session(session(1, "alice").start().unwrap())
            .await
            .unwrap();
        assert!(bob.list_live_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn only_the_publisher_may_publish_and_only_coherent_sessions() {
        let alice = InMemoryPeerDirectory::new("alice");
        let bob = alice.as_agent("bob");
        let err = bob.publish_session(session(1, "alice")).await.unwrap_err();
        assert!(
            matches!(err, DirectoryError::SessionAuthorMismatch { .. }),
            "{err:?}"
        );

        let mut bad = session(2, "alice");
        bad.track.clear();
        let err = alice.publish_session(bad).await.unwrap_err();
        assert!(matches!(err, DirectoryError::InvalidSession(_)), "{err:?}");
        assert!(alice.list_live_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn another_agent_cannot_take_over_or_end_someone_elses_session_id() {
        let alice = InMemoryPeerDirectory::new("alice");
        let bob = alice.as_agent("bob");
        let live = session(1, "alice").start().unwrap();
        alice.publish_session(live.clone()).await.unwrap();
        // Bob publishes an ended version of the same id under his own name.
        bob.publish_session(session(1, "bob").end(5).unwrap())
            .await
            .unwrap();
        assert_eq!(alice.list_live_sessions().await.unwrap(), vec![live]);
    }
}
