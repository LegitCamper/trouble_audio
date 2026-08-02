//! Encodes and paces outgoing LC3 audio down to the controller as HCI ISO Data packets - the
//! central/source-side mirror of `cis.rs`'s `CisManager::on_iso_data` (RX). `bt_hci::data::IsoPacket`
//! has no public outbound constructor (only ways to parse one from wire bytes), so [`build_packet`]
//! hand-encodes the wire bytes itself and round-trips them through `IsoPacket::from_hci_bytes` -
//! the same trick `cis.rs`'s own `iso_data_packet` test helper already uses to build synthetic
//! packets.
//!
//! At 48kHz/10ms, LC3 frames are ~100-120 octets (see `crate::lc3`'s own test), well under
//! typical controller ISO buffer limits - so unlike a general-purpose ISO stack, this never
//! fragments a packet across multiple "First/Continuation/Last Fragment" boundary packets, only
//! ever building single "Complete" packets. Revisit only if a real controller's
//! `ISO_Data_Packet_Length` turns out to be smaller than that.

use bt_hci::data::IsoPacket;
use bt_hci::param::ConnHandle;
use bt_hci::FromHciBytes;
use embassy_time::{Duration, Ticker};
use heapless::Vec as HVec;

/// Max encoded HCI ISO Data packet this crate builds: 4-byte packet header + 4-byte
/// (non-timestamped) `ISO_Data_Load` header + a generous bound on LC3 frame size.
pub const MAX_ISO_PACKET_LEN: usize = 8 + 160;

/// Builds one non-timestamped, "Complete boundary" HCI ISO Data packet - Bluetooth Core 6 Vol 4
/// Part E §5.4.5. `buf` is overwritten and must outlive the returned `IsoPacket`.
pub fn build_packet<'a>(
    buf: &'a mut HVec<u8, MAX_ISO_PACKET_LEN>,
    cis_handle: ConnHandle,
    sequence_num: u16,
    payload: &[u8],
) -> IsoPacket<'a> {
    const PB_COMPLETE: u16 = 0b10;
    let handle_word = (cis_handle.raw() & 0x0fff) | (PB_COMPLETE << 12);
    let data_load_len = 4 + payload.len();

    buf.clear();
    buf.extend_from_slice(&handle_word.to_le_bytes()).unwrap();
    buf.extend_from_slice(&(data_load_len as u16).to_le_bytes()).unwrap();
    buf.extend_from_slice(&sequence_num.to_le_bytes()).unwrap();
    buf.extend_from_slice(&(payload.len() as u16).to_le_bytes()).unwrap();
    buf.extend_from_slice(payload).unwrap();

    IsoPacket::from_hci_bytes(buf).expect("a packet this module just built always parses").0
}

/// Per-CIS `ISO_SDU_Sequence_Number` (Core 6 Vol 6 Part G §1/3): the host is only required to send
/// strictly increasing numbers per CIS, with no particular starting value, so this starts at 0 and
/// wraps on overflow.
#[derive(Debug, Default, Clone, Copy)]
pub struct SequenceNumber(u16);

impl SequenceNumber {
    pub fn next(&mut self) -> u16 {
        let n = self.0;
        self.0 = self.0.wrapping_add(1);
        n
    }
}

/// Builds a [`Ticker`] that fires once per `sdu_interval` - the little-endian 24-bit microsecond
/// encoding [`crate::cig::AseQos::sdu_interval`]/the ASCS wire format uses.
pub fn ticker_for_sdu_interval(sdu_interval: [u8; 3]) -> Ticker {
    let micros = u32::from_le_bytes([sdu_interval[0], sdu_interval[1], sdu_interval[2], 0]);
    Ticker::every(Duration::from_micros(micros as u64))
}

#[cfg(test)]
mod tests {
    use bt_hci::data::IsoPacketBoundary;

    use super::*;

    #[test]
    fn build_packet_round_trips_expected_header_and_payload() {
        let mut buf = HVec::new();
        let payload = [0xAA, 0xBB, 0xCC];
        let packet = build_packet(&mut buf, ConnHandle::new(0x11), 42, &payload);

        assert_eq!(packet.handle().raw(), 0x11);
        assert_eq!(packet.boundary_flag(), IsoPacketBoundary::Complete);
        let header = packet.data_load_header().expect("Complete packets always carry a data load header");
        assert_eq!(header.sequence_num, 42);
        assert_eq!(header.iso_sdu_len, payload.len() as u16);
        assert_eq!(packet.data(), &payload);
    }

    #[test]
    fn sequence_number_increments_and_wraps() {
        let mut seq = SequenceNumber::default();
        assert_eq!(seq.next(), 0);
        assert_eq!(seq.next(), 1);
        seq.0 = u16::MAX;
        assert_eq!(seq.next(), u16::MAX);
        assert_eq!(seq.next(), 0);
    }
}
