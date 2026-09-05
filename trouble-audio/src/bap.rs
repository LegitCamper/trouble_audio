//! Basic Audio Profile 1.0.2
//!
//! BAP doesn't define a GATT service of its own - it defines the procedures for using PACS and
//! ASCS together to set up and control unicast audio streams. The GATT services themselves live
//! in [`crate::pacs`]/[`crate::ascs`]; this module holds those procedures.

use alloc::vec::Vec as AVec;
use embassy_sync::blocking_mutex::raw::RawMutex;
use trouble_host::{
    gatt::GattConnection,
    prelude::{AsGatt, AttributeServer, Characteristic, PacketPool},
};

use crate::{
    ascs::{Ase, AseDirection, AscsServer, AseControlPointNotification, AseState, Operation},
    MAX_SERVICES,
};

/// ASE Control Point Response_Code values (Bluetooth ASCS 5, Table 5.2). Not exhaustive - just
/// enough for the state machine below to report success/failure.
const RESPONSE_SUCCESS: u8 = 0x00;
const RESPONSE_INVALID_ASE_ID: u8 = 0x03;
const RESPONSE_INVALID_ASE_STATE_MACHINE_TRANSITION: u8 = 0x04;
const RESPONSE_INVALID_ASE_DIRECTION: u8 = 0x05;

/// Finds the ASE characteristic currently holding the given ASE_ID, if any, along with its
/// current value. ASE_ID isn't part of a `Characteristic`'s handle metadata, so this reads
/// through each configured ASE's stored value to match it - fine for the small `MAX_ASES`
/// this crate targets.
fn find_ase<M: RawMutex, P: PacketPool, const MAX_ASES: usize, const MAX_CONNECTIONS: usize>(
    server: &AttributeServer<'_, M, P, MAX_SERVICES, MAX_CONNECTIONS>,
    ascs: &AscsServer<MAX_ASES>,
    ase_id: u8,
) -> Option<(usize, AseDirection, Characteristic<Ase>, Ase)> {
    ascs.ases().iter().enumerate().find_map(|(index, (direction, characteristic))| {
        let ase = characteristic.get(server).ok()?;
        (ase.id() == ase_id).then(|| (index, *direction, characteristic.clone(), ase))
    })
}

/// Applies one ASE Control Point operation to `ascs`'s ASEs: validates all targeted ASEs, sends
/// the mandatory Control Point response, then transitions and notifies each accepted ASE.
///
/// Intermediate states are retained until their corresponding CIS lifecycle event occurs. This
/// matters for interoperability: clients are allowed to wait for Releasing/Disabling before
/// tearing down or rebuilding their stream.
pub async fn drive_ase_control_point<
    M: RawMutex,
    P: PacketPool,
    const MAX_ASES: usize,
    const MAX_CONNECTIONS: usize,
