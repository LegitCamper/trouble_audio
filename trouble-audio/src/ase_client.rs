//! Central-side ASE Control Point procedures: writing Config Codec/Config QoS/Enable and
//! interpreting the server's notified response. The mirror image of [`crate::bap`]'s
//! `drive_ase_control_point`, which is peripheral-side (reacts to incoming writes) - this reads
//! from `AscsClient`/`GattClient` (the sink example built this session) and always operates on
//! both ASEs of this crate's stereo scope at once, since ASCS allows one write to cover several
//! ASE_IDs.

use alloc::vec::Vec as AVec;

use heapless::Vec as HVec;
use trouble_host::prelude::*;

use crate::ascs::{
    AscsClient, AseControlPointNotification, AseControlPointOperation, AseMetadataEntry, ConfigCodecEntry, ConfigQosEntry,
};

/// Matches this crate's stereo-only scope: one ASE per channel.
const MAX_ASES: usize = 2;

/// Response_Code for a successful ASE Control Point operation (Bluetooth ASCS 5 Table 5.2).
const RESPONSE_SUCCESS: u8 = 0x00;

/// Per-ASE outcome of one ASE Control Point operation: `Ok(())` on Response_Code Success,
/// `Err((response_code, reason))` otherwise (Bluetooth ASCS 5 Table 5.2/5.3).
pub type AseResult = Result<(), (u8, u8)>;

/// Matches an ASE Control Point notification's per-ASE results against the ASE_IDs an operation
/// targeted, in the order given. `None` if the notification is for a different opcode, or doesn't
/// cover every requested ASE - a mismatched/malformed response the caller should keep waiting
/// past (some other write's notification, most likely) rather than treat as this operation's.
pub fn aggregate_results(notification: &AseControlPointNotification, opcode: u8, ase_ids: &[u8]) -> Option<HVec<(u8, AseResult), MAX_ASES>> {
    if notification.opcode() != opcode {
        return None;
    }
    let mut out = HVec::new();
    for &ase_id in ase_ids {
        let (_, response_code, reason) = notification.results().find(|(id, _, _)| *id == ase_id)?;
        let result = if response_code == RESPONSE_SUCCESS { Ok(()) } else { Err((response_code, reason)) };
        out.push((ase_id, result)).ok()?;
    }
    Some(out)
}

/// Writes an ASE Control Point operation and waits for the server's notified response, matching
/// it back to `ase_ids` via [`aggregate_results`]. Subscribes fresh each call rather than sharing
/// one long-lived listener across the whole config_codec/config_qos/enable sequence, since a
/// dropped `NotificationListener` risks missing the very notification a later call would wait on.
async fn write_and_await<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
    client: &GattClient<'_, T, P, MAX_SERVICES>,
    ascs: &AscsClient,
    operation: &AseControlPointOperation,
    ase_ids: &[u8],
) -> Result<HVec<(u8, AseResult), MAX_ASES>, BleHostError<T::Error>> {
    let mut listener = client.subscribe(&ascs.ase_control_point, false).await?;
    client.write_characteristic(&ascs.ase_control_point, operation.as_gatt()).await?;
    loop {
        let data = listener.next().await;
        if let Ok(notification) = AseControlPointNotification::from_gatt(data.as_ref()) {
            if let Some(results) = aggregate_results(&notification, operation.opcode(), ase_ids) {
                return Ok(results);
            }
        }
    }
}

/// Drives a Config Codec operation over every entry in `entries` at once.
pub async fn config_codec<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
    client: &GattClient<'_, T, P, MAX_SERVICES>,
    ascs: &AscsClient,
    entries: AVec<ConfigCodecEntry>,
) -> Result<HVec<(u8, AseResult), MAX_ASES>, BleHostError<T::Error>> {
    let ase_ids: AVec<u8> = entries.iter().map(|e| e.0).collect();
    let operation = AseControlPointOperation::config_codec(entries);
    write_and_await(client, ascs, &operation, &ase_ids).await
}

/// Drives a Config QoS operation over every entry in `entries` at once. Unlike the peripheral
/// (which only ever records the `cig_id`/`cis_id`/QoS values a central sends it), the central
/// chooses these values itself - the caller picks them before building `entries`.
pub async fn config_qos<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
    client: &GattClient<'_, T, P, MAX_SERVICES>,
    ascs: &AscsClient,
    entries: AVec<ConfigQosEntry>,
) -> Result<HVec<(u8, AseResult), MAX_ASES>, BleHostError<T::Error>> {
    let ase_ids: AVec<u8> = entries.iter().map(|e| e.0).collect();
    let operation = AseControlPointOperation::config_qos(entries);
    write_and_await(client, ascs, &operation, &ase_ids).await
}

/// Drives an Enable operation over every entry in `entries` at once.
pub async fn enable<T: Controller, P: PacketPool, const MAX_SERVICES: usize>(
    client: &GattClient<'_, T, P, MAX_SERVICES>,
    ascs: &AscsClient,
    entries: AVec<AseMetadataEntry>,
) -> Result<HVec<(u8, AseResult), MAX_ASES>, BleHostError<T::Error>> {
    let ase_ids: AVec<u8> = entries.iter().map(|e| e.0).collect();
    let operation = AseControlPointOperation::enable(entries);
    write_and_await(client, ascs, &operation, &ase_ids).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ascs::AseControlPointNotification;

    #[test]
    fn aggregate_results_reports_success_for_every_ase() {
        let notification = AseControlPointNotification::new(0x01, &[(0, 0x00, 0x00), (1, 0x00, 0x00)]);
        let results = aggregate_results(&notification, 0x01, &[0, 1]).expect("expected aggregated results");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], (0, Ok(())));
        assert_eq!(results[1], (1, Ok(())));
    }

    #[test]
    fn aggregate_results_reports_failure_for_one_ase() {
        let notification = AseControlPointNotification::new(0x01, &[(0, 0x00, 0x00), (1, 0x03, 0x00)]);
        let results = aggregate_results(&notification, 0x01, &[0, 1]).expect("expected aggregated results");
        assert_eq!(results[0], (0, Ok(())));
        assert_eq!(results[1], (1, Err((0x03, 0x00))));
    }

    #[test]
    fn aggregate_results_rejects_mismatched_opcode() {
        let notification = AseControlPointNotification::new(0x02, &[(0, 0x00, 0x00)]);
        assert!(aggregate_results(&notification, 0x01, &[0]).is_none());
    }

    #[test]
    fn aggregate_results_rejects_notification_missing_a_requested_ase() {
        let notification = AseControlPointNotification::new(0x01, &[(0, 0x00, 0x00)]);
        assert!(aggregate_results(&notification, 0x01, &[0, 1]).is_none());
    }
}
