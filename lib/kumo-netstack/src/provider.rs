//j841
//j845
//! The provider side of KNF1 (DESIGN/026 §3): the mirror of [`crate::frame::Lane`].
//!
//! A frame provider (the ax88 NIC owner in NET7b) speaks this state machine over the frame
//! channel: it opens each generation with HELLO, reports LINK, sends RX_FRAMEs within the
//! credit it granted, answers each accepted TX_FRAME with an in-order TX_DONE that returns
//! one TX credit, and answers QUERY_COUNTERS. Every rule here mirrors what netstack's Lane
//! enforces, so the pair can be exercised against each other in host tests.

use crate::budget::{FRAME_MAX, MTU, RX_CREDIT, TX_CREDIT};
use crate::frame::{Body, Counters, Frame, Hello, TxStatus, Violation};

/// The provider's side of one frame channel (one interface generation at a time).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Provider {
    generation: u32,
    next_in: u32,
    next_out: u32,
    rx_granted: u32,
    rx_outstanding: u32,
    tx_credit: u32,
    /// Sequences of the inbound TX_FRAMEs awaiting their TX_DONE, oldest at `tx_head`.
    /// CREDIT messages share the inbound sequence space, so consecutive TX sequences can
    /// have gaps; every answer echoes the sequence its frame actually carried.
    tx_sent: [u32; TX_CREDIT],
    tx_head: usize,
    tx_in_flight: u32,
    counters: Counters,
}

impl Default for Provider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider {
    pub const fn new() -> Self {
        Self {
            generation: 0,
            next_in: 0,
            next_out: 0,
            rx_granted: 0,
            rx_outstanding: 0,
            tx_credit: 0,
            tx_sent: [0; TX_CREDIT],
            tx_head: 0,
            tx_in_flight: 0,
            counters: Counters {
                rx_drop_no_credit: 0,
                rx_bad: 0,
                tx_drop: 0,
                tx_err: 0,
            },
        }
    }

    pub const fn generation(&self) -> u32 {
        self.generation
    }

    /// TX frames netstack may still send without a TX_DONE.
    pub const fn tx_credit(&self) -> u32 {
        self.tx_credit
    }

    /// RX frames sent and not yet returned by CREDIT.
    pub const fn rx_outstanding(&self) -> u32 {
        self.rx_outstanding
    }

    pub const fn counters(&self) -> Counters {
        self.counters
    }

