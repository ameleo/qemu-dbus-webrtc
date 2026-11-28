// SPDX-License-Identifier: MPL-2.0

//! One UDP port for every session (`[listen] udp_port`): a firewall forwards a single port to
//! the host, whatever the number of sessions. Each address of the host has one socket on that
//! port, which the sessions send on; a task reads it and hands each datagram to its session:
//!
//! - an ICE connectivity check by the username fragment of its receiver (RFC 8445 7.2.2),
//!   which each session takes, unique, before its offer goes;
//! - a response to a check by its transaction, which the session's request noted;
//! - anything else (DTLS, RTP, RTCP) by the client address it comes from, which a session
//!   claims once it answers a check from it (str0m answers only checks whose integrity, keyed
//!   with the session's ICE password, holds) or sends it anything but a check. Not when it
//!   only sends checks to it: a client's answer could name another client's address as its
//!   candidate, and take that client's datagrams to its own session;
//! - the response to one of the server's own requests (the address a NAT maps the socket to)
//!   to that request.
//!
//! Anything else is dropped.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use str0m::IceCreds;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::network::stun;

/// How long the address a NAT maps a socket to stays known: a mapping that no traffic keeps
/// may change.
const REFLEXIVE_AGE: Duration = Duration::from_secs(30);
/// How long a check's transaction waits for its response: str0m gives up well before.
const TRANSACTION_AGE: Duration = Duration::from_secs(30);
/// The sockets' buffers, as the kernel allows (`net.core.wmem_max`, `rmem_max`): one socket
/// carries every session's bursts (a keyframe goes out at once, and str0m paces the rest).
const BUFFER: usize = 4 << 20;

/// A datagram for a session: where it came from, to which of its sockets.
pub struct Datagram {
    pub local: SocketAddr,
    pub source: SocketAddr,
    pub data: Vec<u8>,
}

/// The server's own Binding requests waiting for their responses, by transaction.
type Asked = Arc<Mutex<HashMap<[u8; 12], std::sync::mpsc::Sender<Vec<u8>>>>>;

/// The sockets on one port, and where their datagrams go.
pub struct Mux {
    port: u16,
    sockets: Mutex<HashMap<IpAddr, Bound>>,
    routes: Arc<Mutex<Routes>>,
    /// The server's own Binding requests, waiting for their responses.
    asked: Asked,
    /// The address a NAT maps each socket to: one lookup at a time per socket.
    reflexive: Mutex<HashMap<SocketAddr, Arc<Mutex<Reflexive>>>>,
}

/// What the STUN server said of a socket.
#[derive(Default)]
struct Reflexive {
    /// When it was last asked.
    asked: Option<Instant>,
    /// The last address it gave: kept when a later lookup gets no answer.
    mapped: Option<SocketAddr>,
}

struct Bound {
    socket: Arc<UdpSocket>,
    _reader: AbortOnDrop,
}

#[derive(Default)]
struct Routes {
    next: u64,
    sessions: HashMap<u64, mpsc::Sender<Datagram>>,
    /// Each session's ICE username fragment.
    fragments: HashMap<String, u64>,
    /// The checks the sessions sent, by transaction, and when.
    transactions: HashMap<[u8; 12], (u64, Instant)>,
    /// The clients the sessions claimed: (the host's socket, the client's address).
    clients: HashMap<(SocketAddr, SocketAddr), u64>,
}

/// The process's sockets on `port`: the server has one port for every session.
pub fn shared(port: u16) -> Arc<Mux> {
    static SHARED: OnceLock<Arc<Mux>> = OnceLock::new();
    SHARED.get_or_init(|| Arc::new(Mux::new(port))).clone()
}

impl Mux {
    pub fn new(port: u16) -> Self {
        Self {
            port,
            sockets: Mutex::default(),
            routes: Arc::default(),
            asked: Arc::default(),
            reflexive: Mutex::default(),
        }
    }

    /// The sockets on `addresses`, each bound the first time: an address the host gains
    /// later gets its socket with the next session. One that cannot be bound is left out.
    pub fn sockets(&self, addresses: &[IpAddr]) -> Vec<(SocketAddr, Arc<UdpSocket>)> {
        let mut sockets = self.sockets.lock().unwrap();
        let mut bound = Vec::new();
        for &ip in addresses {
            if let std::collections::hash_map::Entry::Vacant(vacant) = sockets.entry(ip) {
                match self.bind(ip) {
                    Ok(new) => {
                        vacant.insert(new);
                    }
                    Err(e) => {
                        warn!(address = %ip, port = self.port, error = %format!("{e:#}"), "no shared UDP socket");
                        continue;
                    }
                }
            }
            let socket = &sockets[&ip].socket;
            if let Ok(local) = socket.local_addr() {
                bound.push((local, socket.clone()));
            }
        }
        bound
    }

