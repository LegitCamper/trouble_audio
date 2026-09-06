//! Reassembles incoming HCI ISO Data packets into complete ISO SDUs per Core 6 Vol 4 Part E
//! §5.4.5 - the peripheral/sink-side mirror of `iso_tx.rs`'s outgoing fragmentation. `cis.rs`'s
//! `CisManager::on_iso_data` (per-CIS) and `big_sink.rs`'s `BigSink::on_iso_data` (per-BIS) both
//! feed every packet on their stream through one [`SduReassembler`], keyed by ASE/BIS slot index;
//! only the wire format is shared here; ASE/CIS/BIS bookkeeping and what to do with a finished
//! SDU stay in those modules.
//!
//! [`SduReassembler::push`] never logs - callers report a [`SduDrop`] however fits their own
//! prefix/rate-limiting (e.g. `CisManager::note_lost_sdu`'s throttled warning on the controller's
//! `Packet_Status_Flag == 2`, Core 6 Vol 4 Part E §5.4.5).

use bt_hci::data::{IsoPacket, IsoPacketBoundary};
use heapless::Vec as HVec;

/// Largest encoded ISO SDU this crate retains.
///
/// The bound matches Sony's `LDACBT_MAX_NBYTES`; LC3 frames are substantially smaller.
pub const MAX_ENCODED_AUDIO_SDU_LEN: usize = crate::ldac::MAX_TRANSPORT_FRAME_SEQUENCE_BYTES;

/// One complete, still-encoded ISO SDU from any negotiated audio codec.
pub type EncodedAudioSdu = HVec<u8, MAX_ENCODED_AUDIO_SDU_LEN>;

/// `Packet_Status_Flag` occupies the top 2 bits of `ISO_SDU_Length`'s octet pair.
const PACKET_STATUS_SHIFT: u32 = 14;
const ISO_SDU_LENGTH_MASK: u16 = 0x0fff;

/// Per-stream reassembly state for one CIS or BIS. `push` every HCI ISO Data packet for that
/// stream, in arrival order.
#[derive(Default)]
pub struct SduReassembler {
    /// `Some` once a First/Complete fragment has started an SDU; doubles as the "have we seen a
    /// first fragment" flag Continuation/Last fragments check against.
    sequence_number: Option<u16>,
    timestamp_us: Option<u32>,
    expected_len: usize,
    data: EncodedAudioSdu,
}

/// A fully reassembled SDU, as returned by [`SduReassembler::push`].
#[derive(Debug)]
pub struct ReassembledSdu {
    pub sequence_number: u16,
    /// Present only when the controller's first HCI packet of the SDU carried a timestamp.
    pub timestamp_us: Option<u32>,
    pub data: EncodedAudioSdu,
}

/// Why one HCI ISO Data packet did not yield a [`ReassembledSdu`]. Carries no log message -
/// callers decide wording/rate-limiting themselves.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SduDrop {
    /// A Complete/First fragment packet had no `ISO_Data_Load` header. Unreachable through
    /// `IsoPacket`'s own wire parser (it always attaches one for those boundary flags), but
    /// preserved as a guard for whatever constructs one in the future.
    MissingDataLoadHeader,
    /// `Packet_Status_Flag == 2`: the controller reported the SDU as lost.
    Lost { sequence_number: u16 },
    /// `Packet_Status_Flag` was neither 0 (valid) nor 2 (lost).
    InvalidStatus { sequence_number: u16, status: u16 },
    /// The declared `ISO_SDU_Length` exceeds [`MAX_ENCODED_AUDIO_SDU_LEN`], or this packet's
    /// payload alone is already longer than the SDU it declares.
    DeclaredLengthUnsupported,
    /// A Continuation/Last fragment arrived with no First fragment open for this stream.
    ContinuationWithoutStart,
    /// Accumulated fragment data would exceed (or already exceeded) the declared `ISO_SDU_Length`.
    ExceededDeclaredLength,
    /// The SDU that a Complete or Last-fragment packet was supposed to finish came up short (or
    /// long) of its declared length.
    IncompleteAtLastFragment,
}

impl SduReassembler {
    /// Discards any in-progress fragment, e.g. on ACL/CIS/BIG disconnect.
    pub fn clear(&mut self) {
        self.sequence_number = None;
        self.timestamp_us = None;
        self.expected_len = 0;
        self.data.clear();
    }

