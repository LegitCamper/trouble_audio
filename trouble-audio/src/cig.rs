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
//!
//! By default this issues the plain, spec-standard `LE Set CIG Parameters`. Enable this crate's
//! `nrf-sdc` feature if (and only if) the controller is Nordic's SoftDevice Controller: at least
//! on the nRF54L, it rejects that command with "Unsupported Feature or Parameter Value" for
//! otherwise spec-valid parameters, regardless of the values chosen, and Nordic's own documented
//! workaround is to use `LE Set CIG Parameters Test` instead. That workaround isn't safe to apply
//! unconditionally to every controller, so it's opt-in rather than this crate's default.

use core::cell::RefCell;

use bt_hci::cmd::le::{LeCreateCis, LeRemoveCig, LeRemoveIsoDataPath, LeSetupIsoDataPath};
#[cfg(feature = "nrf-sdc")]
use bt_hci::cmd::le::LeSetCigParametersTest;
#[cfg(not(feature = "nrf-sdc"))]
use bt_hci::cmd::le::LeSetCigParameters;
use bt_hci::controller::{ControllerCmdAsync, ControllerCmdSync};
use bt_hci::event::le::LeCisEstablished;
#[cfg(feature = "nrf-sdc")]
use bt_hci::param::CisConfigTest;
#[cfg(not(feature = "nrf-sdc"))]
use bt_hci::param::CisConfig;
use bt_hci::param::{
    CigId, CisConnConfig, CisId, CodecId, ConnHandle, DataPathDirection, DataPathId, ExtDuration, Framing, Packing, PhyMask, Status,
};
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::channel::Channel;
use trouble_host::prelude::*;

/// The command [`drive_cig`] issues to set up a CIG's parameters - `LE Set CIG Parameters Test`
/// under the `nrf-sdc` feature, or the plain, spec-standard `LE Set CIG Parameters` otherwise (see
/// the module docs for why). Public so callers can name it in their own `Controller` trait bounds
/// (e.g. a generic `run<C>` that itself calls [`drive_cig`]) without hardcoding one or the other.
#[cfg(feature = "nrf-sdc")]
pub type SetCigParametersCmd<'a> = LeSetCigParametersTest<'a>;
#[cfg(not(feature = "nrf-sdc"))]
pub type SetCigParametersCmd<'a> = LeSetCigParameters<'a>;

#[cfg(feature = "defmt")]
use defmt::{info, warn, Debug2Format};

use crate::{
    generic_audio::{FrameDuration, SamplingFrequency},
    lc3::Lc3MonoEncoder,
    CodingFormat,
};

/// Number of CIS this crate's CIG creation supports - purely this crate's own stereo-only scope
/// (the underlying `LE Set CIG Parameters [Test]`/`LE Create CIS` commands take a `cis_configs`
/// slice, so nothing upstream caps this).
const CIS_COUNT: usize = 2;

/// `Worst_Case_SCA`/`Packing` for `LE Set CIG Parameters [Test]`: not a per-ASE concern, so fixed
/// here rather than threaded through the caller. `0` ("251 ppm to 500 ppm") is the safe default
/// absent real clock-accuracy data; Sequential is the simpler of the two packing schemes.
const WORST_CASE_SCA: u8 = 0;

/// `Flush_Timeout` (in `ISO_Interval` units) for both directions of `LE Set CIG Parameters
/// [Test]`: fixed at the minimum (1) so every subevent - including retransmissions - completes
/// within a single ISO interval, keeping audio latency to one SDU interval rather than letting
/// the controller spread retries across several.
const FLUSH_TIMEOUT: u8 = 1;

/// Converts this crate's [`PhySet`] (already a 1M/2M/Coded bitmask, bit-for-bit) to
/// [`PhyMask`], the typed form `CisConfig`/`CisConfigTest` require.
fn phy_mask(phy: PhySet) -> PhyMask {
    PhyMask::new()
        .set_le_1m_phy(matches!(phy, PhySet::M1 | PhySet::M1M2 | PhySet::M1Coded | PhySet::M1M2Coded))
        .set_le_2m_phy(matches!(phy, PhySet::M2 | PhySet::M1M2 | PhySet::M2Coded | PhySet::M1M2Coded))
        .set_le_coded_phy(matches!(phy, PhySet::Coded | PhySet::M1Coded | PhySet::M2Coded | PhySet::M1M2Coded))
}