    fn bind(&self, ip: IpAddr) -> Result<Bound> {
        let socket = std::net::UdpSocket::bind((ip, self.port))?;
        for set in [
            rustix::net::sockopt::set_socket_send_buffer_size,
            rustix::net::sockopt::set_socket_recv_buffer_size,
        ] {
            if let Err(e) = set(&socket, BUFFER) {
                debug!(address = %ip, error = %e, "socket buffer");
            }
        }
        socket.set_nonblocking(true)?;
        let local = socket.local_addr()?;
        let socket = Arc::new(UdpSocket::from_std(socket)?);
        info!(%local, "one UDP port for every session");
        let reader = tokio::spawn(read(
            local,
            socket.clone(),
            self.routes.clone(),
            self.asked.clone(),
        ));
        Ok(Bound {
            socket,
            _reader: AbortOnDrop(reader),
        })
    }

    /// A session's place in the routes: its datagrams go to `incoming`. Its ICE credentials,
    /// for its offer, have a username fragment no other session has.
    pub fn register(&self, incoming: mpsc::Sender<Datagram>) -> Registration {
        let mut routes = self.routes.lock().unwrap();
        let creds = loop {
            let creds = IceCreds::new();
            if !routes.fragments.contains_key(&creds.ufrag) {
                break creds;
            }
        };
        let id = routes.next;
        routes.next += 1;
        routes.sessions.insert(id, incoming);
        routes.fragments.insert(creds.ufrag.clone(), id);
        Registration {
            routes: self.routes.clone(),
            id,
            creds,
            claimed: HashSet::new(),
        }
    }

    /// The address `server` sees the socket `local` at, from a Binding request on it: as
    /// asked less than `REFLEXIVE_AGE` ago, or asked now. Sessions starting together share
    /// one lookup per socket: the others wait for it, then take its answer. A lookup without
    /// an answer keeps the address the last one gave. Blocks for a few hundred milliseconds at
    /// most: call it off the async threads.
    pub fn reflexive(
        &self,
        local: SocketAddr,
        socket: &UdpSocket,
        server: SocketAddr,
    ) -> Option<SocketAddr> {
        let entry = self
            .reflexive
            .lock()
            .unwrap()
            .entry(local)
            .or_default()
            .clone();
        let mut entry = entry.lock().unwrap();
        if entry.asked.is_some_and(|at| at.elapsed() < REFLEXIVE_AGE) {
            return entry.mapped;
        }
        let rng = ring::rand::SystemRandom::new();
        let mut mapped = None;
        for _ in 0..stun::TRIES {
            let Ok(id) = ring::rand::generate::<[u8; 12]>(&rng) else {
                break;
            };
            let id = id.expose();
            let (answer, answered) = std::sync::mpsc::channel();
            self.asked.lock().unwrap().insert(id, answer);
            let sent = socket.try_send_to(&stun::request(&id), server);
            let response = sent
                .ok()
                .and_then(|_| answered.recv_timeout(stun::WAIT).ok());
            self.asked.lock().unwrap().remove(&id);
            mapped = response.and_then(|response| stun::mapped_address(&response, &id));
            if mapped.is_some() {
                break;
            }
        }
        entry.asked = Some(Instant::now());
        if mapped.is_some() {
            entry.mapped = mapped;
        }
        entry.mapped
    }
}

/// A session's place in the routes, until it is dropped.
pub struct Registration {
    routes: Arc<Mutex<Routes>>,
    id: u64,
    pub creds: IceCreds,
    /// The clients it claimed, so that it takes the lock only for a new one.
    claimed: HashSet<(SocketAddr, SocketAddr)>,
}

