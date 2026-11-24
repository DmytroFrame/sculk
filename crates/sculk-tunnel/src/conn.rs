//! One tunnel session's reliable, ordered byte stream over datagrams, with no I/O of its own (the driver passes
//! datagrams and the clock in), so it can be tested on a made-up lossy link.
//!
//! What it does better than TCP on a lossy path (docs/docs/11-nether-tunnel.md, "Where the gain comes from"): every datagram
//! carries a selective acknowledgement of the 32 sequence numbers after the cumulative one, a segment the receiver
//! reported missing three times is sent again at once instead of after a timeout, and the retransmission timeout
//! follows the measured round trip down to 20 ms (TCP's minimum is usually 200 ms or more).

use crate::wire::{Kind, MSS, Segment, pack, unpack};
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// Segments in flight at most.
pub const WINDOW: usize = 256;
const INITIAL_RTO: Duration = Duration::from_millis(200);
const MIN_RTO: Duration = Duration::from_millis(20);
const MAX_RTO: Duration = Duration::from_secs(2);
/// Times a later segment was acknowledged before a missing one is sent again without waiting.
const FAST_RETRANSMIT: u8 = 3;

struct InFlight {
    payload: Vec<u8>,
    deflated: bool,
    sent_at: Instant,
    retries: u32,
    due: Instant,
    missed: u8,
}

pub struct Conn {
    session: u64,
    next_seq: u32,
    /// Segments not sent yet: the window is full.
    queue: VecDeque<(u32, Vec<u8>, bool)>,
    in_flight: BTreeMap<u32, InFlight>,
    recv_next: u32,
    out_of_order: BTreeMap<u32, Vec<u8>>,
    delivered: Vec<u8>,
    ack_due: bool,
    srtt: Option<f64>,
    rttvar: f64,
    rto: Duration,
    /// When the peer was last heard from.
    pub last_heard: Instant,
    /// Datagrams sent again (for tests and the log).
    pub retransmitted: u64,
}

impl Conn {
    pub fn new(session: u64, now: Instant) -> Self {
        Self {
            session,
            next_seq: 0,
            queue: VecDeque::new(),
            in_flight: BTreeMap::new(),
            recv_next: 0,
            out_of_order: BTreeMap::new(),
            delivered: Vec::new(),
            ack_due: false,
            srtt: None,
            rttvar: 0.0,
            rto: INITIAL_RTO,
            last_heard: now,
            retransmitted: 0,
        }
    }

    /// Bytes for the peer, in order.
    pub fn send(&mut self, data: &[u8]) {
        for chunk in data.chunks(MSS) {
            let (payload, deflated) = pack(chunk);
            self.queue.push_back((self.next_seq, payload, deflated));
            self.next_seq += 1;
        }
    }

    /// Segments sent and not acknowledged yet, and those waiting to be sent: what the driver throttles on.
    pub fn backlog(&self) -> usize {
        self.in_flight.len() + self.queue.len()
    }