>(
    server: &AttributeServer<'_, M, P, MAX_SERVICES, MAX_CONNECTIONS>,
    ascs: &AscsServer<MAX_ASES>,
    conn: &GattConnection<'_, '_, P>,
    operation: &Operation,
) -> AseControlPointNotification {
    let mut results: AVec<(u8, u8, u8)> = AVec::new();
    let mut transitions: AVec<(usize, u8, Characteristic<Ase>, AseState)> = AVec::new();

    macro_rules! transition {
        ($ase_id:expr, $body:expr) => {{
            let ase_id: u8 = $ase_id;
            match find_ase(server, ascs, ase_id) {
                Some((ase_index, direction, characteristic, current)) => {
                    let current_state = current.state();
                    let response = (|current_state: Result<AseState, _>| -> Result<AseState, u8> {
                        $body(current_state, direction, ase_index)
                    })(current_state);
                    match response {
                        Ok(new_state) => {
                            if matches!(&new_state, AseState::QosConfigured { .. }) {
                                ascs.cache_qos(ase_index, new_state.clone());
                            } else if matches!(&new_state, AseState::CodecConfigured { .. }) {
                                ascs.clear_cached_qos(ase_index);
                            }
                            transitions.push((ase_index, ase_id, characteristic, new_state));
                            results.push((ase_id, RESPONSE_SUCCESS, 0));
                        }
                        Err(reason) => results.push((ase_id, reason, 0)),
                    }
                }
                None => results.push((ase_id, RESPONSE_INVALID_ASE_ID, 0)),
            }
        }};
    }

    match operation {
        Operation::ConfigCodec(entries) => {
            for (ase_id, _target_latency, target_phy, codec_id, config) in entries.iter() {
                transition!(*ase_id, |current, _direction, _ase_index| match current {
                    Ok(AseState::Idle | AseState::CodecConfigured { .. } | AseState::QosConfigured { .. }) => {
                        Ok(AseState::CodecConfigured {
                            framing: 0,
                            preferred_phy: *target_phy,
                            preferred_retransmission_number: 13,
                            max_transport_latency: 100,
                            presentation_delay_min: [0, 0, 0],
                            presentation_delay_max: [0x40, 0x9C, 0],
                            // A degenerate [0, 0] range makes Android release after Enable.
                            preferred_presentation_delay_min: [0, 0, 0],
                            preferred_presentation_delay_max: [0x40, 0x9C, 0],
                            codec_id: *codec_id,
                            codec_specific_configuration: config.clone(),
                        })
                    }
                    _ => Err(RESPONSE_INVALID_ASE_STATE_MACHINE_TRANSITION),
                });
            }
        }
        Operation::ConfigQos(entries) => {
            for (ase_id, cig_id, cis_id, sdu_interval, framing, phy, max_sdu, rtn, max_latency, delay) in
                entries.iter()
            {
                transition!(*ase_id, |current, _direction, _ase_index| match current {
                    Ok(AseState::CodecConfigured { .. } | AseState::QosConfigured { .. }) => {
                        Ok(AseState::QosConfigured {
                            cig_id: *cig_id,
                            cis_id: *cis_id,
                            sdu_interval: *sdu_interval,
                            framing: *framing,
                            phy: *phy,
                            max_sdu: *max_sdu,
                            retransmission_number: *rtn,
                            max_transport_latency: *max_latency,
                            presentation_delay: *delay,
                        })
                    }
                    _ => Err(RESPONSE_INVALID_ASE_STATE_MACHINE_TRANSITION),
                });
            }
        }
        Operation::Enable(entries) => {
            for (ase_id, metadata) in entries.iter() {
                transition!(*ase_id, |current: Result<AseState, _>, _direction, _ase_index| match current {
                    Ok(AseState::QosConfigured { cig_id, cis_id, .. }) =>
                        Ok(AseState::Enabling { cig_id, cis_id, metadata: metadata.clone() }),
                    _ => Err(RESPONSE_INVALID_ASE_STATE_MACHINE_TRANSITION),
                });
            }
        }
        Operation::ReceiverStartReady(ids) => {
            for &ase_id in ids.iter() {
                transition!(ase_id, |current: Result<AseState, _>, direction, _ase_index| {
                    if direction != AseDirection::Source {
                        return Err(RESPONSE_INVALID_ASE_DIRECTION);
                    }
                    match current {
                        Ok(AseState::Enabling { cig_id, cis_id, metadata }) => {
                            Ok(AseState::Streaming { cig_id, cis_id, metadata })
                        }
                        _ => Err(RESPONSE_INVALID_ASE_STATE_MACHINE_TRANSITION),
                    }
                });
            }
        }
        Operation::UpdateMetadata(entries) => {
            for (ase_id, metadata) in entries.iter() {
                transition!(*ase_id, |current: Result<AseState, _>, _direction, _ase_index| match current {
                    Ok(AseState::Enabling { cig_id, cis_id, .. }) =>
                        Ok(AseState::Enabling { cig_id, cis_id, metadata: metadata.clone() }),
                    Ok(AseState::Streaming { cig_id, cis_id, .. }) =>
                        Ok(AseState::Streaming { cig_id, cis_id, metadata: metadata.clone() }),
                    _ => Err(RESPONSE_INVALID_ASE_STATE_MACHINE_TRANSITION),
                });
            }
        }
        Operation::Disable(ids) | Operation::ReceiverStopReady(ids) => {
            for &ase_id in ids.iter() {
                transition!(ase_id, |current: Result<AseState, _>, direction, ase_index| {
                    if matches!(operation, Operation::ReceiverStopReady(_))
                        && direction != AseDirection::Source
                    {
                        return Err(RESPONSE_INVALID_ASE_DIRECTION);
                    }
                    match current {
                        Ok(AseState::Enabling { cig_id, cis_id, metadata } | AseState::Streaming {
                            cig_id,
                            cis_id,
                            metadata,
                        }) if matches!(operation, Operation::Disable(_))
                            && direction == AseDirection::Source =>
                        {
                            Ok(AseState::Disabling { cig_id, cis_id, metadata })
                        }
                        Ok(AseState::Enabling { .. } | AseState::Streaming { .. })
                            if matches!(operation, Operation::Disable(_))
                                && direction == AseDirection::Sink =>
                        {
                            ascs.cached_qos(ase_index)
                                .ok_or(RESPONSE_INVALID_ASE_STATE_MACHINE_TRANSITION)
                        }
                        Ok(AseState::Disabling { .. })
                            if matches!(operation, Operation::ReceiverStopReady(_)) =>
                        {
                            ascs.cached_qos(ase_index)
                                .ok_or(RESPONSE_INVALID_ASE_STATE_MACHINE_TRANSITION)
                        }
                        _ => Err(RESPONSE_INVALID_ASE_STATE_MACHINE_TRANSITION),
                    }
                });
            }
        }
        Operation::Release(ids) => {
            for &ase_id in ids.iter() {
                transition!(ase_id, |current: Result<AseState, _>, _direction, _ase_index| match current {
                    Ok(
                        AseState::CodecConfigured { .. }
                        | AseState::QosConfigured { .. }
                        | AseState::Enabling { .. }
                        | AseState::Streaming { .. }
                        | AseState::Disabling { .. },
                    ) => Ok(AseState::Releasing),
                    _ => Err(RESPONSE_INVALID_ASE_STATE_MACHINE_TRANSITION),
                });
            }
        }
    }

    let notification = AseControlPointNotification::new(operation.opcode(), &results);
    let _ = ascs
        .ase_control_point()
        .notify_raw(conn, notification.as_gatt(), false)
        .await;
    for (_index, ase_id, characteristic, new_state) in transitions {
        info!("[bap] ase {} -> {:?}", ase_id, new_state);
        let new_ase = Ase::with_state(ase_id, new_state);
        let subscribed = characteristic.should_notify(conn);
        let notify_result = characteristic.notify(conn, &new_ase, true).await;
        info!(
            "[bap] ase {} notified, central subscribed={}, notify result={}",
            ase_id,
            subscribed,
            notify_result.is_ok()
        );
    }
    notification
}

