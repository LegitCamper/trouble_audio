//! Ties central-side ASE Control Point driving ([`crate::ase_client`]) to real CIG/CIS creation
//! and LC3 encoding - the initiator-side mirror of [`crate::cis::CisManager`]. Only handles
//! streaming *to* Sink ASEs (the central is always the audio source here): once both ASEs of this
//! crate's stereo scope are QoS-configured, [`CigManager`] creates the CIG, creates both CIS, sets
//! up each one's ISO data path, and hands back an [`Lc3MonoEncoder`] per ASE once ready.
//!
//! Like `CisManager`, [`CigManager::on_cis_established`] implements [`EventHandler`] so it can be
//! handed to [`RxRunner::run_with_handler`](trouble_host::prelude::RxRunner::run_with_handler);
//! anything requiring an awaited HCI command is only decided there and carried out by
//! [`drive_cig`], which must be polled concurrently.

use core::cell::RefCell;

use bt_hci::controller::{ControllerCmdAsync, ControllerCmdSync};
use bt_hci::event::le::LeCisEstablished;
use bt_hci::param::{ConnHandle, Status};
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::channel::Channel;
use trouble_host::prelude::*;

#[cfg(feature = "defmt")]
use defmt::{info, warn};

use crate::{
    generic_audio::{FrameDuration, SamplingFrequency},
    iso::{
        data_path_direction, LeCreateCis, LeSetCigParameters, LeSetupIsoDataPath, DATA_PATH_ID_HCI, LeRemoveIsoDataPath,
    },
    lc3::Lc3MonoEncoder,
    CodingFormat,
};

/// Number of CIS this crate's CIG creation supports - matches [`crate::iso::LeSetCigParameters`]
/// (itself hardcoded to 2, since `bt_hci::cmd!`'s params are a fixed struct) and this crate's
/// stereo-only scope.
const CIS_COUNT: usize = 2;

/// `Worst_Case_SCA`/`Packing` for [`LeSetCigParameters`]: not a per-ASE concern, so fixed here
/// rather than threaded through the caller. `0` ("251 ppm to 500 ppm") is the safe default absent
/// real clock-accuracy data; `0` (Sequential) is the simpler of the two packing schemes.
const WORST_CASE_SCA: u8 = 0;
const PACKING_SEQUENTIAL: u8 = 0;

/// The QoS this crate's central chooses for one ASE's CIS - shared with its Config QoS ASE
/// Control Point entry (see [`crate::ascs::ConfigQosEntry`]) so the two can't drift apart. Central
/// to peripheral only: these are Sink ASEs, so no data flows peripheral to central (that
/// direction's `Max_SDU`/`RTN` are always sent as 0 to the controller).
#[derive(Clone, Copy)]
pub struct AseQos {
    pub cig_id: u8,
    pub cis_id: u8,
    /// Microseconds, 24-bit.
    pub sdu_interval: [u8; 3],
    pub framing: u8,
    pub phy: PhySet,
    pub max_sdu: u16,
    pub retransmission_number: u8,
    pub max_transport_latency: u16,
    /// Microseconds, 24-bit.
    pub presentation_delay: [u8; 3],
}

// `PhySet` (from trouble-host) doesn't implement `Debug`, so this can't be derived.
impl core::fmt::Debug for AseQos {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AseQos")
            .field("cig_id", &self.cig_id)
            .field("cis_id", &self.cis_id)
            .field("sdu_interval", &self.sdu_interval)
            .field("framing", &self.framing)
            .field("phy", &(self.phy as u8))
            .field("max_sdu", &self.max_sdu)
            .field("retransmission_number", &self.retransmission_number)
            .field("max_transport_latency", &self.max_transport_latency)
            .field("presentation_delay", &self.presentation_delay)
            .finish()
    }
}

#[derive(Clone, Copy, Default)]
struct CigAseSlot {
    ase_id: Option<u8>,
    qos: Option<AseQos>,
    sampling_frequency: Option<SamplingFrequency>,
    frame_duration: Option<FrameDuration>,
    cis_handle: Option<u16>,
}

/// A pending action decided synchronously (by [`CigManager::configure`]/
/// [`CigManager::cig_parameters_set`]/the [`EventHandler`] impl), to be carried out by
/// [`drive_cig`] since it requires awaiting an HCI command.
enum CigAction {
    SetCigParameters(LeSetCigParameters),
    CreateCis {
        cis_0: ConnHandle,
        cis_1: ConnHandle,
        acl: ConnHandle,
    },
    SetupDataPath(ConnHandle, u8),
}

