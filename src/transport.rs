// SPDX-License-Identifier: GPL-3.0-or-later
// mosh-win: Copyright 2026 the mosh-win authors.
// Ported from mosh (https://mosh.org): Copyright 2012 Keith Winstein and the mosh authors.

//! State-synchronization transport (port of mosh's TransportSender,
//! Transport::recv, and the fragment layer), specialized for the client:
//! we send a `UserStream` and receive `Complete` terminal states.

use std::collections::VecDeque;
use std::io::{Read, Write};

use prost::Message;
use rand::Rng;

use crate::complete::Complete;
use crate::network::{self, Connection, MOSH_PROTOCOL_VERSION};
use crate::proto::Instruction;
use crate::timestamp::timestamp;
use crate::user::UserStream;

/* timing parameters */
pub const SEND_INTERVAL_MIN: u64 = 20;
pub const SEND_INTERVAL_MAX: u64 = 250;
pub const ACK_INTERVAL: u64 = 3000;
pub const ACK_DELAY: u64 = 100;
pub const SHUTDOWN_RETRIES: i32 = 16;
pub const ACTIVE_RETRY_TIMEOUT: u64 = 10000;

const FRAG_HEADER_LEN: usize = 10;
const MAX_DECOMPRESSED: u64 = 16 * 1024 * 1024;

fn compress(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn uncompress(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data).take(MAX_DECOMPRESSED).read_to_end(&mut out).ok()?;
    Some(out)
}

/* ---------------- fragments ---------------- */

#[derive(Clone, PartialEq, Default)]
struct Fragment {
    id: u64,
    fragment_num: u16,
    final_: bool,
    contents: Vec<u8>,
}

impl Fragment {
    fn to_bytes(&self) -> Vec<u8> {
        let mut ret = Vec::with_capacity(FRAG_HEADER_LEN + self.contents.len());
        ret.extend_from_slice(&self.id.to_be_bytes());
        let combined = ((self.final_ as u16) << 15) | self.fragment_num;
        ret.extend_from_slice(&combined.to_be_bytes());
        ret.extend_from_slice(&self.contents);
        ret
    }

    fn parse(x: &[u8]) -> Option<Fragment> {
        if x.len() < FRAG_HEADER_LEN {
            return None;
        }
        let id = u64::from_be_bytes(x[..8].try_into().unwrap());
        let n = u16::from_be_bytes([x[8], x[9]]);
        Some(Fragment { id, fragment_num: n & 0x7FFF, final_: n & 0x8000 != 0, contents: x[10..].to_vec() })
    }
}

#[derive(Default)]
struct FragmentAssembly {
    fragments: Vec<Option<Fragment>>,
    current_id: Option<u64>,
    fragments_arrived: usize,
    fragments_total: Option<usize>,
}

impl FragmentAssembly {
    fn add_fragment(&mut self, frag: Fragment) -> bool {
        let num = frag.fragment_num as usize;
        if self.current_id != Some(frag.id) {
            self.fragments.clear();
            self.fragments.resize(num + 1, None);
            self.fragments_arrived = 1;
            self.fragments_total = None;
            self.current_id = Some(frag.id);
            if frag.final_ {
                self.fragments_total = Some(num + 1);
            }
            self.fragments[num] = Some(frag);
        } else {
            if self.fragments.len() > num && self.fragments[num].is_some() {
                // duplicate
            } else {
                if self.fragments.len() < num + 1 {
                    self.fragments.resize(num + 1, None);
                }
                if frag.final_ {
                    self.fragments_total = Some(num + 1);
                }
                self.fragments[num] = Some(frag);
                self.fragments_arrived += 1;
            }
        }
        if let Some(total) = self.fragments_total {
            if self.fragments.len() > total {
                // a fragment numbered past the final one: malformed, start over
                self.fragments.truncate(total);
                self.fragments_arrived = self.fragments.iter().filter(|f| f.is_some()).count();
            }
        }
        Some(self.fragments_arrived) == self.fragments_total
    }

