// SPDX-License-Identifier: GPL-3.0-or-later
// mosh-win: Copyright 2026 the mosh-win authors.
// Ported from mosh (https://mosh.org): Copyright 2012 Keith Winstein and the mosh authors.

//! UDP datagram layer, port of mosh's `Network::Connection` (client side only).
//!
//! Each socket gets a receiver thread that forwards raw datagrams to the main
//! loop; decryption and timing bookkeeping happen on the main thread.

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::crypto::Session;
use crate::timestamp::{timestamp, timestamp16, timestamp_diff};

pub const MOSH_PROTOCOL_VERSION: u32 = 2;

const DIRECTION_MASK: u64 = 1 << 63;
const SEQUENCE_MASK: u64 = !DIRECTION_MASK;

const IPV4_HEADER_LEN: usize = 20 + 8;
const IPV6_HEADER_LEN: usize = 40 + 16 + 8;
const DEFAULT_SEND_MTU: usize = 500;
const DEFAULT_IPV4_MTU: usize = 1280;
const DEFAULT_IPV6_MTU: usize = 1280;
const MIN_RTO: u64 = 50;
const MAX_RTO: u64 = 1000;
const PORT_HOP_INTERVAL: u64 = 10000;
const MAX_PORTS_OPEN: usize = 10;
const MAX_OLD_SOCKET_AGE: u64 = 60000;

/// Network transport overhead (nonce + timestamps).
pub const ADDED_BYTES: usize = 8 + 4;
/// OCB tag.
pub const CRYPTO_ADDED_BYTES: usize = 16;

const RECEIVE_MTU: usize = 2048;

/// An undecrypted datagram from one of our sockets.
pub struct Datagram(pub Vec<u8>);

struct Sock {
    socket: Arc<UdpSocket>,
    alive: Arc<AtomicBool>,
}

impl Drop for Sock {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
    }
}

pub struct Connection {
    socks: VecDeque<Sock>,
    remote_addr: SocketAddr,
    mtu: usize,
    session: Session,
    next_seq: u64,
    saved_timestamp: u16,
    saved_timestamp_received_at: u64,
    expected_receiver_seq: u64,
    last_heard: u64,
    last_port_choice: u64,
    last_roundtrip_success: u64,
    rtt_hit: bool,
    srtt: f64,
    rttvar: f64,
    send_error: String,
    datagram_tx: Sender<crate::client::Event>,
}

fn bind_socket(remote: &SocketAddr) -> std::io::Result<UdpSocket> {
    let local: SocketAddr = if remote.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let s = UdpSocket::bind(local)?;
    s.set_read_timeout(Some(Duration::from_millis(250)))?;
    Ok(s)
}

fn spawn_receiver(socket: Arc<UdpSocket>, alive: Arc<AtomicBool>, tx: Sender<crate::client::Event>) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; RECEIVE_MTU];
        while alive.load(Ordering::Relaxed) {
            match socket.recv_from(&mut buf) {
                Ok((n, _from)) => {
                    if !alive.load(Ordering::Relaxed) {
                        break;
                    }
                    if tx.send(crate::client::Event::Net(Datagram(buf[..n].to_vec()))).is_err() {
                        break;
                    }
                }
                // Timeouts, and WSAECONNRESET from ICMP port-unreachable, are routine.
                Err(_) => continue,
            }
        }
    });
}

impl Connection {
    pub fn new(key: &[u8; 16], remote_addr: SocketAddr, tx: Sender<crate::client::Event>) -> std::io::Result<Self> {
        let mtu = if remote_addr.is_ipv4() {
            DEFAULT_IPV4_MTU - IPV4_HEADER_LEN
        } else {
            DEFAULT_IPV6_MTU - IPV6_HEADER_LEN
        };
        let mut c = Connection {
            socks: VecDeque::new(),
            remote_addr,
            mtu,
            session: Session::new(key),
            next_seq: 0,
            saved_timestamp: u16::MAX,
            saved_timestamp_received_at: 0,
            expected_receiver_seq: 0,
            last_heard: u64::MAX,
            last_port_choice: timestamp(),
            last_roundtrip_success: u64::MAX,
            rtt_hit: false,
            srtt: 1000.0,
            rttvar: 500.0,
            send_error: String::new(),
            datagram_tx: tx,
        };
        c.add_socket()?;
        Ok(c)
    }

    fn add_socket(&mut self) -> std::io::Result<()> {
        let socket = Arc::new(bind_socket(&self.remote_addr)?);
        let alive = Arc::new(AtomicBool::new(true));
        spawn_receiver(socket.clone(), alive.clone(), self.datagram_tx.clone());
        self.socks.push_back(Sock { socket, alive });
        Ok(())
    }

    fn hop_port(&mut self) {
        self.last_port_choice = timestamp();
        if let Err(e) = self.add_socket() {
            self.send_error = format!("socket: {e}");
        }
        self.prune_sockets();
    }