/// Central-side counterpart to [`crate::cis::CisManager`]: creates the CIG/CIS for this crate's
/// stereo scope (exactly [`CIS_COUNT`] ASEs, always Sink ASEs on the peer) and encodes outgoing
/// audio. See the module docs for how to wire this up.
pub struct CigManager<M: RawMutex> {
    slots: RefCell<[CigAseSlot; CIS_COUNT]>,
    encoders: RefCell<[Option<Lc3MonoEncoder>; CIS_COUNT]>,
    acl_handle: RefCell<Option<ConnHandle>>,
    actions: Channel<M, CigAction, 4>,
    ready: Channel<M, u8, 4>,
}

impl<M: RawMutex> Default for CigManager<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M: RawMutex> CigManager<M> {
    /// Creates an empty manager, before any ASE has been configured.
    pub fn new() -> Self {
        Self {
            slots: RefCell::new([CigAseSlot::default(); CIS_COUNT]),
            encoders: RefCell::new(core::array::from_fn(|_| None)),
            acl_handle: RefCell::new(None),
            actions: Channel::new(),
            ready: Channel::new(),
        }
    }

    /// The ACL connection this CIG/CIS belongs to - set once, right after connecting, before
    /// [`Self::configure`] is called for any ASE.
    pub fn set_acl_handle(&self, handle: ConnHandle) {
        *self.acl_handle.borrow_mut() = Some(handle);
    }

    /// Records the QoS/codec this crate's central chose for one ASE (right after its Config QoS
    /// ASE Control Point write succeeds). Once every ASE of this crate's stereo scope has been
    /// configured, queues `LE Set CIG Parameters` for [`drive_cig`] to carry out.
    pub fn configure(&self, ase_id: u8, qos: AseQos, sampling_frequency: SamplingFrequency, frame_duration: FrameDuration) {
        let mut slots = self.slots.borrow_mut();
        let Some(idx) = slots.iter().position(|s| s.ase_id.is_none()) else {
            #[cfg(feature = "log")]
            log::warn!("[cig] configure() called for more ASEs than this crate's stereo scope supports");
            #[cfg(feature = "defmt")]
            warn!("[cig] configure() called for more ASEs than this crate's stereo scope supports");
            return;
        };
        slots[idx] = CigAseSlot {
            ase_id: Some(ase_id),
            qos: Some(qos),
            sampling_frequency: Some(sampling_frequency),
            frame_duration: Some(frame_duration),
            cis_handle: None,
        };

        if slots.iter().any(|s| s.ase_id.is_none()) {
            return;
        }
        // Every ASE is configured - build LE Set CIG Parameters from both slots' QoS.
        let (Some(q0), Some(q1)) = (slots[0].qos, slots[1].qos) else { return };
        debug_assert_eq!(q0.cig_id, q1.cig_id, "both ASEs of one CIG must share a CIG_ID");
        let params = LeSetCigParameters::new(
            q0.cig_id,
            q0.sdu_interval,
            q0.sdu_interval, // No peripheral-to-central traffic (Sink ASEs): same interval, 0 SDU.
            WORST_CASE_SCA,
            PACKING_SEQUENTIAL,
            q0.framing,
            q0.max_transport_latency,
            q0.max_transport_latency,
            CIS_COUNT as u8,
            q0.cis_id,
            q0.max_sdu,
            0, // max_sdu_p_to_c
            q0.phy as u8,
            q0.phy as u8,
            q0.retransmission_number,
            0, // rtn_p_to_c
            q1.cis_id,
            q1.max_sdu,
            0,
            q1.phy as u8,
            q1.phy as u8,
            q1.retransmission_number,
            0,
        );
        let _ = self.actions.try_send(CigAction::SetCigParameters(params));
    }

    /// Called by [`drive_cig`] once `LE Set CIG Parameters` returns, to record the
    /// controller-assigned CIS connection handles and queue `LE Create CIS`.
    fn cig_parameters_set(&self, connection_handle_0: ConnHandle, connection_handle_1: ConnHandle) {
        self.slots.borrow_mut()[0].cis_handle = Some(connection_handle_0.raw());
        self.slots.borrow_mut()[1].cis_handle = Some(connection_handle_1.raw());
        let Some(acl) = *self.acl_handle.borrow() else {
            #[cfg(feature = "log")]
            log::warn!("[cig] LE Set CIG Parameters completed before an ACL handle was set");
            #[cfg(feature = "defmt")]
            warn!("[cig] LE Set CIG Parameters completed before an ACL handle was set");
            return;
        };
        let _ = self.actions.try_send(CigAction::CreateCis {
            cis_0: connection_handle_0,
            cis_1: connection_handle_1,
            acl,
        });
    }

    /// Waits for the next ASE whose CIS/ISO data path is up and ready to stream encoded audio.
    pub async fn next_ready_ase(&self) -> u8 {
        self.ready.receive().await
    }