    fn get_assembly(&mut self) -> Option<Instruction> {
        let mut encoded = Vec::new();
        for f in &self.fragments {
            encoded.extend_from_slice(&f.as_ref()?.contents);
        }
        self.fragments.clear();
        self.fragments_arrived = 0;
        self.fragments_total = None;
        let raw = uncompress(&encoded)?;
        Instruction::decode(raw.as_slice()).ok()
    }
}

#[derive(Default)]
struct Fragmenter {
    next_instruction_id: u64,
    last_instruction: Instruction,
    last_mtu: Option<usize>,
}

impl Fragmenter {
    fn make_fragments(&mut self, inst: &Instruction, mtu: usize) -> Vec<Fragment> {
        let mtu = mtu - FRAG_HEADER_LEN;
        let l = &self.last_instruction;
        if inst.old_num != l.old_num
            || inst.new_num != l.new_num
            || inst.ack_num != l.ack_num
            || inst.throwaway_num != l.throwaway_num
            || inst.chaff != l.chaff
            || inst.protocol_version != l.protocol_version
            || self.last_mtu != Some(mtu)
        {
            self.next_instruction_id += 1;
        }
        self.last_instruction = inst.clone();
        self.last_mtu = Some(mtu);

        let payload = compress(&inst.encode_to_vec());
        let mut ret = Vec::new();
        let chunks: Vec<&[u8]> = payload.chunks(mtu).collect();
        let n = chunks.len();
        for (i, c) in chunks.into_iter().enumerate() {
            ret.push(Fragment {
                id: self.next_instruction_id,
                fragment_num: i as u16,
                final_: i + 1 == n,
                contents: c.to_vec(),
            });
        }
        ret
    }

    fn last_ack_sent(&self) -> u64 {
        self.last_instruction.ack_num.unwrap_or(0)
    }
}

/* ---------------- sender ---------------- */

#[derive(Clone)]
struct TimestampedState<S> {
    timestamp: u64,
    num: u64,
    state: S,
}

pub struct TransportSender {
    current_state: UserStream,
    sent_states: VecDeque<TimestampedState<UserStream>>,
    assumed_receiver_state: usize,
    fragmenter: Fragmenter,
    next_ack_time: u64,
    next_send_time: u64,
    shutdown_in_progress: bool,
    shutdown_tries: i32,
    shutdown_start: u64,
    ack_num: u64,
    pending_data_ack: bool,
    send_mindelay: u64,
    last_heard: u64,
    mindelay_clock: u64,
}

impl TransportSender {
    fn new(initial: UserStream) -> Self {
        let now = timestamp();
        TransportSender {
            current_state: initial.clone(),
            sent_states: VecDeque::from([TimestampedState { timestamp: now, num: 0, state: initial }]),
            assumed_receiver_state: 0,
            fragmenter: Fragmenter::default(),
            next_ack_time: now,
            next_send_time: now,
            shutdown_in_progress: false,
            shutdown_tries: 0,
            shutdown_start: u64::MAX,
            ack_num: 0,
            pending_data_ack: false,
            send_mindelay: 8,
            last_heard: 0,
            mindelay_clock: u64::MAX,
        }
    }

    pub fn send_interval(&self, conn: &Connection) -> u64 {
        ((conn.srtt() / 2.0).ceil() as u64).clamp(SEND_INTERVAL_MIN, SEND_INTERVAL_MAX)
    }

    fn calculate_timers(&mut self, conn: &Connection) {
        let now = timestamp();
        self.update_assumed_receiver_state(conn);
        self.rationalize_states();

        if self.pending_data_ack && self.next_ack_time > now + ACK_DELAY {
            self.next_ack_time = now + ACK_DELAY;
        }

        let back = self.sent_states.back().unwrap();
        if self.current_state != back.state {
            if self.mindelay_clock == u64::MAX {
                self.mindelay_clock = now;
            }
            self.next_send_time =
                (self.mindelay_clock + self.send_mindelay).max(back.timestamp + self.send_interval(conn));
        } else if self.current_state != self.sent_states[self.assumed_receiver_state].state
            && self.last_heard + ACTIVE_RETRY_TIMEOUT > now
        {
            self.next_send_time = back.timestamp + self.send_interval(conn);
            if self.mindelay_clock != u64::MAX {
                self.next_send_time = self.next_send_time.max(self.mindelay_clock + self.send_mindelay);
            }
        } else if self.current_state != self.sent_states.front().unwrap().state
            && self.last_heard + ACTIVE_RETRY_TIMEOUT > now
        {
            self.next_send_time = back.timestamp + conn.timeout() + ACK_DELAY;
        } else {
            self.next_send_time = u64::MAX;
        }

        /* speed up shutdown sequence */
        if self.shutdown_in_progress || self.ack_num == u64::MAX {
            self.next_ack_time = self.sent_states.back().unwrap().timestamp + self.send_interval(conn);
        }
    }

