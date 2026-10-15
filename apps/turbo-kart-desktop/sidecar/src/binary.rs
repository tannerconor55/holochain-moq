//! Separate E6.2 binary pipe. No JSON, socket path or arbitrary track names.
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use std::sync::Arc;
use crate::slot::NetSlot;

pub const HEADER: usize = 56;
pub const MAX_BODY: usize = HEADER + holochain_moq_mesh::RELIABLE_MAX_PAYLOAD;
pub const OUTPUT_QUEUE: usize = 64;
/// E6.3: the player's own WP3 kart state (`Latest` track `state`).
pub const KART_LANE: u8 = 4;
/// E6.4: host-owned AI kart state; `member` on a send is the AI slot (Rust checks the sender is the host).
pub const AI_LANE: u8 = 5;
/// E6.5a: the authoritative progress snapshot (host renderer → Rust on a send; host → peers on a delivery).
pub const PROGRESS_LANE: u8 = 6;
/// E6.5a: lap / finish / status events; delivery only (Rust derives them, no renderer sends them).
pub const PROGRESS_EVENT_LANE: u8 = 7;

#[derive(Debug, Clone)]
pub struct Packet {
    pub kind: u8, pub lane: u8, pub member: u8, pub code: u8,
    pub id: u32, pub generation: u32, pub seq: u64,
    pub race: String, pub payload: Vec<u8>,
}
impl Packet {
    pub fn parse(bytes: &[u8]) -> Result<Self, &'static str> {
        if !(HEADER..=MAX_BODY).contains(&bytes.len()) || &bytes[..4] != b"TKR2" { return Err("invalid binary frame size/magic"); }
        let race = std::str::from_utf8(&bytes[24..56]).map_err(|_| "invalid race text")?;
        if race.len() != 32 || !race.bytes().all(|c| c.is_ascii_hexdigit()) { return Err("invalid race ID"); }
        let p = Self { kind: bytes[4], lane: bytes[5], member: bytes[6], code: bytes[7],
            id: u32::from_be_bytes(bytes[8..12].try_into().unwrap()),
            generation: u32::from_be_bytes(bytes[12..16].try_into().unwrap()),
            seq: u64::from_be_bytes(bytes[16..24].try_into().unwrap()), race: race.to_string(), payload: bytes[HEADER..].to_vec() };
        if !(1..=PROGRESS_EVENT_LANE).contains(&p.lane) || p.member > 7 { return Err("invalid fixed lane/member"); }
        let max = if p.lane == 2 { holochain_moq_mesh::RELIABLE_MAX_PAYLOAD } else { holochain_moq_mesh::LATEST_MAX_PAYLOAD };
        if p.kind == 1 && (p.code != 0 || (p.member != 0 && p.lane != AI_LANE) || p.seq != 0 || p.payload.len() > max) { return Err("invalid send or oversized payload"); }
        // E6.3 kart state: exactly one WP3 KartState, nothing else.
        if p.kind == 1 && (p.lane == KART_LANE || p.lane == AI_LANE) && p.payload.len() != holochain_moq_kart_wire::KART_STATE_LEN { return Err("kart state must be one WP3 KartState"); }
        if p.kind == 1 && p.lane == PROGRESS_LANE && p.payload.len() != crate::progress::BODY_LEN { return Err("progress must be one progress body"); }
        if p.kind == 1 && p.lane == PROGRESS_EVENT_LANE { return Err("progress events are derived by Rust, never sent"); }
        if p.kind == 5 && (!p.payload.is_empty() || p.code != 0 || p.member != 0 || p.seq != 0) { return Err("invalid disconnect probe"); }
        Ok(p)
    }
    pub fn bytes(&self) -> Vec<u8> {
        let mut b=vec![0;HEADER]; b[..4].copy_from_slice(b"TKR2"); b[4]=self.kind; b[5]=self.lane; b[6]=self.member; b[7]=self.code;
        b[8..12].copy_from_slice(&self.id.to_be_bytes()); b[12..16].copy_from_slice(&self.generation.to_be_bytes());
        b[16..24].copy_from_slice(&self.seq.to_be_bytes()); b[24..56].copy_from_slice(self.race.as_bytes()); b.extend_from_slice(&self.payload); b
    }
    pub fn delivery(lane:u8, member:u8, generation:u32, race:&str, seq:u64, payload:Vec<u8>) -> Self {
        Self { kind:3,lane,member,code:0,id:0,generation,seq,race:race.into(),payload }
    }
    pub fn reply(&self, result:Result<u64, crate::protocol::IpcError>) -> Self {
        let (code,seq,payload)=match result { Ok(seq)=>(0,seq,vec![]),Err(e)=>(if e.code==crate::protocol::IpcErrorCode::NotHost {4} else {1},0,e.message.bytes().take(256).collect()) };
        Self {kind:2,code,seq,payload,..self.clone()}
    }
}

pub async fn read<R: AsyncRead + Unpin>(r:&mut R) -> std::io::Result<Vec<u8>> {
    let n=r.read_u32().await? as usize;
    if !(HEADER..=MAX_BODY).contains(&n) { return Err(std::io::Error::new(std::io::ErrorKind::InvalidData,"binary frame length refused")); }
    let mut b=vec![0;n]; r.read_exact(&mut b).await?; Ok(b)
}
pub async fn write<W: AsyncWrite + Unpin>(w:&mut W, p:&Packet) -> std::io::Result<()> {
    let b=p.bytes(); w.write_u32(b.len() as u32).await?; w.write_all(&b).await
}

