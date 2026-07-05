//! Basic Audio Profile 1.0.2
//!
//! BAP doesn't define a GATT service of its own - it defines the procedures for using PACS and
//! ASCS together to set up and control unicast audio streams. The GATT services themselves live
//! in [`crate::pacs`]/[`crate::ascs`]; this module holds those procedures.

use alloc::vec::Vec as AVec;
use embassy_sync::blocking_mutex::raw::RawMutex;
use trouble_host::{
    gatt::GattConnection,
    prelude::{AttributeServer, Characteristic, PacketPool},
};

use crate::{
    ascs::{Ase, AscsServer, AseControlPointNotification, AseControlPointOperation, AseState},
    MAX_SERVICES,
};

/// ASE Control Point Response_Code values (Bluetooth ASCS 5, Table 5.2). Not exhaustive - just
/// enough for the simplified state machine below to report success/failure.
const RESPONSE_SUCCESS: u8 = 0x00;
const RESPONSE_UNSPECIFIED_ERROR: u8 = 0x01;
const RESPONSE_INVALID_ASE_ID: u8 = 0x02;
const RESPONSE_INVALID_ASE_STATE_MACHINE_TRANSITION: u8 = 0x03;

/// Finds the ASE characteristic currently holding the given ASE_ID, if any, along with its
/// current value. ASE_ID isn't part of a `Characteristic`'s handle metadata, so this reads
/// through each configured ASE's stored value to match it - fine for the small `MAX_ASES`
/// this crate targets.
fn find_ase<M: RawMutex, P: PacketPool, const MAX_ASES: usize, const MAX_CONNECTIONS: usize>(
    server: &AttributeServer<'_, M, P, MAX_SERVICES, MAX_CONNECTIONS>,
    ascs: &AscsServer<MAX_ASES>,
    ase_id: u8,
) -> Option<(Characteristic<Ase>, Ase)> {
    ascs.ases().iter().find_map(|(_, characteristic)| {
        let ase = characteristic.get(server).ok()?;
        (ase.id() == ase_id).then(|| (characteristic.clone(), ase))
    })
}

/// Applies one ASE Control Point operation to `ascs`'s ASEs: transitions each targeted ASE's
/// state, notifies its new value, and builds the Control Point response.
///
/// This favors a simple, always-forward-progressing state machine over strictly modeling every
/// intermediate state (e.g. `Enabling`/`Disabling`/`Releasing` are collapsed into their next
/// resting state immediately, rather than waiting on a real CIS/ISO handshake) - good enough to
/// demonstrate wiring the control point up to real ASE state, not a full unicast audio stack.
/// In particular, no CIS/ISO channel is actually established here.
pub async fn drive_ase_control_point<
    M: RawMutex,
    P: PacketPool,
    const MAX_ASES: usize,
    const MAX_CONNECTIONS: usize,