    fn wait_time(&mut self, conn: &Connection) -> u64 {
        self.calculate_timers(conn);
        let next_wakeup = self.next_ack_time.min(self.next_send_time);
        next_wakeup.saturating_sub(timestamp())
    }

    fn tick(&mut self, conn: &mut Connection) {
        self.calculate_timers(conn);
        let now = timestamp();
        if now < self.next_ack_time && now < self.next_send_time {
            return;
        }

        let mut diff = self.current_state.diff_from(&self.sent_states[self.assumed_receiver_state].state);
        self.attempt_prospective_resend_optimization(&mut diff);

        if diff.is_empty() {
            if now >= self.next_ack_time {
                self.send_empty_ack(conn);
                self.mindelay_clock = u64::MAX;
            }
            if now >= self.next_send_time {
                self.next_send_time = u64::MAX;
                self.mindelay_clock = u64::MAX;
            }
        } else if now >= self.next_send_time || now >= self.next_ack_time {
            self.send_to_receiver(conn, diff);
            self.mindelay_clock = u64::MAX;
        }
    }

    fn send_empty_ack(&mut self, conn: &mut Connection) {
        let now = timestamp();
        let mut new_num = self.sent_states.back().unwrap().num.wrapping_add(1);
        if self.shutdown_in_progress {
            new_num = u64::MAX;
        }
        let state = self.current_state.clone();
        self.add_sent_state(now, new_num, state);
        self.send_in_fragments(conn, Vec::new(), new_num);
        self.next_ack_time = now + ACK_INTERVAL;
        self.next_send_time = u64::MAX;
    }

    fn add_sent_state(&mut self, ts: u64, num: u64, state: UserStream) {
        self.sent_states.push_back(TimestampedState { timestamp: ts, num, state });
        if self.sent_states.len() > 32 {
            /* erase state from middle of queue */
            let idx = self.sent_states.len() - 16;
            self.sent_states.remove(idx);
            if self.assumed_receiver_state >= self.sent_states.len() {
                self.assumed_receiver_state = self.sent_states.len() - 1;
            }
        }
    }

    fn send_to_receiver(&mut self, conn: &mut Connection, diff: Vec<u8>) {
        let back_num = self.sent_states.back().unwrap().num;
        let mut new_num = if self.current_state == self.sent_states.back().unwrap().state {
            back_num
        } else {
            back_num.wrapping_add(1)
        };
        if self.shutdown_in_progress {
            new_num = u64::MAX;
        }
        if new_num == back_num {
            self.sent_states.back_mut().unwrap().timestamp = timestamp();
        } else {
            let state = self.current_state.clone();
            self.add_sent_state(timestamp(), new_num, state);
        }
        self.send_in_fragments(conn, diff, new_num);
        self.assumed_receiver_state = self.sent_states.len() - 1;
        self.next_ack_time = timestamp() + ACK_INTERVAL;
        self.next_send_time = u64::MAX;
    }

    fn update_assumed_receiver_state(&mut self, conn: &Connection) {
        let now = timestamp();
        self.assumed_receiver_state = 0;
        for i in 1..self.sent_states.len() {
            if now.wrapping_sub(self.sent_states[i].timestamp) < conn.timeout() + ACK_DELAY {
                self.assumed_receiver_state = i;
            } else {
                return;
            }
        }
    }

    fn rationalize_states(&mut self) {
        let known = self.sent_states.front().unwrap().state.clone();
        self.current_state.subtract(&known);
        for s in self.sent_states.iter_mut() {
            s.state.subtract(&known);
        }
    }