    /// The peer's bytes that arrived in order since the last call.
    pub fn take_delivered(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.delivered)
    }

    /// A datagram from the peer.
    pub fn receive(&mut self, now: Instant, segment: Segment) {
        self.last_heard = now;
        if matches!(segment.kind, Kind::Data | Kind::Ack) {
            self.acknowledged(now, segment.ack, segment.sack);
        }
        if segment.kind != Kind::Data {
            return;
        }
        // Always acknowledged, a duplicate too: its acknowledgement may have been lost.
        self.ack_due = true;
        if segment.seq < self.recv_next || self.out_of_order.contains_key(&segment.seq) {
            return;
        }
        let Some(bytes) = unpack(&segment.payload, segment.deflated) else { return };
        self.out_of_order.insert(segment.seq, bytes);
        while let Some(bytes) = self.out_of_order.remove(&self.recv_next) {
            self.delivered.extend_from_slice(&bytes);
            self.recv_next += 1;
        }
    }

    fn acknowledged(&mut self, now: Instant, ack: u32, sack: u32) {
        let mut acked: Vec<u32> = self.in_flight.range(..ack).map(|(s, _)| *s).collect();
        let mut highest = None;
        for i in 0..32 {
            if sack & (1 << i) != 0 {
                let seq = ack + 1 + i;
                highest = Some(seq);
                if self.in_flight.contains_key(&seq) {
                    acked.push(seq);
                }
            }
        }
        for seq in acked {
            if let Some(f) = self.in_flight.remove(&seq)
                && f.retries == 0
            {
                // Karn's rule: only a segment sent once times the round trip.
                self.sample(now.saturating_duration_since(f.sent_at));
            }
        }
        // Later segments got through and this one did not: lost, most likely.
        if let Some(highest) = highest {
            for f in self.in_flight.range_mut(..highest).map(|(_, f)| f) {
                f.missed += 1;
                if f.missed >= FAST_RETRANSMIT {
                    f.missed = 0;
                    f.due = now;
                }
            }
        }
    }

    /// RFC 6298's estimate, with a lower floor.
    fn sample(&mut self, rtt: Duration) {
        let rtt = rtt.as_secs_f64();
        match self.srtt {
            None => {
                self.srtt = Some(rtt);
                self.rttvar = rtt / 2.0;
            }
            Some(srtt) => {
                self.rttvar = 0.75 * self.rttvar + 0.25 * (srtt - rtt).abs();
                self.srtt = Some(0.875 * srtt + 0.125 * rtt);
            }
        }
        let rto = self.srtt.unwrap_or_default() + 4.0 * self.rttvar;
        self.rto = Duration::from_secs_f64(rto).clamp(MIN_RTO, MAX_RTO);
    }

    /// What `ack` and `sack` say now.
    fn acknowledgement(&self) -> (u32, u32) {
        let mut sack = 0u32;
        for seq in self.out_of_order.range(self.recv_next + 1..=self.recv_next + 32).map(|(s, _)| *s) {
            sack |= 1 << (seq - self.recv_next - 1);
        }
        (self.recv_next, sack)
    }

    /// The datagrams to send now: segments due again, new ones the window lets out, or an acknowledgement alone.
    pub fn poll(&mut self, now: Instant) -> Vec<Segment> {
        let (ack, sack) = self.acknowledgement();
        let data = |seq: u32, payload: &[u8], deflated: bool| Segment {
            kind: Kind::Data,
            session: self.session,
            seq,
            ack,
            sack,
            payload: payload.to_vec(),
            deflated,
        };
        let mut out = Vec::new();
        let rto = self.rto;
        for (&seq, f) in self.in_flight.iter_mut().filter(|(_, f)| f.due <= now) {
            f.retries += 1;
            f.sent_at = now;
            f.due = now + (rto * 2u32.pow(f.retries.min(4))).min(MAX_RTO);
            self.retransmitted += 1;
            out.push(data(seq, &f.payload, f.deflated));
        }
        while self.in_flight.len() < WINDOW
            && let Some((seq, payload, deflated)) = self.queue.pop_front()
        {
            out.push(data(seq, &payload, deflated));
            self.in_flight
                .insert(seq, InFlight { payload, deflated, sent_at: now, retries: 0, due: now + rto, missed: 0 });
        }
        if out.is_empty() && self.ack_due {
            out.push(Segment { kind: Kind::Ack, ack, sack, ..Segment::control(Kind::Ack, self.session) });
        }
        self.ack_due = false;
        out
    }

    /// When `poll` has something to do next without anything arriving.
    pub fn next_timeout(&self) -> Option<Instant> {
        self.in_flight.values().map(|f| f.due).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny deterministic random source for the made-up link.
    struct Lcg(u64);
    impl Lcg {
        fn chance(&mut self, percent: u64) -> bool {
            self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % 100 < percent
        }
    }

    /// Two ends over a link that drops `loss` percent of datagrams and delays each by 15 ms (some 30 ms, so they
    /// arrive out of order); returns what each end received and how long it took.
    fn exchange(a_sends: &[u8], b_sends: &[u8], loss: u64) -> (Vec<u8>, Vec<u8>, Duration, u64) {
        let start = Instant::now();
        let (mut a, mut b) = (Conn::new(1, start), Conn::new(1, start));
        a.send(a_sends);
        b.send(b_sends);
        let mut random = Lcg(7);
        let mut wire: Vec<(Instant, bool, Segment)> = Vec::new();
        let (mut got_a, mut got_b) = (Vec::new(), Vec::new());
        let mut now = start;
        while (got_a.len() < b_sends.len() || got_b.len() < a_sends.len()) && now - start < Duration::from_secs(120) {
            for (to_b, conn) in [(true, &mut a), (false, &mut b)] {
                for segment in conn.poll(now) {
                    if !random.chance(loss) {
                        let delay = if random.chance(10) { 30 } else { 15 };
                        wire.push((now + Duration::from_millis(delay), to_b, segment));
                    }
                }
            }
            let (due, later): (Vec<_>, Vec<_>) = wire.into_iter().partition(|(at, _, _)| *at <= now);
            wire = later;
            for (_, to_b, segment) in due {
                if to_b { b.receive(now, segment) } else { a.receive(now, segment) }
            }
            got_a.extend(a.take_delivered());
            got_b.extend(b.take_delivered());
            now += Duration::from_millis(1);
        }
        (got_a, got_b, now - start, a.retransmitted + b.retransmitted)
    }

    fn stream(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed) ^ (i >> 7) as u8).collect()
    }

    #[test]
    fn both_streams_arrive_whole_and_in_order_over_a_lossy_link() {
        let (a_sends, b_sends) = (stream(300_000, 1), stream(120_000, 2));
        let (got_a, got_b, took, retransmitted) = exchange(&a_sends, &b_sends, 20);
        assert_eq!(got_b, a_sends);
        assert_eq!(got_a, b_sends);
        assert!(retransmitted > 0);
        assert!(took < Duration::from_secs(10), "took {took:?}");
    }

    #[test]
    fn a_clean_link_needs_no_retransmission() {
        let (got_a, got_b, _, retransmitted) = exchange(&stream(50_000, 3), &stream(10, 4), 0);
        assert_eq!(got_b, stream(50_000, 3));
        assert_eq!(got_a, stream(10, 4));
        assert_eq!(retransmitted, 0);
    }

    #[test]
    fn a_lost_segment_is_sent_again_before_its_timeout() {
        let start = Instant::now();
        let (mut a, mut b) = (Conn::new(1, start), Conn::new(1, start));
        a.send(&stream(MSS * 5, 5));
        let sent = a.poll(start);
        assert_eq!(sent.len(), 5);
        // The first is lost; the other four arrive and are acknowledged one by one.
        for segment in sent.into_iter().skip(1) {
            b.receive(start, segment);
            for ack in b.poll(start) {
                a.receive(start + Duration::from_millis(5), ack);
            }
        }
        let again = a.poll(start + Duration::from_millis(5));
        assert_eq!(again.iter().map(|s| s.seq).collect::<Vec<_>>(), vec![0], "sent again at once, not after 200 ms");
    }
}