    /// The CIS connection handle for `ase_id`, once `LE Set CIG Parameters` has completed for it -
    /// needed to address outgoing ISO Data packets (see [`crate::iso_tx::build_packet`]).
    pub fn cis_handle(&self, ase_id: u8) -> Option<ConnHandle> {
        let slots = self.slots.borrow();
        let slot = slots.iter().find(|s| s.ase_id == Some(ase_id))?;
        Some(ConnHandle::new(slot.cis_handle?))
    }

    /// Encodes one PCM frame for `ase_id`'s CIS, if it's ready (see [`Self::next_ready_ase`]).
    /// `out`'s length picks the codec frame size, per [`Lc3MonoEncoder::encode`].
    pub fn encode(&self, ase_id: u8, pcm: &[i16], out: &mut [u8]) -> Option<Result<(), crate::lc3::Lc3EncoderError>> {
        let idx = self.slots.borrow().iter().position(|s| s.ase_id == Some(ase_id))?;
        let mut encoders = self.encoders.borrow_mut();
        let encoder = encoders[idx].as_mut()?;
        Some(encoder.encode(pcm, out))
    }
}

impl<M: RawMutex> EventHandler for CigManager<M> {
    fn on_cis_established(&self, event: &LeCisEstablished) {
        if event.status != Status::SUCCESS {
            #[cfg(feature = "log")]
            log::warn!("[cig] CIS establishment failed");
            #[cfg(feature = "defmt")]
            warn!("[cig] CIS establishment failed");
            return;
        }

        let handle = event.handle.raw();
        let slot = {
            let slots = self.slots.borrow();
            let Some(idx) = slots.iter().position(|s| s.cis_handle == Some(handle)) else {
                #[cfg(feature = "log")]
                log::warn!("[cig] CIS established for an untracked handle {}", handle);
                #[cfg(feature = "defmt")]
                warn!("[cig] CIS established for an untracked handle {}", handle);
                return;
            };
            (idx, slots[idx])
        };
        let (idx, slot) = slot;
        let (Some(sampling_frequency), Some(frame_duration)) = (slot.sampling_frequency, slot.frame_duration) else {
            return;
        };
        match Lc3MonoEncoder::new(sampling_frequency, frame_duration) {
            Ok(encoder) => self.encoders.borrow_mut()[idx] = Some(encoder),
            Err(_) => {
                #[cfg(feature = "log")]
                log::warn!("[cig] unsupported LC3 sampling frequency for established CIS");
                #[cfg(feature = "defmt")]
                warn!("[cig] unsupported LC3 sampling frequency for established CIS");
                return;
            }
        }

        #[cfg(feature = "log")]
        log::info!("[cig] CIS established, setting up ISO data path");
        #[cfg(feature = "defmt")]
        info!("[cig] CIS established, setting up ISO data path");
        let _ = self.actions.try_send(CigAction::SetupDataPath(event.handle, slot.ase_id.unwrap_or(0)));
    }
}