    fn make_chaff() -> Vec<u8> {
        let mut rng = rand::thread_rng();
        let len = rng.gen_range(0..=16);
        (0..len).map(|_| rng.r#gen::<u8>()).collect()
    }

    fn send_in_fragments(&mut self, conn: &mut Connection, diff: Vec<u8>, new_num: u64) {
        let inst = Instruction {
            protocol_version: Some(MOSH_PROTOCOL_VERSION),
            old_num: Some(self.sent_states[self.assumed_receiver_state].num),
            new_num: Some(new_num),
            ack_num: Some(self.ack_num),
            throwaway_num: Some(self.sent_states.front().unwrap().num),
            diff: Some(diff),
            chaff: Some(Self::make_chaff()),
        };
        if new_num == u64::MAX {
            self.shutdown_tries += 1;
        }
        let mtu = conn.mtu() - network::ADDED_BYTES - network::CRYPTO_ADDED_BYTES;
        for f in self.fragmenter.make_fragments(&inst, mtu) {
            conn.send(&f.to_bytes());
        }
        self.pending_data_ack = false;
    }

    fn process_acknowledgment_through(&mut self, ack_num: u64) {
        if self.sent_states.iter().any(|s| s.num == ack_num) {
            self.sent_states.retain(|s| s.num >= ack_num);
            if self.assumed_receiver_state >= self.sent_states.len() {
                self.assumed_receiver_state = self.sent_states.len() - 1;
            }
        }
    }

    fn attempt_prospective_resend_optimization(&mut self, proposed_diff: &mut Vec<u8>) {
        if self.assumed_receiver_state == 0 {
            return;
        }
        let resend_diff = self.current_state.diff_from(&self.sent_states.front().unwrap().state);
        if resend_diff.len() <= proposed_diff.len()
            || (resend_diff.len() < 1000 && resend_diff.len() - proposed_diff.len() < 100)
        {
            self.assumed_receiver_state = 0;
            *proposed_diff = resend_diff;
        }
    }

    fn shutdown_ack_timed_out(&self) -> bool {
        self.shutdown_in_progress
            && (self.shutdown_tries >= SHUTDOWN_RETRIES
                || timestamp().wrapping_sub(self.shutdown_start) >= ACTIVE_RETRY_TIMEOUT)
    }
}

/* ---------------- transport ---------------- */

pub struct Transport {
    pub connection: Connection,
    sender: TransportSender,
    received_states: Vec<TimestampedState<Complete>>,
    receiver_quench_timer: u64,
    fragments: FragmentAssembly,
}

impl Transport {
    pub fn new(connection: Connection, initial: UserStream, initial_remote: Complete) -> Self {
        Transport {
            connection,
            sender: TransportSender::new(initial),
            received_states: vec![TimestampedState { timestamp: timestamp(), num: 0, state: initial_remote }],
            receiver_quench_timer: 0,
            fragments: FragmentAssembly::default(),
        }
    }