/// Transitions a Sink ASE from `Enabling` to `Streaming` once its CIS/ISO data path is up. Unlike
/// a Source ASE (which reaches `Streaming` via the client's Receiver Start Ready operation), a
/// Sink ASE has no client-driven operation for this, so it's done autonomously here. No-op if the
/// ASE isn't currently `Enabling`.
pub async fn notify_ase_streaming<
    M: RawMutex,
    P: PacketPool,
    const MAX_ASES: usize,
    const MAX_CONNECTIONS: usize,
>(
    server: &AttributeServer<'_, M, P, MAX_SERVICES, MAX_CONNECTIONS>,
    ascs: &AscsServer<MAX_ASES>,
    conn: &GattConnection<'_, '_, P>,
    ase_id: u8,
) {
    let Some((_index, direction, characteristic, current)) = find_ase(server, ascs, ase_id) else {
        return;
    };
    if direction != AseDirection::Sink {
        return;
    }
    let Ok(AseState::Enabling { cig_id, cis_id, metadata }) = current.state() else {
        return;
    };
    let new_ase = Ase::with_state(ase_id, AseState::Streaming { cig_id, cis_id, metadata });
    let notify_result = characteristic.notify(conn, &new_ase, true).await;
    info!(
        "[bap] ase {} -> Streaming (autonomous, CIS established), notify result={}",
        ase_id,
        notify_result.is_ok()
    );
}

/// Completes the server-autonomous Released operation after a Release data-path teardown.
/// No-op when the ASE has already moved or the completion is stale from an older connection.
pub async fn notify_ase_released<
    M: RawMutex,
    P: PacketPool,
    const MAX_ASES: usize,
    const MAX_CONNECTIONS: usize,
>(
    server: &AttributeServer<'_, M, P, MAX_SERVICES, MAX_CONNECTIONS>,
    ascs: &AscsServer<MAX_ASES>,
    conn: &GattConnection<'_, '_, P>,
    ase_id: u8,
) {
    let Some((index, _direction, characteristic, current)) = find_ase(server, ascs, ase_id) else {
        return;
    };
    if !matches!(current.state(), Ok(AseState::Releasing)) {
        return;
    }
    ascs.clear_cached_qos(index);
    let new_ase = Ase::new(ase_id);
    let notify_result = characteristic.notify(conn, &new_ase, true).await;
    info!(
        "[bap] ase {} -> Idle (Released), notify result={}",
        ase_id,
        notify_result.is_ok()
    );
}

/// Returns an ASE to QoS Configured after the controller reports unexpected CIS link loss.
/// ASCS requires this transition only from Streaming or Disabling.
pub async fn notify_ase_qos_configured<
    M: RawMutex,
    P: PacketPool,
    const MAX_ASES: usize,
    const MAX_CONNECTIONS: usize,
>(
    server: &AttributeServer<'_, M, P, MAX_SERVICES, MAX_CONNECTIONS>,
    ascs: &AscsServer<MAX_ASES>,
    conn: &GattConnection<'_, '_, P>,
    ase_id: u8,
) {
    let Some((index, _direction, characteristic, current)) = find_ase(server, ascs, ase_id) else {
        return;
    };
    if !matches!(
        current.state(),
        Ok(AseState::Streaming { .. } | AseState::Disabling { .. })
    ) {
        return;
    }
    let Some(qos) = ascs.cached_qos(index) else {
        warn!("[bap] ASE {} lost its CIS without cached QoS state", ase_id);
        return;
    };
    let new_ase = Ase::with_state(ase_id, qos);
    let notify_result = characteristic.notify(conn, &new_ase, true).await;
    info!(
        "[bap] ase {} -> QoS Configured (CIS link loss), notify result={}",
        ase_id,
        notify_result.is_ok()
    );
}