/// Drives the HCI side of CIG/CIS/ISO setup, as decided synchronously by `manager`'s
/// [`CigManager::configure`]/[`EventHandler`] callbacks. Must be polled concurrently with
/// [`RxRunner::run_with_handler`](trouble_host::prelude::RxRunner::run_with_handler) (e.g. via
/// `select`) for those decisions to actually reach the controller.
pub async fn drive_cig<C, M: RawMutex>(stack: &Stack<'_, C, impl PacketPool>, manager: &CigManager<M>) -> !
where
    C: Controller
        + ControllerCmdSync<LeSetCigParameters>
        + ControllerCmdAsync<LeCreateCis>
        + for<'a> ControllerCmdSync<LeSetupIsoDataPath<'a>>
        + ControllerCmdSync<LeRemoveIsoDataPath>,
{
    let iso = stack.iso();
    loop {
        match manager.actions.receive().await {
            CigAction::SetCigParameters(params) => match iso.command(params).await {
                Ok(ret) => manager.cig_parameters_set(ret.connection_handle_0, ret.connection_handle_1),
                Err(_e) => {
                    #[cfg(feature = "log")]
                    log::warn!("[cig] LE Set CIG Parameters failed");
                    #[cfg(feature = "defmt")]
                    warn!("[cig] LE Set CIG Parameters failed");
                }
            },
            CigAction::CreateCis { cis_0, cis_1, acl } => {
                if let Err(_e) = iso.command_async(LeCreateCis::new(CIS_COUNT as u8, cis_0, acl, cis_1, acl)).await {
                    #[cfg(feature = "log")]
                    log::warn!("[cig] LE Create CIS failed");
                    #[cfg(feature = "defmt")]
                    warn!("[cig] LE Create CIS failed");
                }
            }
            CigAction::SetupDataPath(handle, ase_id) => {
                let result = iso
                    .command(LeSetupIsoDataPath::new(
                        handle,
                        data_path_direction::INPUT,
                        DATA_PATH_ID_HCI,
                        u8::from(CodingFormat::Transparent),
                        0,
                        0,
                        [0, 0, 0],
                        &[],
                    ))
                    .await;
                match result {
                    Ok(_) => {
                        #[cfg(feature = "log")]
                        log::info!("[cig] ISO data path set up for handle {}", handle.raw());
                        #[cfg(feature = "defmt")]
                        info!("[cig] ISO data path set up for handle {}", handle.raw());
                        let _ = manager.ready.try_send(ase_id);
                    }
                    Err(_e) => {
                        #[cfg(feature = "log")]
                        log::warn!("[cig] LE Setup ISO Data Path failed");
                        #[cfg(feature = "defmt")]
                        warn!("[cig] LE Setup ISO Data Path failed");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bt_hci::param::PhyKind;
    use embassy_sync::blocking_mutex::raw::NoopRawMutex;

    use super::*;
    use crate::generic_audio::{FrameDuration, SamplingFrequency};

    fn qos(cig_id: u8, cis_id: u8) -> AseQos {
        AseQos {
            cig_id,
            cis_id,
            sdu_interval: [0x10, 0x27, 0x00],
            framing: 0,
            phy: PhySet::M1,
            max_sdu: 100,
            retransmission_number: 2,
            max_transport_latency: 20,
            presentation_delay: [0, 0, 0],
        }
    }

    fn established_event(handle: u16) -> LeCisEstablished {
        LeCisEstablished {
            status: Status::SUCCESS,
            handle: ConnHandle::new(handle),
            cig_sync_delay: Default::default(),
            cis_sync_delay: Default::default(),
            transport_latency_c_to_p: Default::default(),
            transport_latency_p_to_c: Default::default(),
            phy_c_to_p: PhyKind::Le1M,
            phy_p_to_c: PhyKind::Le1M,
            nse: 1,
            bn_c_to_p: 1,
            bn_p_to_c: 0,
            ft_c_to_p: 1,
            ft_p_to_c: 0,
            max_pdu_c_to_p: 100,
            max_pdu_p_to_c: 0,
            iso_interval: Default::default(),
        }
    }

    #[test]
    fn configuring_both_ases_then_establishing_both_cis_drives_cig_setup_in_order() {
        let manager = CigManager::<NoopRawMutex>::new();
        manager.set_acl_handle(ConnHandle::new(0x05));

        let sampling_frequency = SamplingFrequency::Hz48000;
        let frame_duration = FrameDuration::Duration10MS;

        // Configuring only one ASE isn't enough to start CIG creation.
        manager.configure(0, qos(7, 0), sampling_frequency, frame_duration);
        assert!(manager.actions.try_receive().is_err());

        // The second ASE completes the stereo pair - CIG creation starts.
        manager.configure(1, qos(7, 1), sampling_frequency, frame_duration);
        match manager.actions.try_receive() {
            Ok(CigAction::SetCigParameters(_)) => {}
            other => panic!("expected SetCigParameters, got {:?}", other.is_ok()),
        }

        // Simulate `drive_cig` completing LE Set CIG Parameters.
        manager.cig_parameters_set(ConnHandle::new(0x10), ConnHandle::new(0x11));
        match manager.actions.try_receive() {
            Ok(CigAction::CreateCis { cis_0, cis_1, acl }) => {
                assert_eq!(cis_0.raw(), 0x10);
                assert_eq!(cis_1.raw(), 0x11);
                assert_eq!(acl.raw(), 0x05);
            }
            other => panic!("expected CreateCis, got {:?}", other.is_ok()),
        }

        // Both CIS establish - each queues its own SetupDataPath.
        manager.on_cis_established(&established_event(0x10));
        match manager.actions.try_receive() {
            Ok(CigAction::SetupDataPath(handle, ase_id)) => {
                assert_eq!(handle.raw(), 0x10);
                assert_eq!(ase_id, 0);
            }
            other => panic!("expected SetupDataPath, got {:?}", other.is_ok()),
        }

        manager.on_cis_established(&established_event(0x11));
        match manager.actions.try_receive() {
            Ok(CigAction::SetupDataPath(handle, ase_id)) => {
                assert_eq!(handle.raw(), 0x11);
                assert_eq!(ase_id, 1);
            }
            other => panic!("expected SetupDataPath, got {:?}", other.is_ok()),
        }
    }
}