    /// Feeds one HCI ISO Data packet into this stream's reassembly. `Ok(Some(sdu))` when this
    /// packet completed one, `Ok(None)` while fragments are still pending, `Err(reason)` when the
    /// packet was dropped (any in-progress fragment is cleared first, so a bad packet can't
    /// corrupt the next SDU).
    pub fn push(&mut self, packet: &IsoPacket<'_>) -> Result<Option<ReassembledSdu>, SduDrop> {
        match packet.boundary_flag() {
            IsoPacketBoundary::Complete | IsoPacketBoundary::FirstFragment => {
                let Some(header) = packet.data_load_header() else {
                    return Err(SduDrop::MissingDataLoadHeader);
                };
                let packet_status = header.iso_sdu_len >> PACKET_STATUS_SHIFT;
                if packet_status == 2 {
                    self.clear();
                    return Err(SduDrop::Lost { sequence_number: header.sequence_num });
                }
                if packet_status != 0 {
                    self.clear();
                    return Err(SduDrop::InvalidStatus {
                        sequence_number: header.sequence_num,
                        status: packet_status,
                    });
                }

                let declared_length = usize::from(header.iso_sdu_len & ISO_SDU_LENGTH_MASK);
                if declared_length > MAX_ENCODED_AUDIO_SDU_LEN || packet.data().len() > declared_length {
                    self.clear();
                    return Err(SduDrop::DeclaredLengthUnsupported);
                }

                let Ok(data) = EncodedAudioSdu::from_slice(packet.data()) else {
                    // Can't happen given the length checks above, but don't corrupt any
                    // in-progress fragment on the (impossible) failure path.
                    return Ok(None);
                };

                if packet.boundary_flag() == IsoPacketBoundary::Complete {
                    self.clear();
                    if data.len() != declared_length {
                        return Err(SduDrop::IncompleteAtLastFragment);
                    }
                    return Ok(Some(ReassembledSdu {
                        sequence_number: header.sequence_num,
                        timestamp_us: header.timestamp,
                        data,
                    }));
                }

                self.sequence_number = Some(header.sequence_num);
                self.timestamp_us = header.timestamp;
                self.expected_len = declared_length;
                self.data = data;
                Ok(None)
            }
            IsoPacketBoundary::ContinuationFragment | IsoPacketBoundary::LastFragment => {
                let Some(sequence_number) = self.sequence_number else {
                    return Err(SduDrop::ContinuationWithoutStart);
                };
                if self.data.len().saturating_add(packet.data().len()) > self.expected_len
                    || self.data.extend_from_slice(packet.data()).is_err()
                {
                    self.clear();
                    return Err(SduDrop::ExceededDeclaredLength);
                }
                if packet.boundary_flag() == IsoPacketBoundary::ContinuationFragment {
                    return Ok(None);
                }
                if self.data.len() != self.expected_len {
                    self.clear();
                    return Err(SduDrop::IncompleteAtLastFragment);
                }
                let timestamp_us = self.timestamp_us;
                let data = core::mem::take(&mut self.data);
                self.clear();
                Ok(Some(ReassembledSdu { sequence_number, timestamp_us, data }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bt_hci::FromHciBytes;

    use super::*;

    /// Hand-encodes one non-timestamped HCI ISO Data packet - `IsoPacket` has no public
    /// constructor other than parsing from wire bytes (mirrors `iso_tx.rs`'s own trick, and
    /// `cis.rs`/`big_sink.rs`'s synthetic-packet test helpers).
    fn packet_bytes(
        handle: u16,
        boundary: IsoPacketBoundary,
        sequence_number: u16,
        declared_sdu_len: u16,
        packet_status: u16,
        payload: &[u8],
    ) -> alloc::vec::Vec<u8> {
        let pb = match boundary {
            IsoPacketBoundary::FirstFragment => 0u16,
            IsoPacketBoundary::ContinuationFragment => 1,
            IsoPacketBoundary::Complete => 2,
            IsoPacketBoundary::LastFragment => 3,
        };
        let has_header = matches!(boundary, IsoPacketBoundary::FirstFragment | IsoPacketBoundary::Complete);
        let handle_word = (handle & 0x0fff) | (pb << 12);
        let mut out = alloc::vec::Vec::new();
        out.extend_from_slice(&handle_word.to_le_bytes());
        let data_load_len = if has_header { 4 + payload.len() } else { payload.len() };
        out.extend_from_slice(&(data_load_len as u16).to_le_bytes());
        if has_header {
            out.extend_from_slice(&sequence_number.to_le_bytes());
            let iso_sdu_len = (declared_sdu_len & ISO_SDU_LENGTH_MASK) | (packet_status << PACKET_STATUS_SHIFT);
            out.extend_from_slice(&iso_sdu_len.to_le_bytes());
        }
        out.extend_from_slice(payload);
        out
    }

    fn push_bytes(reassembler: &mut SduReassembler, bytes: &[u8]) -> Result<Option<ReassembledSdu>, SduDrop> {
        let (packet, rest) = IsoPacket::from_hci_bytes(bytes).unwrap();
        assert!(rest.is_empty());
        reassembler.push(&packet)
    }

    #[test]
    fn complete_packet_yields_an_sdu_immediately() {
        let mut reassembler = SduReassembler::default();
        let bytes = packet_bytes(0x11, IsoPacketBoundary::Complete, 5, 3, 0, &[1, 2, 3]);
        let sdu = push_bytes(&mut reassembler, &bytes).unwrap().expect("Complete packet completes an SDU");
        assert_eq!(sdu.sequence_number, 5);
        assert_eq!(sdu.timestamp_us, None);
        assert_eq!(&sdu.data[..], &[1, 2, 3]);
    }

    #[test]
    fn empty_complete_packet_yields_an_empty_sdu() {
        let mut reassembler = SduReassembler::default();
        let bytes = packet_bytes(0x11, IsoPacketBoundary::Complete, 0, 0, 0, &[]);
        let sdu = push_bytes(&mut reassembler, &bytes).unwrap().expect("Complete packet completes an SDU");
        assert!(sdu.data.is_empty());
    }

    #[test]
    fn first_then_last_fragment_reassembles() {
        let mut reassembler = SduReassembler::default();
        let first = packet_bytes(0x11, IsoPacketBoundary::FirstFragment, 7, 6, 0, &[1, 2, 3]);
        assert!(push_bytes(&mut reassembler, &first).unwrap().is_none());

        let last = packet_bytes(0x11, IsoPacketBoundary::LastFragment, 0, 0, 0, &[4, 5, 6]);
        let sdu = push_bytes(&mut reassembler, &last).unwrap().expect("Last fragment completes an SDU");
        assert_eq!(sdu.sequence_number, 7);
        assert_eq!(&sdu.data[..], &[1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn first_then_continuation_then_last_reassembles() {
        let mut reassembler = SduReassembler::default();
        let first = packet_bytes(0x11, IsoPacketBoundary::FirstFragment, 9, 9, 0, &[1, 2, 3]);
        assert!(push_bytes(&mut reassembler, &first).unwrap().is_none());

        let middle = packet_bytes(0x11, IsoPacketBoundary::ContinuationFragment, 0, 0, 0, &[4, 5, 6]);
        assert!(push_bytes(&mut reassembler, &middle).unwrap().is_none());

        let last = packet_bytes(0x11, IsoPacketBoundary::LastFragment, 0, 0, 0, &[7, 8, 9]);
        let sdu = push_bytes(&mut reassembler, &last).unwrap().expect("Last fragment completes an SDU");
        assert_eq!(sdu.sequence_number, 9);
        assert_eq!(&sdu.data[..], &[1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn lost_packet_status_flag_drops_and_clears_state() {
        let mut reassembler = SduReassembler::default();
        let bytes = packet_bytes(0x11, IsoPacketBoundary::Complete, 42, 3, 2, &[]);
        assert_eq!(
            push_bytes(&mut reassembler, &bytes).unwrap_err(),
            SduDrop::Lost { sequence_number: 42 }
        );
    }

    #[test]
    fn invalid_packet_status_flag_is_reported() {
        let mut reassembler = SduReassembler::default();
        // Packet_Status_Flag == 1 ("possibly invalid") is neither 0 nor 2.
        let bytes = packet_bytes(0x11, IsoPacketBoundary::Complete, 3, 3, 1, &[1, 2, 3]);
        assert_eq!(
            push_bytes(&mut reassembler, &bytes).unwrap_err(),
            SduDrop::InvalidStatus { sequence_number: 3, status: 1 }
        );
    }

    #[test]
    fn declared_length_over_the_supported_max_is_rejected() {
        let mut reassembler = SduReassembler::default();
        let too_long = (MAX_ENCODED_AUDIO_SDU_LEN + 1) as u16;
        let bytes = packet_bytes(0x11, IsoPacketBoundary::FirstFragment, 1, too_long, 0, &[1, 2, 3]);
        assert_eq!(push_bytes(&mut reassembler, &bytes).unwrap_err(), SduDrop::DeclaredLengthUnsupported);
    }

    #[test]
    fn payload_longer_than_its_own_declared_length_is_rejected() {
        let mut reassembler = SduReassembler::default();
        let bytes = packet_bytes(0x11, IsoPacketBoundary::Complete, 1, 2, 0, &[1, 2, 3]);
        assert_eq!(push_bytes(&mut reassembler, &bytes).unwrap_err(), SduDrop::DeclaredLengthUnsupported);
    }

    #[test]
    fn continuation_without_a_first_fragment_is_rejected() {
        let mut reassembler = SduReassembler::default();
        let bytes = packet_bytes(0x11, IsoPacketBoundary::ContinuationFragment, 0, 0, 0, &[1, 2, 3]);
        assert_eq!(push_bytes(&mut reassembler, &bytes).unwrap_err(), SduDrop::ContinuationWithoutStart);
    }

    #[test]
    fn continuation_that_overflows_the_declared_length_is_rejected_and_clears_state() {
        let mut reassembler = SduReassembler::default();
        let first = packet_bytes(0x11, IsoPacketBoundary::FirstFragment, 1, 4, 0, &[1, 2, 3]);
        assert!(push_bytes(&mut reassembler, &first).unwrap().is_none());

        let overflow = packet_bytes(0x11, IsoPacketBoundary::LastFragment, 0, 0, 0, &[4, 5]);
        assert_eq!(push_bytes(&mut reassembler, &overflow).unwrap_err(), SduDrop::ExceededDeclaredLength);

        // State was cleared: a fresh First fragment must not see stale data from the aborted SDU.
        let restart = packet_bytes(0x11, IsoPacketBoundary::Complete, 2, 1, 0, &[9]);
        let sdu = push_bytes(&mut reassembler, &restart).unwrap().expect("reassembler recovered after the drop");
        assert_eq!(&sdu.data[..], &[9]);
    }

    #[test]
    fn last_fragment_short_of_its_declared_length_is_rejected() {
        let mut reassembler = SduReassembler::default();
        let first = packet_bytes(0x11, IsoPacketBoundary::FirstFragment, 1, 5, 0, &[1, 2, 3]);
        assert!(push_bytes(&mut reassembler, &first).unwrap().is_none());

        let last = packet_bytes(0x11, IsoPacketBoundary::LastFragment, 0, 0, 0, &[4]);
        assert_eq!(push_bytes(&mut reassembler, &last).unwrap_err(), SduDrop::IncompleteAtLastFragment);
    }

    #[test]
    fn complete_packet_whose_payload_is_short_of_its_declared_length_is_rejected() {
        // Can't actually construct this through the wire parser (Complete's data load header
        // declares iso_sdu_len, but `packet.data().len() > declared_length` is already checked
        // separately) other than the equal-length case above and the `> declared_length` case -
        // this exercises `data.len() != declared_length` when it's *shorter*, which the
        // `packet.data().len() > declared_length` guard alone doesn't catch since it only rejects
        // longer-than-declared payloads.
        let mut reassembler = SduReassembler::default();
        let bytes = packet_bytes(0x11, IsoPacketBoundary::Complete, 1, 5, 0, &[1, 2, 3]);
        assert_eq!(push_bytes(&mut reassembler, &bytes).unwrap_err(), SduDrop::IncompleteAtLastFragment);
    }

    #[test]
    fn clear_discards_an_in_progress_fragment() {
        let mut reassembler = SduReassembler::default();
        let first = packet_bytes(0x11, IsoPacketBoundary::FirstFragment, 1, 6, 0, &[1, 2, 3]);
        assert!(push_bytes(&mut reassembler, &first).unwrap().is_none());

        reassembler.clear();

        let bytes = packet_bytes(0x11, IsoPacketBoundary::LastFragment, 0, 0, 0, &[4, 5, 6]);
        assert_eq!(push_bytes(&mut reassembler, &bytes).unwrap_err(), SduDrop::ContinuationWithoutStart);
    }
}
