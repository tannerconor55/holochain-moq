//! Test-only lossy UDP proxy: `client -> proxy -> target` and back, dropping
//! each datagram independently with probability `loss`, using a seeded
//! xorshift so the drop *decisions* are reproducible (which packets they hit
//! still depends on timing). Counts what it forwarded and dropped, so a test
//! can prove the traffic really went through it.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::net::UdpSocket;

#[derive(Default, Debug)]
pub struct ProxyStats {
    pub forwarded: AtomicU64,
    pub dropped: AtomicU64,
    /// Bytes of every datagram that reached the proxy (forwarded or dropped).
    pub bytes: AtomicU64,
    /// Datagrams / bytes forwarded target -> client (the client's receive side).
    pub to_client: AtomicU64,
    pub to_client_bytes: AtomicU64,
}

impl ProxyStats {
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.forwarded.load(Ordering::Relaxed),
            self.dropped.load(Ordering::Relaxed),
        )
    }
}

struct Dice(u64);

impl Dice {
    fn hit(&mut self, p: f64) -> bool {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64 <= p && p > 0.0
    }
}

pub struct LossyProxy {
    pub addr: SocketAddr,
    pub stats: Arc<ProxyStats>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for LossyProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl LossyProxy {
    /// Listen on 127.0.0.1 and forward to `target`. One upstream socket per
    /// client address, so replies are routed back to the right client.
    pub async fn start(target: SocketAddr, loss: f64, seed: u64) -> std::io::Result<Self> {
        let front = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let addr = front.local_addr()?;
        let stats = Arc::new(ProxyStats::default());
        let dice = Arc::new(Mutex::new(Dice(seed | 1)));
        let st = stats.clone();
        let task = tokio::spawn(async move {
            let mut upstream: HashMap<SocketAddr, Arc<UdpSocket>> = HashMap::new();
            let mut buf = vec![0u8; 65536];
            loop {
                let Ok((n, client)) = front.recv_from(&mut buf).await else {
                    return;
                };
                let up = match upstream.get(&client) {
                    Some(u) => u.clone(),
                    None => {
                        let Ok(u) = UdpSocket::bind("127.0.0.1:0").await else {
                            continue;
                        };
                        let u = Arc::new(u);
                        upstream.insert(client, u.clone());
                        // Return path: target -> upstream socket -> front -> client.
                        let (front, u2, st, dice) =
                            (front.clone(), u.clone(), st.clone(), dice.clone());
                        tokio::spawn(async move {
                            let mut b = vec![0u8; 65536];
                            while let Ok((n, _)) = u2.recv_from(&mut b).await {
                                st.bytes.fetch_add(n as u64, Ordering::Relaxed);
                                if dice.lock().unwrap().hit(loss) {
                                    st.dropped.fetch_add(1, Ordering::Relaxed);
                                } else {
                                    st.forwarded.fetch_add(1, Ordering::Relaxed);
                                    st.to_client.fetch_add(1, Ordering::Relaxed);
                                    st.to_client_bytes.fetch_add(n as u64, Ordering::Relaxed);
                                    let _ = front.send_to(&b[..n], client).await;
                                }
                            }
                        });
                        u
                    }
                };
                st.bytes.fetch_add(n as u64, Ordering::Relaxed);
                if dice.lock().unwrap().hit(loss) {
                    st.dropped.fetch_add(1, Ordering::Relaxed);
                } else {
                    st.forwarded.fetch_add(1, Ordering::Relaxed);
                    let _ = up.send_to(&buf[..n], target).await;
                }
            }
        });
        Ok(Self { addr, stats, task })
    }
}