/// `0` = Unframed, `1` = Framed (see [`AseQos::framing`]).
fn framing(value: u8) -> Framing {
    if value == 0 {
        Framing::Unframed
    } else {
        Framing::Framed
    }
}

/// Builds this ASE's entry for the standard `LE Set CIG Parameters` command - the default,
/// spec-plain path (see the `nrf-sdc` feature docs on [`crate`] for when
/// [`cis_config_test`] is used instead).
#[cfg(not(feature = "nrf-sdc"))]
fn cis_config(q: &AseQos) -> CisConfig {
    CisConfig {
        cis_id: CisId::new(q.cis_id),
        max_sdu_c_to_p: q.max_sdu,
        max_sdu_p_to_c: 0, // No peripheral-to-central traffic (Sink ASEs).
        phy_c_to_p: phy_mask(q.phy),
        phy_p_to_c: phy_mask(q.phy),
        rtn_c_to_p: q.retransmission_number,
        rtn_p_to_c: 0,
    }
}

/// Builds this ASE's entry for `LE Set CIG Parameters Test` - the `nrf-sdc` workaround path.
///
/// `CisConfigTest` (as published by upstream `bt-hci` today) has no `NSE` field, even though the
/// Core spec's Test command takes one per CIS - so unlike [`cis_config`]'s `rtn_c_to_p`, this
/// can't actually request retransmissions; `bn_c_to_p` is fixed at 1 SDU/interval like before.
#[cfg(feature = "nrf-sdc")]
fn cis_config_test(q: &AseQos) -> CisConfigTest {
    CisConfigTest {
        cis_id: CisId::new(q.cis_id),
        max_sdu_c_to_p: q.max_sdu,
        max_sdu_p_to_c: 0,
        max_pdu_c_to_p: q.max_sdu, // Unfragmented (BN=1), so Max_PDU == Max_SDU.
        max_pdu_p_to_c: 0,
        phy_c_to_p: phy_mask(q.phy),
        phy_p_to_c: phy_mask(q.phy),
        bn_c_to_p: 1,
        bn_p_to_c: 0,
    }
}

/// Converts a 24-bit little-endian microsecond value (as used for `SDU_Interval`/
/// `Presentation_Delay` throughout this crate) to a plain `u32`.
fn u24_le_to_u32(bytes: [u8; 3]) -> u32 {
    u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16)
}

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
    /// Remove the CIG with this CIG_ID from the controller before recreating it.  Queued by
    /// [`CigManager::reset`] so reconnect attempts don't hit `Command Disallowed (0x0C)` when
    /// they re-issue `LE Set CIG Parameters` with the same CIG_ID.
    RemoveCig(u8),
    /// Both ASEs' QoS, from which [`drive_cig`] builds the actual command - deferred rather than
    /// built here since the command borrows a per-CIS slice that can't outlive this action.
    SetCigParameters(AseQos, AseQos),
    CreateCis {
        cig_id: u8,
        cis_0: ConnHandle,
        cis_1: ConnHandle,
        acl: ConnHandle,
    },
    SetupDataPath(ConnHandle, u8),
}

/// An [`Lc3MonoEncoder`] tagged with the Sampling_Frequency/Frame_Duration it was built for, so a
/// reconnect with the same params can reuse it instead of leaking a new one (see `lc3.rs`) - see
/// [`CigManager::on_cis_established`].
struct CachedEncoder {
    sampling_frequency: SamplingFrequency,
    frame_duration: FrameDuration,
    encoder: Lc3MonoEncoder,
}