impl Registration {
    /// The session sends `contents` from its socket `local` to `client`: a check notes its
    /// transaction; an answer to a check, or anything but STUN, claims the client.
    pub fn sent(&mut self, local: SocketAddr, client: SocketAddr, contents: &[u8]) {
        match stun::binding(contents) {
            Some((stun::Class::Request, id)) => {
                let mut routes = self.routes.lock().unwrap();
                let now = Instant::now();
                if routes.transactions.len() >= 1024 {
                    routes
                        .transactions
                        .retain(|_, (_, at)| now.duration_since(*at) < TRANSACTION_AGE);
                }
                routes.transactions.insert(id, (self.id, now));
            }
            Some((stun::Class::Indication | stun::Class::Error, _)) => {}
            Some((stun::Class::Success, _)) | None => {
                if self.claimed.insert((local, client)) {
                    self.routes
                        .lock()
                        .unwrap()
                        .clients
                        .insert((local, client), self.id);
                }
            }
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut routes = self.routes.lock().unwrap();
        let id = self.id;
        routes.sessions.remove(&id);
        routes.fragments.remove(&self.creds.ufrag);
        routes.transactions.retain(|_, (session, _)| *session != id);
        routes.clients.retain(|_, session| *session != id);
    }
}

impl Routes {
    /// The session a datagram from `source` to the socket `local` goes to.
    fn route(&mut self, local: SocketAddr, source: SocketAddr, data: &[u8]) -> Option<u64> {
        match stun::binding(data) {
            Some((stun::Class::Request, _)) => {
                let fragment = stun::check_receiver(data)?;
                self.fragments.get(fragment).copied()
            }
            Some((stun::Class::Success | stun::Class::Error, id)) => {
                self.transactions.remove(&id).map(|(session, _)| session)
            }
            Some((stun::Class::Indication, _)) => None,
            None => self.clients.get(&(local, source)).copied(),
        }
    }
}

/// Read the datagrams of a shared socket, and hand each to its session.
async fn read(local: SocketAddr, socket: Arc<UdpSocket>, routes: Arc<Mutex<Routes>>, asked: Asked) {
    let mut buffer = vec![0; 2048];
    loop {
        let (length, source) = match socket.recv_from(&mut buffer).await {
            Ok(received) => received,
            // An ICMP error of an earlier datagram, on Linux: the socket goes on.
            Err(e) => {
                debug!(%local, error = %e, "receiving");
                continue;
            }
        };
        let data = &buffer[..length];
        if let Some((stun::Class::Success | stun::Class::Error, id)) = stun::binding(data)
            && let Some(answer) = asked.lock().unwrap().remove(&id)
        {
            let _ = answer.send(data.to_vec());
            continue;
        }
        let incoming = {
            let mut routes = routes.lock().unwrap();
            routes
                .route(local, source, data)
                .and_then(|session| routes.sessions.get(&session).cloned())
        };
        // A session that does not keep up loses the datagram, as on the network: the others
        // do not wait for it.
        if let Some(incoming) = incoming {
            let _ = incoming.try_send(Datagram {
                local,
                source,
                data: data.to_vec(),
            });
        }
    }
}

/// Aborts a task when dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The port of the sockets: refuses 0, which would give each address a port of its own.
pub fn check_port(port: u16) -> Result<u16> {
    if port == 0 {
        bail!("udp_port: a port from 1 to 65535");
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOOPBACK: [IpAddr; 1] = [IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)];

    /// A Binding request whose USERNAME is "<receiver>:<sender>".
    fn check(receiver: &str, id: [u8; 12]) -> Vec<u8> {
        let username = format!("{receiver}:them");
        let padded = username.len().next_multiple_of(4);
        let mut message = stun::request(&id).to_vec();
        message.extend_from_slice(&6u16.to_be_bytes());
        message.extend_from_slice(&(username.len() as u16).to_be_bytes());
        message.extend_from_slice(username.as_bytes());
        message.resize(20 + 4 + padded, 0);
        let length = (message.len() - 20) as u16;
        message[2..4].copy_from_slice(&length.to_be_bytes());
        message
    }

    /// A Binding success response of the transaction `id`, with no attribute.
    fn success(id: [u8; 12]) -> Vec<u8> {
        let mut message = stun::request(&id).to_vec();
        message[0..2].copy_from_slice(&0x0101u16.to_be_bytes());
        message
    }

    async fn next(incoming: &mut mpsc::Receiver<Datagram>) -> Option<Vec<u8>> {
        tokio::time::timeout(Duration::from_millis(300), incoming.recv())
            .await
            .ok()
            .flatten()
            .map(|d| d.data)
    }

    /// A STUN server on this host that answers the first `answers` requests, with the
    /// source as XOR-MAPPED-ADDRESS, and counts them all.
    fn stun_server(answers: usize) -> (SocketAddr, Arc<std::sync::atomic::AtomicUsize>) {
        let server = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = count.clone();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 1500];
            while let Ok((_, from)) = server.recv_from(&mut buffer) {
                let n = counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n >= answers {
                    continue;
                }
                let IpAddr::V4(ip) = from.ip() else { continue };
                let cookie = 0x2112_A442u32.to_be_bytes();
                let mut response = vec![0x01, 0x01, 0x00, 0x0c];
                response.extend_from_slice(&cookie);
                response.extend_from_slice(&buffer[8..20]);
                response.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
                response.extend_from_slice(&(from.port() ^ 0x2112).to_be_bytes());
                response.extend(ip.octets().iter().zip(cookie).map(|(b, c)| b ^ c));
                let _ = server.send_to(&response, from);
            }
        });
        (address, count)
    }

    /// Sessions starting together share one lookup per socket; a lookup without an answer
    /// keeps the address the last one gave.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_reflexive_lookup_per_socket() {
        let mux = Arc::new(Mux::new(0));
        let [(local, socket)] = &mux.sockets(&LOOPBACK)[..] else {
            panic!("no socket");
        };
        let (local, socket) = (*local, socket.clone());
        let (server, count) = stun_server(1);
        let lookups: Vec<_> = (0..4)
            .map(|_| {
                let (mux, socket) = (mux.clone(), socket.clone());
                tokio::task::spawn_blocking(move || mux.reflexive(local, &socket, server))
            })
            .collect();
        for lookup in lookups {
            assert_eq!(lookup.await.unwrap(), Some(local));
        }
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Asked again once stale: the server no longer answers, the address stays.
        mux.reflexive.lock().unwrap()[&local].lock().unwrap().asked = None;
        let again = tokio::task::spawn_blocking({
            let mux = mux.clone();
            move || mux.reflexive(local, &socket, server)
        });
        assert_eq!(again.await.unwrap(), Some(local));
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            1 + stun::TRIES as usize
        );
    }

    /// Two sessions on one socket: each gets its checks, the responses to its own checks and
    /// the datagrams of the client it claimed; a datagram from a client no one claimed, or
    /// that a session only sent checks to, goes to no one; a session gone leaves no route.
    #[tokio::test]
    async fn datagrams_go_to_their_session() {
        let mux = Mux::new(0);
        let [(local, _)] = mux.sockets(&LOOPBACK)[..] else {
            panic!("no socket");
        };
        let (a_tx, mut a) = mpsc::channel(16);
        let (b_tx, mut b) = mpsc::channel(16);
        let mut a_reg = mux.register(a_tx);
        let b_reg = mux.register(b_tx);
        assert_ne!(a_reg.creds.ufrag, b_reg.creds.ufrag);
        let client = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let other = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();

        // Checks, by the receiver's fragment.
        client
            .send_to(&check(&b_reg.creds.ufrag, [1; 12]), local)
            .unwrap();
        assert!(next(&mut b).await.is_some());
        client
            .send_to(&check(&a_reg.creds.ufrag, [2; 12]), local)
            .unwrap();
        assert!(next(&mut a).await.is_some());
        assert!(next(&mut b).await.is_none());

        // Media from a client no one claimed, and from one A only sent a check to.
        client.send_to(&[0x80; 40], local).unwrap();
        a_reg.sent(
            local,
            client.local_addr().unwrap(),
            &stun::request(&[3; 12]),
        );
        client.send_to(&[0x80; 40], local).unwrap();
        assert!(next(&mut a).await.is_none());
        // The response to A's check comes to A, once.
        client.send_to(&success([3; 12]), local).unwrap();
        assert!(next(&mut a).await.is_some());
        client.send_to(&success([3; 12]), local).unwrap();
        assert!(next(&mut a).await.is_none());

        // A answers the client's check: its media come to A, not another client's.
        a_reg.sent(local, client.local_addr().unwrap(), &success([2; 12]));
        client.send_to(&[0x80; 40], local).unwrap();
        assert_eq!(next(&mut a).await.as_deref(), Some(&[0x80; 40][..]));
        other.send_to(&[0x80; 40], local).unwrap();
        assert!(next(&mut a).await.is_none() && next(&mut b).await.is_none());

        // A gone: its fragment and its client lead nowhere.
        let fragment = a_reg.creds.ufrag.clone();
        drop(a_reg);
        let routes = mux.routes.lock().unwrap();
        assert!(!routes.fragments.contains_key(&fragment));
        assert!(routes.clients.is_empty() && routes.sessions.len() == 1);
    }
}
