use crate::RawVideoFrame;
use std::{
    collections::VecDeque,
    sync::{Condvar, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Notify;

#[derive(Debug, PartialEq, Eq)]
pub enum PushOutcome {
    Queued,
    /// The queue was full: the *oldest* frame was discarded to make room.
    DroppedOldest,
    /// The queue is closed; the frame was not queued.
    Closed,
}

struct State {
    items: VecDeque<(RawVideoFrame, Instant)>,
    closed: bool,
    dropped: u64,
}

/// Bounded queue of raw frames between capture and encoding.
///
/// Live sources use [`push_drop_oldest`](Self::push_drop_oldest): when the
/// encoder falls behind, the *stalest* frame is discarded so latency stays
/// bounded (the newest frame is always kept; drops are counted). Deterministic
/// sources use [`push_wait`](Self::push_wait) and are simply paced by the
/// consumer, dropping nothing. Usable from sync threads and async tasks.
/// The result of [`FrameQueue::pop_blocking_timed_for`].
#[derive(Debug)]
pub enum PopTimed {
    /// A frame, and how long it waited in the queue.
    Frame(RawVideoFrame, Duration),
    /// Nothing arrived within the wait; the queue is still open.
    Timeout,
    /// Closed and drained.
    Closed,
}

pub struct FrameQueue {
    capacity: usize,
    state: Mutex<State>,
    cond: Condvar,
    items: Notify,
    space: Notify,
}

impl FrameQueue {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            capacity,
            state: Mutex::new(State {
                items: VecDeque::with_capacity(capacity),
                closed: false,
                dropped: 0,
            }),
            cond: Condvar::new(),
            items: Notify::new(),
            space: Notify::new(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.state.lock().unwrap().items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Frames discarded by [`push_drop_oldest`](Self::push_drop_oldest) so far.
    pub fn dropped(&self) -> u64 {
        self.state.lock().unwrap().dropped
    }

    pub fn push_drop_oldest(&self, frame: RawVideoFrame) -> PushOutcome {
        let outcome = {
            let mut st = self.state.lock().unwrap();
            if st.closed {
                return PushOutcome::Closed;
            }
            let outcome = if st.items.len() == self.capacity {
                st.items.pop_front();
                st.dropped += 1;
                PushOutcome::DroppedOldest
            } else {
                PushOutcome::Queued
            };
            st.items.push_back((frame, Instant::now()));
            outcome
        };
        self.cond.notify_one();
        self.items.notify_one();
        outcome
    }

    /// Wait for space instead of dropping (for deterministic, consumer-paced sources).
    pub async fn push_wait(&self, frame: RawVideoFrame) -> PushOutcome {
        loop {
            let space = self.space.notified();
            {
                let mut st = self.state.lock().unwrap();
                if st.closed {
                    return PushOutcome::Closed;
                }
                if st.items.len() < self.capacity {
                    st.items.push_back((frame, Instant::now()));
                    drop(st);
                    self.cond.notify_one();
                    self.items.notify_one();
                    return PushOutcome::Queued;
                }
            }
            space.await;
        }
    }

    /// Stop accepting frames; consumers drain what is queued, then see the end.
    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.cond.notify_all();
        self.items.notify_waiters();
        self.items.notify_one();
        self.space.notify_waiters();
        self.space.notify_one();
    }

    /// Blocking pop for a worker thread. `None` once closed *and* drained.
    pub fn pop_blocking(&self) -> Option<RawVideoFrame> {
        self.pop_blocking_timed().map(|(f, _)| f)
    }

    /// Like [`pop_blocking`](Self::pop_blocking), also returning how long the
    /// frame waited in the queue (capture -> encoder-start latency).
    pub fn pop_blocking_timed(&self) -> Option<(RawVideoFrame, Duration)> {
        let mut st = self.state.lock().unwrap();
        loop {
            if let Some((f, at)) = st.items.pop_front() {
                drop(st);
                self.space.notify_one();
                return Some((f, at.elapsed()));
            }
            if st.closed {
                return None;
            }
            st = self.cond.wait(st).unwrap();
        }
    }

    /// [`pop_blocking_timed`](Self::pop_blocking_timed) that gives up after `max_wait`, so a caller
    /// that has other work to do (an encoder with packets still in flight) can go and do it.
    /// `Closed` only once the queue is closed *and* drained, exactly when `pop_blocking_timed`
    /// would return `None`.
    pub fn pop_blocking_timed_for(&self, max_wait: Duration) -> PopTimed {
        let deadline = Instant::now() + max_wait;
        let mut st = self.state.lock().unwrap();
        loop {
            if let Some((f, at)) = st.items.pop_front() {
                drop(st);
                self.space.notify_one();
                return PopTimed::Frame(f, at.elapsed());
            }
            if st.closed {
                return PopTimed::Closed;
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return PopTimed::Timeout;
            };
            st = self.cond.wait_timeout(st, left).unwrap().0;
        }
    }

    /// Async pop. `None` once closed *and* drained.
    pub async fn pop(&self) -> Option<RawVideoFrame> {
        loop {
            let items = self.items.notified();
            {
                let mut st = self.state.lock().unwrap();
                if let Some((f, _)) = st.items.pop_front() {
                    drop(st);
                    self.space.notify_one();
                    return Some(f);
                }
                if st.closed {
                    return None;
                }
            }
            items.await;
        }
    }

    /// Like [`pop_blocking`](Self::pop_blocking) with a timeout (`Ok(None)` = closed).
    pub fn pop_timeout(&self, timeout: Duration) -> Result<Option<RawVideoFrame>, ()> {
        let mut st = self.state.lock().unwrap();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some((f, _)) = st.items.pop_front() {
                drop(st);
                self.space.notify_one();
                return Ok(Some(f));
            }
            if st.closed {
                return Ok(None);
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(());
            }
            st = self.cond.wait_timeout(st, deadline - now).unwrap().0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq: u64) -> RawVideoFrame {
        RawVideoFrame {
            width: 2,
            height: 2,
            sequence: seq,
            capture_timestamp_ms: seq,
            y: vec![0; 4],
            u: vec![0],
            v: vec![0],
        }
    }

    #[test]
    fn drop_oldest_keeps_the_newest_and_counts_drops() {
        let q = FrameQueue::new(3);
        for i in 0..3 {
            assert_eq!(q.push_drop_oldest(frame(i)), PushOutcome::Queued);
        }
        assert_eq!(q.push_drop_oldest(frame(3)), PushOutcome::DroppedOldest);
        assert_eq!(q.push_drop_oldest(frame(4)), PushOutcome::DroppedOldest);
        assert_eq!((q.len(), q.dropped()), (3, 2));
        let seqs: Vec<u64> = std::iter::from_fn(|| q.pop_timeout(Duration::ZERO).ok().flatten())
            .map(|f| f.sequence)
            .collect();
        assert_eq!(
            seqs,
            [2, 3, 4],
            "the stalest frames went, the newest stayed, order kept"
        );
    }

    #[test]
    fn close_drains_then_ends_and_refuses_pushes() {
        let q = FrameQueue::new(2);
        q.push_drop_oldest(frame(0));
        q.close();
        assert_eq!(q.push_drop_oldest(frame(1)), PushOutcome::Closed);
        assert_eq!(q.pop_blocking().map(|f| f.sequence), Some(0));
        assert!(q.pop_blocking().is_none());
    }

    #[tokio::test]
    async fn push_wait_blocks_until_consumed_and_drops_nothing() {
        let q = std::sync::Arc::new(FrameQueue::new(1));
        q.push_wait(frame(0)).await;
        let q2 = q.clone();
        let pusher = tokio::spawn(async move { q2.push_wait(frame(1)).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!pusher.is_finished(), "waits while full");
        assert_eq!(q.pop().await.map(|f| f.sequence), Some(0));
        assert_eq!(pusher.await.unwrap(), PushOutcome::Queued);
        assert_eq!(q.pop().await.map(|f| f.sequence), Some(1));
        assert_eq!(q.dropped(), 0);
    }

    #[test]
    fn timed_pop_returns_a_frame_times_out_while_open_and_reports_closed_only_when_drained() {
        let q = FrameQueue::new(2);
        let t = Instant::now();
        assert!(matches!(
            q.pop_blocking_timed_for(Duration::from_millis(30)),
            PopTimed::Timeout
        ));
        assert!(t.elapsed() >= Duration::from_millis(25), "it really waited");
        q.push_drop_oldest(frame(7));
        match q.pop_blocking_timed_for(Duration::from_millis(30)) {
            PopTimed::Frame(f, waited) => {
                assert_eq!(f.sequence, 7);
                assert!(waited < Duration::from_secs(1));
            }
            other => panic!("expected the queued frame, got {other:?}"),
        }
        q.push_drop_oldest(frame(8));
        q.close();
        // closed but not drained: the frame still comes out, then Closed (never a Timeout)
        assert!(matches!(
            q.pop_blocking_timed_for(Duration::from_millis(30)),
            PopTimed::Frame(..)
        ));
        assert!(matches!(
            q.pop_blocking_timed_for(Duration::from_millis(30)),
            PopTimed::Closed
        ));
    }

    #[test]
    fn timed_pop_wakes_for_a_frame_that_arrives_while_it_waits() {
        let q = std::sync::Arc::new(FrameQueue::new(2));
        let q2 = q.clone();
        let pusher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            q2.push_drop_oldest(frame(1));
        });
        let t = Instant::now();
        assert!(matches!(
            q.pop_blocking_timed_for(Duration::from_secs(5)),
            PopTimed::Frame(..)
        ));
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "woke on the push, not on the timeout"
        );
        pusher.join().unwrap();
    }
}