    /// Process one datagram. Errors are ignored silently like mosh's dos_assert path.
    pub fn recv(&mut self, datagram: &[u8]) -> Result<(), String> {
        let Some(payload) = self.connection.recv(datagram) else { return Ok(()) };
        let Some(frag) = Fragment::parse(&payload) else { return Ok(()) };
        if !self.fragments.add_fragment(frag) {
            return Ok(());
        }
        let Some(inst) = self.fragments.get_assembly() else { return Ok(()) };

        if inst.protocol_version.unwrap_or(0) != MOSH_PROTOCOL_VERSION {
            return Err("mosh protocol version mismatch".into());
        }
        let new_num = inst.new_num.unwrap_or(0);
        let old_num = inst.old_num.unwrap_or(0);

        self.sender.process_acknowledgment_through(inst.ack_num.unwrap_or(0));
        /* inform network layer of roundtrip (end-to-end-to-end) connectivity */
        self.connection.set_last_roundtrip_success(self.sender.sent_states.front().unwrap().timestamp);

        /* first, make sure we don't already have the new state */
        if self.received_states.iter().any(|s| s.num == new_num) {
            return Ok(());
        }
        /* now, make sure we do have the old state */
        let Some(reference) = self.received_states.iter().position(|s| s.num == old_num) else {
            return Ok(());
        };

        self.process_throwaway_until(inst.throwaway_num.unwrap_or(0));
        let reference = match self.received_states.iter().position(|s| s.num == old_num) {
            Some(r) => r,
            None => {
                let _ = reference;
                return Ok(());
            }
        };

        if self.received_states.len() > 1024 {
            let now = timestamp();
            if now < self.receiver_quench_timer {
                return Ok(());
            }
            self.receiver_quench_timer = now + 15000;
        }

        /* apply diff to reference state */
        let mut new_state = self.received_states[reference].clone();
        new_state.timestamp = timestamp();
        new_state.num = new_num;
        let diff = inst.diff.unwrap_or_default();
        if !diff.is_empty() {
            new_state.state.apply_string(&diff);
        }

        /* Insert new state in sorted place */
        if let Some(pos) = self.received_states.iter().position(|s| s.num > new_num) {
            self.received_states.insert(pos, new_state);
            return Ok(());
        }
        let ts = new_state.timestamp;
        self.received_states.push(new_state);
        self.sender.ack_num = new_num;
        self.sender.last_heard = ts;
        if !diff.is_empty() {
            self.sender.pending_data_ack = true;
        }
        Ok(())
    }

    fn process_throwaway_until(&mut self, throwaway_num: u64) {
        self.received_states.retain(|s| s.num >= throwaway_num);
        assert!(!self.received_states.is_empty());
    }

    pub fn tick(&mut self) {
        self.sender.tick(&mut self.connection);
    }

    pub fn wait_time(&mut self) -> u64 {
        self.sender.wait_time(&self.connection)
    }

    pub fn current_state_mut(&mut self) -> &mut UserStream {
        debug_assert!(!self.sender.shutdown_in_progress);
        &mut self.sender.current_state
    }

    pub fn latest_remote_state(&self) -> (&Complete, u64, u64) {
        let s = self.received_states.last().unwrap();
        (&s.state, s.timestamp, s.num)
    }

    pub fn set_send_delay(&mut self, d: u64) {
        self.sender.send_mindelay = d;
    }

    pub fn send_interval(&self) -> u64 {
        self.sender.send_interval(&self.connection)
    }

    pub fn start_shutdown(&mut self) {
        if !self.sender.shutdown_in_progress {
            self.sender.shutdown_start = timestamp();
            self.sender.shutdown_in_progress = true;
        }
    }

    pub fn shutdown_in_progress(&self) -> bool {
        self.sender.shutdown_in_progress
    }

    pub fn shutdown_acknowledged(&self) -> bool {
        self.sender.sent_states.front().unwrap().num == u64::MAX
    }

    pub fn shutdown_ack_timed_out(&self) -> bool {
        self.sender.shutdown_ack_timed_out()
    }

    pub fn counterparty_shutdown_ack_sent(&self) -> bool {
        self.sender.fragmenter.last_ack_sent() == u64::MAX
    }

    pub fn sent_state_acked_timestamp(&self) -> u64 {
        self.sender.sent_states.front().unwrap().timestamp
    }

    pub fn sent_state_acked(&self) -> u64 {
        self.sender.sent_states.front().unwrap().num
    }

    pub fn sent_state_last(&self) -> u64 {
        self.sender.sent_states.back().unwrap().num
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragment_roundtrip() {
        let mut fr = Fragmenter::default();
        let inst = Instruction {
            protocol_version: Some(2),
            old_num: Some(1),
            new_num: Some(2),
            ack_num: Some(3),
            throwaway_num: Some(0),
            diff: Some((0..5000u32).map(|i| (i * 7919 % 251) as u8).collect()),
            chaff: Some(vec![]),
        };
        let frags = fr.make_fragments(&inst, 300);
        assert!(frags.len() > 1);
        let mut asm = FragmentAssembly::default();
        let mut done = false;
        for f in frags.iter().rev() {
            done = asm.add_fragment(Fragment::parse(&f.to_bytes()).unwrap());
        }
        assert!(done);
        assert_eq!(asm.get_assembly().unwrap(), inst);
    }
}