/// fd 3 is an inherited, anonymous duplex pipe owned only by Electron main
/// and its child. No listener, bearer credential or renderer-selected path.
#[cfg(unix)]
pub fn inherited() -> std::io::Result<tokio::net::UnixStream> {
    use std::os::fd::FromRawFd;
    // Called exactly once, only when trusted main provisions binary_enabled.
    let socket=unsafe { std::os::unix::net::UnixStream::from_raw_fd(3) };
    socket.set_nonblocking(true)?; tokio::net::UnixStream::from_std(socket)
}

pub async fn serve<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(stream:S, slot:Arc<NetSlot>,
    tx:mpsc::Sender<Packet>, mut rx:mpsc::Receiver<Packet>) {
    let (mut r,mut w)=tokio::io::split(stream);
    let writer=tokio::spawn(async move { while let Some(p)=rx.recv().await { if write(&mut w,&p).await.is_err() { break; } } });
    while let Ok(b)=read(&mut r).await {
        let p=match Packet::parse(&b) { Ok(p) if p.kind==1 || p.kind==5 => p,_=>break };
        let result=slot.binary_send(&p).await;
        if tx.send(p.reply(result)).await.is_err() { break; }
    }
    // A failed data pipe stops the session; it does not silently reroute data
    // through JSON or keep publishing without its authorized consumer.
    slot.stop_session().await;
    writer.abort(); let _=writer.await;
}

#[cfg(test)]
mod tests {
 use super::*;
 #[tokio::test] async fn lengths_refused_before_body_allocation() {
  for len in [0u32,55,MAX_BODY as u32+1,u32::MAX] {
   let (mut w,mut r)=tokio::io::duplex(8); w.write_u32(len).await.unwrap();
   assert_eq!(read(&mut r).await.unwrap_err().kind(),std::io::ErrorKind::InvalidData);
  }
 }
 #[test] fn fixed_lanes_and_payload_limits() {
  let mut p=Packet::delivery(1,0,1,&"a".repeat(32),0,vec![1;1024]);p.kind=1;p.id=1;
  assert!(Packet::parse(&p.bytes()).is_ok());p.payload.push(1);assert!(Packet::parse(&p.bytes()).is_err());
  p.lane=2;p.payload=vec![2;4096];assert!(Packet::parse(&p.bytes()).is_ok());
  p.payload.push(2);assert!(Packet::parse(&p.bytes()).is_err());p.payload.clear();p.lane=8;assert!(Packet::parse(&p.bytes()).is_err());
 }
 #[test] fn ai_lane_names_a_slot_and_carries_exactly_one_wp3_state() {
  let mut p=Packet::delivery(AI_LANE,5,1,&"a".repeat(32),0,vec![1;holochain_moq_kart_wire::KART_STATE_LEN]);p.kind=1;p.id=1;
  assert!(Packet::parse(&p.bytes()).is_ok());
  p.member=8;assert!(Packet::parse(&p.bytes()).is_err());p.member=5;
  for n in [0,41,43,1024] { p.payload=vec![1;n]; assert!(Packet::parse(&p.bytes()).is_err(),"{n}"); }
  // Human state still names no member on a send.
  let mut h=Packet::delivery(KART_LANE,1,1,&"a".repeat(32),0,vec![1;holochain_moq_kart_wire::KART_STATE_LEN]);h.kind=1;h.id=1;assert!(Packet::parse(&h.bytes()).is_err());
 }
 #[test] fn progress_lane_carries_one_body_and_events_are_never_sent() {
  let mut p=Packet::delivery(PROGRESS_LANE,0,1,&"a".repeat(32),0,vec![0;crate::progress::BODY_LEN]);p.kind=1;p.id=1;
  assert!(Packet::parse(&p.bytes()).is_ok());
  for n in [0,crate::progress::BODY_LEN-1,crate::progress::BODY_LEN+1] { p.payload=vec![0;n]; assert!(Packet::parse(&p.bytes()).is_err(),"{n}"); }
  p.payload=vec![0;crate::progress::BODY_LEN];p.member=1;assert!(Packet::parse(&p.bytes()).is_err());
  let mut e=Packet::delivery(PROGRESS_EVENT_LANE,0,1,&"a".repeat(32),0,vec![0;crate::progress::EVENT_LEN]);e.kind=1;e.id=1;
  assert!(Packet::parse(&e.bytes()).is_err());
  e.kind=3;assert!(Packet::parse(&e.bytes()).is_ok());
 }
 #[test] fn kart_lane_carries_exactly_one_wp3_state() {
  let mut p=Packet::delivery(KART_LANE,0,1,&"a".repeat(32),0,vec![1;holochain_moq_kart_wire::KART_STATE_LEN]);p.kind=1;p.id=1;
  assert!(Packet::parse(&p.bytes()).is_ok());
  for n in [0,41,43,1024] { p.payload=vec![1;n]; assert!(Packet::parse(&p.bytes()).is_err(),"{n}"); }
 }
}