    fn prune_sockets(&mut self) {
        // don't keep old sockets if the new socket has been working for long enough
        if self.socks.len() > 1 {
            if timestamp().wrapping_sub(self.last_port_choice) > MAX_OLD_SOCKET_AGE {
                while self.socks.len() > 1 {
                    self.socks.pop_front();
                }
            }
        } else {
            return;
        }
        // make sure we don't have too many receive sockets open
        while self.socks.len() > MAX_PORTS_OPEN {
            self.socks.pop_front();
        }
    }

    pub fn send(&mut self, payload: &[u8]) {
        let mut outgoing_timestamp_reply = u16::MAX;
        let now = timestamp();
        if now.wrapping_sub(self.saved_timestamp_received_at) < 1000 {
            // send "corrected" timestamp advanced by how long we held it
            outgoing_timestamp_reply =
                self.saved_timestamp.wrapping_add(now.wrapping_sub(self.saved_timestamp_received_at) as u16);
            self.saved_timestamp = u16::MAX;
            self.saved_timestamp_received_at = 0;
        }

        let seq = self.next_seq;
        self.next_seq += 1;
        let direction_seq = seq & SEQUENCE_MASK; // TO_SERVER: direction bit clear
        let mut plaintext = Vec::with_capacity(4 + payload.len());
        plaintext.extend_from_slice(&timestamp16().to_be_bytes());
        plaintext.extend_from_slice(&outgoing_timestamp_reply.to_be_bytes());
        plaintext.extend_from_slice(payload);
        let packet = self.session.encrypt(direction_seq, &plaintext);

        if let Some(sock) = self.socks.back() {
            match sock.socket.send_to(&packet, self.remote_addr) {
                Ok(n) if n == packet.len() => {}
                Ok(_) => self.send_error = "sendto: short write".into(),
                Err(e) => {
                    self.send_error = format!("sendto: {e}");
                    if e.raw_os_error() == Some(10040) {
                        // WSAEMSGSIZE
                        self.mtu = DEFAULT_SEND_MTU;
                    }
                }
            }
        }

        let now = timestamp();
        if now.wrapping_sub(self.last_port_choice) > PORT_HOP_INTERVAL
            && now.wrapping_sub(self.last_roundtrip_success) > PORT_HOP_INTERVAL
        {
            self.hop_port();
        }
    }

    /// Decrypts a datagram and updates timing state. Returns the payload.
    pub fn recv(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        let (nonce_val, text) = self.session.decrypt(data)?;
        if text.len() < 4 {
            return None;
        }
        // prevent malicious playback to sender
        if nonce_val & DIRECTION_MASK == 0 {
            return None;
        }
        let seq = nonce_val & SEQUENCE_MASK;
        let ts = u16::from_be_bytes([text[0], text[1]]);
        let ts_reply = u16::from_be_bytes([text[2], text[3]]);
        let payload = text[4..].to_vec();

        self.prune_sockets();

        if seq < self.expected_receiver_seq {
            // don't use (but do return) out-of-order packets for timestamp or targeting
            return Some(payload);
        }
        self.expected_receiver_seq = seq + 1;

        if ts != u16::MAX {
            self.saved_timestamp = ts;
            self.saved_timestamp_received_at = timestamp();
        }

        if ts_reply != u16::MAX {
            let r = timestamp_diff(timestamp16(), ts_reply) as f64;
            if r < 5000.0 {
                if !self.rtt_hit {
                    self.srtt = r;
                    self.rttvar = r / 2.0;
                    self.rtt_hit = true;
                } else {
                    const ALPHA: f64 = 1.0 / 8.0;
                    const BETA: f64 = 1.0 / 4.0;
                    self.rttvar = (1.0 - BETA) * self.rttvar + BETA * (self.srtt - r).abs();
                    self.srtt = (1.0 - ALPHA) * self.srtt + ALPHA * r;
                }
            }
        }

        self.last_heard = timestamp();
        Some(payload)
    }

    pub fn mtu(&self) -> usize {
        self.mtu
    }

    pub fn timeout(&self) -> u64 {
        let rto = (self.srtt + 4.0 * self.rttvar).ceil() as u64;
        rto.clamp(MIN_RTO, MAX_RTO)
    }

    pub fn srtt(&self) -> f64 {
        self.srtt
    }

    pub fn set_last_roundtrip_success(&mut self, t: u64) {
        self.last_roundtrip_success = t;
    }

    pub fn take_send_error(&mut self) -> Option<String> {
        if self.send_error.is_empty() { None } else { Some(std::mem::take(&mut self.send_error)) }
    }

    pub fn local_port(&self) -> Option<u16> {
        self.socks.back().and_then(|s| s.socket.local_addr().ok()).map(|a| a.port())
    }
}