    /// The answer for a QUERY_COUNTERS, and the counters message itself.
    pub fn counters_frame(&mut self) -> Option<Frame<'static>> {
        let counters = self.counters;
        self.outbound(Body::Counters(counters))
    }

    /// Start a new generation with HELLO. Every prior-generation rule and counter resets.
    /// The caller supplies the chip's unicast node address.
    pub fn hello(&mut self, mac: [u8; 6]) -> Frame<'static> {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.next_out = 2; // HELLO takes sequence 1
        self.next_in = 1; // netstack's first message is sequence 1
        self.rx_granted = RX_CREDIT as u32;
        self.rx_outstanding = 0;
        self.tx_credit = TX_CREDIT as u32;
        self.tx_sent = [0; TX_CREDIT];
        self.tx_head = 0;
        self.tx_in_flight = 0;
        self.counters = Counters {
            rx_drop_no_credit: 0,
            rx_bad: 0,
            tx_drop: 0,
            tx_err: 0,
        };
        Frame {
            generation: self.generation,
            sequence: 1,
            body: Body::Hello(Hello {
                mac,
                mtu: MTU as u16,
                max_frame: FRAME_MAX as u16,
                rx_credit_wanted: RX_CREDIT as u16,
                tx_credit_offered: TX_CREDIT as u16,
                flags: 0,
            }),
        }
    }

    /// A LINK state report for the current generation.
    pub fn link(&mut self, up: bool, full_duplex: bool, mbps: u32) -> Option<Frame<'static>> {
        self.outbound(Body::Link {
            up,
            full_duplex,
            mbps,
        })
    }

    /// One RX frame; `None` means the granted window is full: the frame is dropped and
    /// counted (`rx_drop_no_credit`), matching DESIGN/026 §3.
    pub fn send_rx<'a>(&mut self, frame: &'a [u8]) -> Option<Frame<'a>> {
        if self.generation == 0 || self.rx_outstanding >= self.rx_granted {
            self.counters.rx_drop_no_credit = self.counters.rx_drop_no_credit.saturating_add(1);
            return None;
        }
        let out = self.outbound(Body::RxFrame(frame))?;
        self.rx_outstanding += 1;
        Some(out)
    }

    /// The in-order TX_DONE for the oldest unacknowledged TX_FRAME, returning one credit
    /// and echoing the sequence that frame actually carried (CREDITs share the sequence
    /// space, so answers can be non-consecutive). `None` when no TX_FRAME is outstanding
    /// (a driver error, counted by the caller).
    pub fn tx_done(&mut self, status: TxStatus) -> Option<Frame<'static>> {
        if self.tx_in_flight == 0 {
            return None;
        }
        let answers = self.tx_sent[self.tx_head];
        let out = self.outbound(Body::TxDone { answers, status })?;
        self.tx_head = (self.tx_head + 1) % TX_CREDIT;
        self.tx_in_flight -= 1;
        match status {
            TxStatus::HandedToBus => {}
            TxStatus::Dropped => self.counters.tx_drop = self.counters.tx_drop.saturating_add(1),
            TxStatus::DeviceError => self.counters.tx_err = self.counters.tx_err.saturating_add(1),
        }
        Some(out)
    }

    /// Count one unparseable RX aggregate (driver-side inspection).
    pub fn note_rx_bad(&mut self) {
        self.counters.rx_bad = self.counters.rx_bad.saturating_add(1);
    }

    /// Check an inbound netstack message and apply its effect; `Err` means the channel must
    /// be closed and a fresh generation opened on a new channel.
    pub fn accept(&mut self, frame: &Frame<'_>) -> Result<(), Violation> {
        let op = frame.op();
        if op.from_provider() {
            return Err(Violation::WrongDirection);
        }
        if self.generation == 0 {
            return Err(Violation::NoHello);
        }
        if frame.generation != self.generation {
            return Err(Violation::StaleGeneration);
        }
        if frame.sequence != self.next_in {
            return Err(Violation::SequenceGap);
        }
        match frame.body {
            Body::TxFrame(_) => {
                if self.tx_in_flight >= self.tx_credit {
                    return Err(Violation::CreditOverrun);
                }
                let slot = (self.tx_head + self.tx_in_flight as usize) % TX_CREDIT;
                self.tx_sent[slot] = frame.sequence;
                self.tx_in_flight += 1;
            }
            Body::Credit(credit) => {
                if credit == 0 || credit > self.rx_granted || credit > self.rx_outstanding {
                    return Err(Violation::CreditOverrun);
                }
                self.rx_outstanding -= credit;
            }
            Body::QueryCounters => {}
            _ => return Err(Violation::WrongDirection),
        }
        self.next_in = self.next_in.wrapping_add(1);
        Ok(())
    }

    fn outbound<'a>(&mut self, body: Body<'a>) -> Option<Frame<'a>> {
        if self.generation == 0 {
            return None;
        }
        let sequence = self.next_out;
        self.next_out = self.next_out.wrapping_add(1);
        Some(Frame {
            generation: self.generation,
            sequence,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::frame::{decode, encode, Lane, Op};

    const MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 7];

    fn wire(frame: &Frame<'_>) -> std::vec::Vec<u8> {
        let mut out = [0u8; crate::frame::HEADER_LEN + FRAME_MAX];
        let len = encode(frame, &mut out).expect("encodes");
        out[..len].to_vec()
    }

    fn through_lane(lane: &mut Lane, frame: &Frame<'_>) {
        let bytes = wire(frame);
        let decoded = decode(&bytes, 0).expect("decodes");
        assert_eq!(decoded, *frame);
        lane.accept(&decoded).expect("lane accepts");
    }

    fn through_provider(provider: &mut Provider, frame: &Frame<'_>) {
        let bytes = wire(frame);
        let decoded = decode(&bytes, 0).expect("decodes");
        assert_eq!(decoded, *frame);
        provider.accept(&decoded).expect("provider accepts");
    }

    #[test]
    fn a_full_duplex_exchange_crosses_both_state_machines() {
        let mut provider = Provider::new();
        let mut lane = Lane::new();
        through_lane(&mut lane, &provider.hello(MAC));
        assert_eq!(lane.generation(), 1);
        assert_eq!(lane.tx_credit(), TX_CREDIT as u32);

        // LINK reaches netstack.
        through_lane(&mut lane, &provider.link(true, true, 1000).unwrap());

        // Provider RX spends credit, and only a CREDIT frees it.
        let rx: [u8; 60] = [0xAB; 60];
        for _ in 0..RX_CREDIT - 1 {
            through_lane(&mut lane, &provider.send_rx(&rx).unwrap());
        }
        through_lane(&mut lane, &provider.send_rx(&rx).unwrap());
        assert_eq!(provider.send_rx(&rx), None, "granted window full: drop");
        assert_eq!(provider.counters().rx_drop_no_credit, 1);
        through_provider(&mut provider, &lane.return_rx(4).unwrap());
        assert_eq!(provider.rx_outstanding(), (RX_CREDIT - 4) as u32);
        through_lane(&mut lane, &provider.send_rx(&rx).unwrap());

        // TX: netstack spends its credit; the provider answers in order with TX_DONE.
        let tx: [u8; 60] = [0xCD; 60];
        for _ in 0..TX_CREDIT {
            let frame = lane.send_tx(&tx).unwrap();
            through_provider(&mut provider, &frame);
        }
        assert_eq!(lane.send_tx(&tx), None, "TX credit exhausted: backpressure");
        for status in [
            TxStatus::HandedToBus,
            TxStatus::Dropped,
            TxStatus::DeviceError,
        ] {
            through_lane(&mut lane, &provider.tx_done(status).unwrap());
        }
        assert_eq!(provider.counters().tx_drop, 1);
        assert_eq!(provider.counters().tx_err, 1);
        assert_eq!(lane.tx_credit(), 3);
        through_provider(&mut provider, &lane.send_tx(&tx).unwrap());

        // Counters answer a query.
        provider.note_rx_bad();
        let query = Frame {
            generation: 1,
            sequence: provider.next_in,
            body: Body::QueryCounters,
        };
        through_provider(&mut provider, &query);
        let counters = provider.counters_frame().unwrap();
        assert_eq!(counters.op(), Op::Counters);
        through_lane(&mut lane, &counters);
    }

    #[test]
    fn provider_rejects_direction_generation_sequence_and_credit() {
        let mut provider = Provider::new();
        let stray = Frame {
            generation: 1,
            sequence: 1,
            body: Body::QueryCounters,
        };
        assert_eq!(provider.accept(&stray), Err(Violation::NoHello));
        let hello = provider.hello(MAC);
        assert_eq!(
            Provider::default().accept(&hello),
            Err(Violation::WrongDirection)
        );

        let tx = |generation, sequence| Frame {
            generation,
            sequence,
            body: Body::TxFrame(&[0x11; 60]),
        };
        assert_eq!(provider.accept(&tx(1, 2)), Err(Violation::SequenceGap));
        assert_eq!(provider.accept(&tx(9, 1)), Err(Violation::StaleGeneration));
        provider.accept(&tx(1, 1)).unwrap();
        let credit_bad = Frame {
            generation: 1,
            sequence: 2,
            body: Body::Credit(1),
        };
        assert_eq!(provider.accept(&credit_bad), Err(Violation::CreditOverrun));
        let replayed = provider.hello(MAC);
        assert_eq!(replayed.generation, 2);
        assert_eq!(replayed.sequence, 1);
        assert_eq!(provider.rx_outstanding(), 0);
        assert_eq!(provider.tx_credit(), TX_CREDIT as u32);
        assert_eq!(provider.accept(&tx(1, 1)), Err(Violation::StaleGeneration));
    }

    #[test]
    fn tx_dones_answer_the_real_sequence_across_interleaved_credits() {
        let mut provider = Provider::new();
        let mut lane = Lane::new();
        through_lane(&mut lane, &provider.hello(MAC));

        // TX at sequence 1, a CREDIT takes sequence 2, the next TX takes sequence 3.
        let tx: [u8; 60] = [0x33; 60];
        let rx: [u8; 60] = [0x44; 60];
        through_lane(&mut lane, &provider.send_rx(&rx).unwrap());
        through_provider(&mut provider, &lane.send_tx(&tx).unwrap());
        through_provider(&mut provider, &lane.return_rx(1).unwrap());
        through_provider(&mut provider, &lane.send_tx(&tx).unwrap());

        let first = provider.tx_done(TxStatus::HandedToBus).unwrap();
        let second = provider.tx_done(TxStatus::HandedToBus).unwrap();
        for (done, want) in [(first, 1u32), (second, 3u32)] {
            match done.body {
                Body::TxDone { answers, .. } => assert_eq!(answers, want),
                _ => panic!("not a TX_DONE"),
            }
        }
        assert!(provider.tx_done(TxStatus::HandedToBus).is_none());
        // The lane matches both answers against the sequences it sent.
        through_lane(&mut lane, &first);
        through_lane(&mut lane, &second);
    }

    #[test]
    fn a_tx_frame_beyond_the_offered_credit_is_a_violation() {
        let mut provider = Provider::new();
        provider.hello(MAC);
        // netstack cannot exceed what it granted itself: drive the raw frames by hand.
        let mut sequence = 1;
        for _ in 0..TX_CREDIT {
            let frame = Frame {
                generation: 1,
                sequence,
                body: Body::TxFrame(&[0x22; 60]),
            };
            provider.accept(&frame).unwrap();
            sequence += 1;
        }
        let over = Frame {
            generation: 1,
            sequence,
            body: Body::TxFrame(&[0x22; 60]),
        };
        assert_eq!(
            provider.accept(&over),
            Err(Violation::CreditOverrun),
            "TX beyond the offered credit"
        );
    }
}
