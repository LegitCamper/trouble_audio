//! Per-stream ISO SDU delivery counters.
//!
//! Hardware conformance runs need numbers, not just "audio arrived": a stream that drops 30% of
//! its SDUs still decodes frames and still logs successfully. [`SduStats`] classifies every
//! incoming SDU sequence number into delivered/lost/reordered/duplicate counts so a test harness
//! can assert a loss threshold.
//!
//! Sequence numbers are the controller's per-stream `Packet_Sequence_Number` (Core 6 Vol 4 Part E
//! §5.4.5), which increments once per SDU interval and wraps at 16 bits.

/// Half the 16-bit sequence space: deltas at or above this are treated as backwards (reordered or
/// duplicate) rather than as an enormous forward gap.
const WRAP_HALF: u16 = 0x8000;

/// Delivery counters for one CIS or BIS.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SduStats {
    /// SDUs delivered intact.
    pub received: u32,
    /// SDUs the controller explicitly flagged lost (`Packet_Status_Flag == 2`).
    pub flagged_lost: u32,
    /// SDUs never seen, inferred from forward jumps in the sequence number.
    pub missing: u32,
    /// SDUs that arrived with a sequence number already seen.
    pub duplicates: u32,
    /// SDUs that arrived behind the highest sequence number seen, but were not duplicates.
    pub reordered: u32,
    /// Largest single run of inferred-missing SDUs.
    pub max_burst_missing: u32,
    /// Highest sequence number observed so far.
    last_sequence: Option<u16>,
}

impl SduStats {
    /// Records one intact SDU.
    pub fn note_received(&mut self, sequence_number: u16) {
        self.received = self.received.saturating_add(1);
        self.track(sequence_number);
    }

    /// Records one SDU the controller reported as lost. Its sequence number still advances the
    /// stream, so it must not also be counted as inferred-missing.
    pub fn note_lost(&mut self, sequence_number: u16) {
        self.flagged_lost = self.flagged_lost.saturating_add(1);
        self.track(sequence_number);
    }

    fn track(&mut self, sequence_number: u16) {
        let Some(last) = self.last_sequence else {
            self.last_sequence = Some(sequence_number);
            return;
        };
        let delta = sequence_number.wrapping_sub(last);
        if delta == 0 {
            self.duplicates = self.duplicates.saturating_add(1);
        } else if delta >= WRAP_HALF {
            self.reordered = self.reordered.saturating_add(1);
        } else {
            let missing = u32::from(delta - 1);
            self.missing = self.missing.saturating_add(missing);
            self.max_burst_missing = self.max_burst_missing.max(missing);
            self.last_sequence = Some(sequence_number);
        }
    }

    /// Total SDUs the stream should have delivered over the observed sequence span.
    pub fn expected(&self) -> u32 {
        self.received
            .saturating_add(self.flagged_lost)
            .saturating_add(self.missing)
    }

    /// Loss in tenths of a percent (per mille) over the observed span. Zero when nothing arrived.
    ///
    /// Integer math keeps this usable in a `no_std` interrupt-adjacent path.
    pub fn loss_per_mille(&self) -> u32 {
        let expected = self.expected();
        if expected == 0 {
            return 0;
        }
        let lost = self.flagged_lost.saturating_add(self.missing);
        (lost.saturating_mul(1_000)) / expected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contiguous_sdus_report_no_loss() {
        let mut stats = SduStats::default();
        for seq in 0..100u16 {
            stats.note_received(seq);
        }
        assert_eq!(stats.received, 100);
        assert_eq!(stats.missing, 0);
        assert_eq!(stats.loss_per_mille(), 0);
    }

    #[test]
    fn forward_jump_infers_missing_sdus_and_burst_length() {
        let mut stats = SduStats::default();
        stats.note_received(10);
        stats.note_received(14);
        stats.note_received(15);
        stats.note_received(18);
        assert_eq!(stats.received, 4);
        assert_eq!(stats.missing, 5);
        assert_eq!(stats.max_burst_missing, 3);
    }

    #[test]
    fn repeated_sequence_number_counts_as_duplicate_not_loss() {
        let mut stats = SduStats::default();
        stats.note_received(7);
        stats.note_received(7);
        assert_eq!(stats.duplicates, 1);
        assert_eq!(stats.missing, 0);
        assert_eq!(stats.loss_per_mille(), 0);
    }

    #[test]
    fn backwards_sequence_number_counts_as_reordered() {
        let mut stats = SduStats::default();
        stats.note_received(20);
        stats.note_received(19);
        assert_eq!(stats.reordered, 1);
        assert_eq!(stats.missing, 0);
    }

    #[test]
    fn sequence_number_wrap_is_not_a_gap() {
        let mut stats = SduStats::default();
        stats.note_received(u16::MAX - 1);
        stats.note_received(u16::MAX);
        stats.note_received(0);
        stats.note_received(1);
        assert_eq!(stats.received, 4);
        assert_eq!(stats.missing, 0);
    }

    #[test]
    fn controller_flagged_loss_is_counted_once() {
        let mut stats = SduStats::default();
        stats.note_received(0);
        stats.note_lost(1);
        stats.note_received(2);
        assert_eq!(stats.flagged_lost, 1);
        assert_eq!(stats.missing, 0);
        assert_eq!(stats.expected(), 3);
        assert_eq!(stats.loss_per_mille(), 333);
    }
}