/// Central-side counterpart to [`crate::cis::CisManager`]: creates the CIG/CIS for this crate's
/// stereo scope (exactly [`CIS_COUNT`] ASEs, always Sink ASEs on the peer) and encodes outgoing
/// audio. See the module docs for how to wire this up.
pub struct CigManager<M: RawMutex> {
    slots: RefCell<[CigAseSlot; CIS_COUNT]>,
    encoders: RefCell<[Option<CachedEncoder>; CIS_COUNT]>,
    acl_handle: RefCell<Option<ConnHandle>>,
    /// The CIG_ID most recently passed to [`Self::configure`]; needed by [`Self::reset`] so it
    /// can queue a `RemoveCig` action without the caller having to pass the ID in again.
    cig_id: RefCell<Option<u8>>,
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
            cig_id: RefCell::new(None),
            actions: Channel::new(),
            ready: Channel::new(),
        }
    }

    /// Resets the manager to its initial empty state, ready to service a new connection.
    ///
    /// Call this at the start of each new connection attempt (before [`Self::set_acl_handle`] and
    /// [`Self::configure`]) to clear stale slots/handles. If a CIG was created previously, this
    /// queues a `LE Remove CIG` for [`drive_cig`] - without it, the controller rejects a re-create
    /// with `Command Disallowed (0x0C)`.
    ///
    /// Deliberately does *not* clear `encoders`, so [`Self::on_cis_established`] can reuse one
    /// across a reconnect - relies on the caller always calling [`Self::configure`] for the same
    /// ASEs in the same order, so slot indices stay consistent.
    pub fn reset(&self) {
        *self.slots.borrow_mut() = [CigAseSlot::default(); CIS_COUNT];
        *self.acl_handle.borrow_mut() = None;
        while self.actions.try_receive().is_ok() {}
        while self.ready.try_receive().is_ok() {}
        if let Some(cig_id) = *self.cig_id.borrow() {
            let _ = self.actions.try_send(CigAction::RemoveCig(cig_id));
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
        // Every ASE is configured - queue LE Set CIG Parameters[/Test] for `drive_cig` to build
        // and send (see the module docs for why the command variant is feature-gated).
        let (Some(q0), Some(q1)) = (slots[0].qos, slots[1].qos) else { return };
        *self.cig_id.borrow_mut() = Some(q0.cig_id);
        debug_assert_eq!(q0.cig_id, q1.cig_id, "both ASEs of one CIG must share a CIG_ID");
        debug_assert_eq!(q0.sdu_interval, q1.sdu_interval, "both ASEs of one CIG must share an SDU interval");
        // ISO_Interval is in 1.25ms units; unframed PDUs require it to be an integer multiple of
        // SDU_Interval, and this crate always requests a 1:1 mapping.
        debug_assert_eq!(u24_le_to_u32(q0.sdu_interval) % 1250, 0, "SDU_Interval must be a multiple of 1.25ms");
        let _ = self.actions.try_send(CigAction::SetCigParameters(q0, q1));
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
        let Some(cig_id) = *self.cig_id.borrow() else {
            #[cfg(feature = "log")]
            log::warn!("[cig] LE Set CIG Parameters completed before a CIG_ID was recorded");
            #[cfg(feature = "defmt")]
            warn!("[cig] LE Set CIG Parameters completed before a CIG_ID was recorded");
            return;
        };
        let _ = self.actions.try_send(CigAction::CreateCis {
            cig_id,
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
        let cached = encoders[idx].as_mut()?;
        Some(cached.encoder.encode(pcm, out))
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

        // Reuse the existing encoder when it matches (see `CachedEncoder`/`reset()` docs).
        let already_matches = matches!(
            &self.encoders.borrow()[idx],
            Some(cached) if cached.sampling_frequency == sampling_frequency && cached.frame_duration == frame_duration
        );
        if already_matches {
            #[cfg(feature = "log")]
            log::info!("[cig] reusing existing encoder for ase slot {} (reconnect, same audio params)", idx);
            #[cfg(feature = "defmt")]
            info!("[cig] reusing existing encoder for ase slot {} (reconnect, same audio params)", idx);
        } else {
            match Lc3MonoEncoder::new(sampling_frequency, frame_duration) {
                Ok(encoder) => {
                    self.encoders.borrow_mut()[idx] = Some(CachedEncoder {
                        sampling_frequency,
                        frame_duration,
                        encoder,
                    });
                }
                Err(_) => {
                    #[cfg(feature = "log")]
                    log::warn!("[cig] unsupported LC3 sampling frequency for established CIS");
                    #[cfg(feature = "defmt")]
                    warn!("[cig] unsupported LC3 sampling frequency for established CIS");
                    return;
                }
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
        + for<'a> ControllerCmdSync<SetCigParametersCmd<'a>>
        + for<'a> ControllerCmdAsync<LeCreateCis<'a>>
        + for<'a> ControllerCmdSync<LeSetupIsoDataPath<'a>>
        + ControllerCmdSync<LeRemoveIsoDataPath>
        + ControllerCmdSync<LeRemoveCig>,
{
    let iso = stack.iso();
    loop {
        match manager.actions.receive().await {
            CigAction::RemoveCig(cig_id) => {
                // Errors are expected on the very first connection (no CIG yet) or if the
                // controller already removed the CIG when the ACL dropped. Ignore them.
                let _ = iso.command(LeRemoveCig::new(CigId::new(cig_id))).await;
                #[cfg(feature = "log")]
                log::info!("[cig] removed CIG {cig_id} (or it was already gone)");
                #[cfg(feature = "defmt")]
                info!("[cig] removed CIG {} (or it was already gone)", cig_id);
            }
            CigAction::SetCigParameters(q0, q1) => {
                let sdu_interval_us = u24_le_to_u32(q0.sdu_interval);
                let sdu_interval = ExtDuration::from_u32(sdu_interval_us);
                // ISO_Interval is in 1.25ms units; unframed PDUs require it to be an integer
                // multiple of SDU_Interval, and this crate always requests a 1:1 mapping (see the
                // matching `debug_assert_eq!` in `CigManager::configure`).
                let iso_interval = (sdu_interval_us / 1250) as u16;
                #[cfg(feature = "nrf-sdc")]
                let cis_configs = [cis_config_test(&q0), cis_config_test(&q1)];
                #[cfg(not(feature = "nrf-sdc"))]
                let cis_configs = [cis_config(&q0), cis_config(&q1)];
                let cmd = SetCigParametersCmd::new(
                    CigId::new(q0.cig_id),
                    sdu_interval,
                    sdu_interval, // No peripheral-to-central traffic (Sink ASEs): same interval, 0 SDU.
                    FLUSH_TIMEOUT,
                    FLUSH_TIMEOUT,
                    iso_interval,
                    WORST_CASE_SCA,
                    Packing::Sequential,
                    framing(q0.framing),
                    &cis_configs,
                );
                match iso.command(cmd).await {
                    Ok(ret) => manager.cig_parameters_set(ret.cis_handles[0], ret.cis_handles[1]),
                    Err(_e) => {
                        #[cfg(feature = "log")]
                        log::warn!("[cig] LE Set CIG Parameters failed: {_e:?}");
                        #[cfg(feature = "defmt")]
                        warn!("[cig] LE Set CIG Parameters failed: {}", Debug2Format(&_e));
                    }
                }
            }
            CigAction::CreateCis { cig_id, cis_0, cis_1, acl } => {
                let cis_configs = [
                    CisConnConfig { cis_handle: cis_0, acl_handle: acl },
                    CisConnConfig { cis_handle: cis_1, acl_handle: acl },
                ];
                if let Err(_e) = iso.command_async(LeCreateCis::new(CigId::new(cig_id), &cis_configs)).await {
                    #[cfg(feature = "log")]
                    log::warn!("[cig] LE Create CIS failed: {_e:?}");
                    #[cfg(feature = "defmt")]
                    warn!("[cig] LE Create CIS failed: {}", Debug2Format(&_e));
                }
            }
            CigAction::SetupDataPath(handle, ase_id) => {
                let result = iso
                    .command(LeSetupIsoDataPath::new(
                        handle,
                        DataPathDirection::Input,
                        DataPathId::HCI,
                        CodecId {
                            coding_format: u8::from(CodingFormat::Transparent),
                            company_id: 0,
                            vendor_specific_codec_id: 0,
                        },
                        ExtDuration::from_u32(0),
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
                        log::warn!("[cig] LE Setup ISO Data Path failed: {_e:?}");
                        #[cfg(feature = "defmt")]
                        warn!("[cig] LE Setup ISO Data Path failed: {}", Debug2Format(&_e));
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
            Ok(CigAction::SetCigParameters(_, _)) => {}
            other => panic!("expected SetCigParameters, got {:?}", other.is_ok()),
        }

        // Simulate `drive_cig` completing LE Set CIG Parameters.
        manager.cig_parameters_set(ConnHandle::new(0x10), ConnHandle::new(0x11));
        match manager.actions.try_receive() {
            Ok(CigAction::CreateCis { cig_id, cis_0, cis_1, acl }) => {
                assert_eq!(cig_id, 7);
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

    /// Regression test mirroring `cis::CisManager`'s leak fix: `reset()` used to unconditionally
    /// clear `encoders` on every reconnect, leaking an `Lc3MonoEncoder` per ASE each time.
    #[test]
    fn reconnecting_with_the_same_audio_params_reuses_the_existing_encoders() {
        let manager = CigManager::<NoopRawMutex>::new();
        manager.set_acl_handle(ConnHandle::new(0x05));

        let sampling_frequency = SamplingFrequency::Hz48000;
        let frame_duration = FrameDuration::Duration10MS;

        manager.configure(0, qos(7, 0), sampling_frequency, frame_duration);
        manager.configure(1, qos(7, 1), sampling_frequency, frame_duration);
        let _ = manager.actions.try_receive(); // SetCigParameters
        manager.cig_parameters_set(ConnHandle::new(0x10), ConnHandle::new(0x11));
        let _ = manager.actions.try_receive(); // CreateCis

        let before_first = crate::test_alloc::allocated();
        manager.on_cis_established(&established_event(0x10));
        manager.on_cis_established(&established_event(0x11));
        let _ = manager.actions.try_receive(); // SetupDataPath (ase 0)
        let _ = manager.actions.try_receive(); // SetupDataPath (ase 1)
        let allocated_first = crate::test_alloc::allocated() - before_first;
        assert!(
            allocated_first > 20_000,
            "expected the first establishment to really allocate two encoders (~15904 bytes each at 48kHz/10ms), got {allocated_first}"
        );

        // Simulate a reconnect: reset, re-configure the same ASEs, fresh CIS handles.
        manager.reset();
        let _ = manager.actions.try_receive(); // RemoveCig
        manager.set_acl_handle(ConnHandle::new(0x06));
        manager.configure(0, qos(7, 0), sampling_frequency, frame_duration);
        manager.configure(1, qos(7, 1), sampling_frequency, frame_duration);
        let _ = manager.actions.try_receive(); // SetCigParameters
        manager.cig_parameters_set(ConnHandle::new(0x20), ConnHandle::new(0x21));
        let _ = manager.actions.try_receive(); // CreateCis

        let before_reconnect = crate::test_alloc::allocated();
        manager.on_cis_established(&established_event(0x20));
        manager.on_cis_established(&established_event(0x21));
        let _ = manager.actions.try_receive(); // SetupDataPath (ase 0)
        let _ = manager.actions.try_receive(); // SetupDataPath (ase 1)
        let allocated_on_reconnect = crate::test_alloc::allocated() - before_reconnect;
        assert_eq!(
            allocated_on_reconnect, 0,
            "reconnecting with unchanged audio params must not allocate new encoders (leaks ~15904 bytes/ASE/reconnect otherwise)"
        );

        // The reused encoders must still actually work.
        let pcm = [0i16; 480];
        let mut out = [0u8; 100];
        assert!(manager.encode(0, &pcm, &mut out).unwrap().is_ok());
        assert!(manager.encode(1, &pcm, &mut out).unwrap().is_ok());
    }
}