>(
    server: &AttributeServer<'_, M, P, MAX_SERVICES, MAX_CONNECTIONS>,
    ascs: &AscsServer<MAX_ASES>,
    conn: &GattConnection<'_, '_, P>,
    operation: &AseControlPointOperation,
) -> AseControlPointNotification {
    let mut results: AVec<(u8, u8, u8)> = AVec::new();

    macro_rules! transition {
        ($ase_id:expr, $body:expr) => {{
            let ase_id: u8 = $ase_id;
            match find_ase(server, ascs, ase_id) {
                Some((characteristic, current)) => {
                    let current_state = current.state();
                    let response = (|current_state: Result<AseState, _>| -> Result<AseState, u8> { $body(current_state) })(current_state);
                    match response {
                        Ok(new_state) => {
                            let new_ase = Ase::with_state(ase_id, new_state);
                            let _ = characteristic.notify(conn, &new_ase, true).await;
                            results.push((ase_id, RESPONSE_SUCCESS, 0));
                        }
                        Err(reason) => results.push((ase_id, reason, 0)),
                    }
                }
                None => results.push((ase_id, RESPONSE_INVALID_ASE_ID, 0)),
            }
        }};
    }

    const OP_CONFIG_CODEC: u8 = 0x01;
    const OP_CONFIG_QOS: u8 = 0x02;
    const OP_ENABLE: u8 = 0x03;
    const OP_RECEIVER_START_READY: u8 = 0x04;
    const OP_DISABLE: u8 = 0x05;
    const OP_RECEIVER_STOP_READY: u8 = 0x06;
    const OP_UPDATE_METADATA: u8 = 0x07;
    const OP_RELEASE: u8 = 0x08;

    match operation.opcode() {
        OP_CONFIG_CODEC => {
            for (ase_id, _target_latency, target_phy, codec_id, config) in operation.as_config_codec().unwrap_or_default()
            {
                transition!(ase_id, |_current| Ok(AseState::CodecConfigured {
                    framing: 0,
                    preferred_phy: target_phy,
                    preferred_retransmission_number: 13,
                    max_transport_latency: 100,
                    presentation_delay_min: [0, 0, 0],
                    presentation_delay_max: [0x40, 0x9C, 0],
                    preferred_presentation_delay_min: [0, 0, 0],
                    preferred_presentation_delay_max: [0, 0, 0],
                    codec_id,
                    codec_specific_configuration: config.clone(),
                }));
            }
        }
        OP_CONFIG_QOS => {
            for (ase_id, cig_id, cis_id, sdu_interval, framing, phy, max_sdu, rtn, max_latency, delay) in
                operation.as_config_qos().unwrap_or_default()
            {
                transition!(ase_id, |_current| Ok(AseState::QosConfigured {
                    cig_id,
                    cis_id,
                    sdu_interval,
                    framing,
                    phy,
                    max_sdu,
                    retransmission_number: rtn,
                    max_transport_latency: max_latency,
                    presentation_delay: delay,
                }));
            }
        }
        OP_ENABLE => {
            for (ase_id, metadata) in operation.as_enable().unwrap_or_default() {
                transition!(ase_id, |current: Result<AseState, _>| match current {
                    Ok(AseState::QosConfigured { cig_id, cis_id, .. }) =>
                        Ok(AseState::Enabling { cig_id, cis_id, metadata: metadata.clone() }),
                    _ => Err(RESPONSE_INVALID_ASE_STATE_MACHINE_TRANSITION),
                });
            }
        }
        OP_RECEIVER_START_READY => {
            for ase_id in operation.ase_ids() {
                transition!(ase_id, |current: Result<AseState, _>| match current {
                    Ok(AseState::Enabling { cig_id, cis_id, metadata }) =>
                        Ok(AseState::Streaming { cig_id, cis_id, metadata }),
                    _ => Err(RESPONSE_INVALID_ASE_STATE_MACHINE_TRANSITION),
                });
            }
        }
        OP_UPDATE_METADATA => {
            for (ase_id, metadata) in operation.as_update_metadata().unwrap_or_default() {
                transition!(ase_id, |current: Result<AseState, _>| match current {
                    Ok(AseState::Enabling { cig_id, cis_id, .. }) =>
                        Ok(AseState::Enabling { cig_id, cis_id, metadata: metadata.clone() }),
                    Ok(AseState::Streaming { cig_id, cis_id, .. }) =>
                        Ok(AseState::Streaming { cig_id, cis_id, metadata: metadata.clone() }),
                    _ => Err(RESPONSE_INVALID_ASE_STATE_MACHINE_TRANSITION),
                });
            }
        }
        OP_DISABLE | OP_RECEIVER_STOP_READY => {
            for ase_id in operation.ase_ids() {
                // Real CIS/ISO teardown isn't modeled here, and `Enabling`/`Streaming` don't
                // retain the QoS parameters needed to go back to `QosConfigured` (they aren't
                // part of those states' Additional_ASE_Parameters), so this goes straight to
                // `Idle` instead of the spec's `Disabling` -> `QosConfigured`.
                transition!(ase_id, |current: Result<AseState, _>| match current {
                    Ok(AseState::Enabling { .. } | AseState::Streaming { .. }) => Ok(AseState::Idle),
                    _ => Err(RESPONSE_INVALID_ASE_STATE_MACHINE_TRANSITION),
                });
            }
        }
        OP_RELEASE => {
            for ase_id in operation.ase_ids() {
                transition!(ase_id, |_current| Ok(AseState::Idle));
            }
        }
        _ => {
            for ase_id in operation.ase_ids() {
                results.push((ase_id, RESPONSE_UNSPECIFIED_ERROR, 0));
            }
        }
    }

    AseControlPointNotification::new(operation.opcode(), &results)
}
