//! Ties the ASE Control Point state machine to real CIS establishment and the ISO audio data
//! path: accepts/rejects incoming CIS requests, sets up the ISO data path once a CIS is
//! established, and runs LC3 encode/decode over the resulting ISO data.
//!
//! ASCS's own `AseState` (see `ascs.rs`) only carries the fields the spec defines for an ASE's
//! *current* state - codec parameters aren't part of QosConfigured/Enabling/Streaming, even
//! though CIS/ISO setup still needs them. [`CisManager`] keeps its own side-table of that
//! information, fed by [`CisManager::observe_operation`] as the ASE Control Point is driven.
//!
//! [`CisManager::on_cis_request`]/[`on_cis_established`](CisManager::on_cis_established)/
//! [`on_iso_data`](CisManager::on_iso_data) implement [`EventHandler`] so a [`CisManager`] can be
//! handed straight to [`RxRunner::run_with_handler`](trouble_host::prelude::RxRunner::run_with_handler).
//! Those callbacks are synchronous, but accepting/rejecting a CIS and setting up its ISO data
//! path require awaiting HCI commands - so they only decide and hand off to [`drive_cis`], which
//! must be polled concurrently (e.g. via `select`) to actually send those commands.

use core::cell::RefCell;

use bt_hci::cmd::le::{
    LeAcceptCisRequest, LeReadLocalSupportedFeatures, LeRejectCisRequest, LeRemoveIsoDataPath, LeSetHostFeature,
    LeSetupIsoDataPath,
};
use bt_hci::controller::{ControllerCmdAsync, ControllerCmdSync};
use bt_hci::data::IsoPacket;
use bt_hci::event::le::{LeCisEstablished, LeCisRequest};
use bt_hci::param::{ConnHandle, IsoDataPathDirection, Status};

/// The standard HCI transport `Data_Path_ID`, as opposed to a vendor-specific one.
const DATA_PATH_ID_HCI: u8 = 0x00;
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::channel::Channel;
use heapless::Vec as HVec;
use trouble_host::prelude::*;

#[cfg(feature = "defmt")]
use defmt::{info, warn};

use crate::{
    ascs::{AscsServer, AseControlPointOperation, AseDirection},
    generic_audio::{decode_list, AudioLocation, CodecSpecificConfiguration, FrameDuration, SamplingFrequency},
    lc3::{Lc3MonoDecoder, Lc3MonoEncoder},
    CodingFormat, MAX_SERVICES,
};

/// HCI error code used to reject a CIS request that doesn't match any QoS-configured ASE
/// ("Unacceptable Connection Parameters", Core 5 Vol 2, Part D).
const REJECT_REASON_UNACCEPTABLE_CIG_PARAMETERS: u8 = 0x3b;

/// PCM samples per LC3 frame at the largest configuration this crate's [`crate::lc3`] wrapper
/// supports (48 kHz, 10 ms).
pub const MAX_PCM_SAMPLES_PER_FRAME: usize = 480;

/// One decoded LC3 frame's worth of PCM samples.
pub type PcmFrame = HVec<i16, MAX_PCM_SAMPLES_PER_FRAME>;

/// A decoded LC3 frame from one Sink ASE, tagged with which ASE (and, if the central declared
/// one via Config Codec's `Audio_Channel_Allocation`, which audio channel location) it came from
/// - needed to tell e.g. left from right when more than one ASE is streaming, since [`CisManager`]
/// multiplexes every ASE's decoded audio onto one [`CisManager::receive_pcm`] queue.
#[derive(Debug, Clone)]
pub struct DecodedPcm {
    pub ase_id: u8,
    pub channel_allocation: Option<AudioLocation>,
    pub samples: PcmFrame,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct AudioParams {
    sampling_frequency: SamplingFrequency,
    frame_duration: FrameDuration,
    channel_allocation: Option<AudioLocation>,
}

fn audio_params_from_config(config: &[u8]) -> Option<AudioParams> {
    let entries: alloc::vec::Vec<CodecSpecificConfiguration> = decode_list(config).ok()?;
    let mut sampling_frequency = None;
    let mut frame_duration = None;
    let mut channel_allocation = None;
    for entry in entries {
        match entry {
            CodecSpecificConfiguration::SamplingFrequency(v) => sampling_frequency = Some(v),
            CodecSpecificConfiguration::FrameDuration(v) => frame_duration = Some(v),
            CodecSpecificConfiguration::AudioChannelAllocation(v) => channel_allocation = Some(v),
            _ => {}
        }
    }
    Some(AudioParams {
        sampling_frequency: sampling_frequency?,
        frame_duration: frame_duration?,
        channel_allocation,
    })
}

#[derive(Clone, Copy, Default)]
struct AseSlot {
    ase_id: Option<u8>,
    direction: Option<AseDirection>,
    audio: Option<AudioParams>,
    cig_id: Option<u8>,
    cis_id: Option<u8>,
    cis_handle: Option<u16>,
}

/// Each variant is tagged with the [`AudioParams`] it was built for, so a reconnect that
/// renegotiates the exact same Sampling_Frequency/Frame_Duration can reuse the existing,
/// already-`Box::leak`'d codec instead of allocating (and leaking) a brand new one - see
/// [`CisManager::on_cis_established`].
enum Codec {
    Encoder(AudioParams, Lc3MonoEncoder),
    Decoder(AudioParams, Lc3MonoDecoder),
}

/// A pending action decided synchronously in an `EventHandler` callback, to be carried out by
/// [`drive_cis`] since it requires awaiting an HCI command.
enum CisAction {
    Accept(ConnHandle),
    Reject(ConnHandle, u8),
    /// `Some(ase_id)` if this is a Sink ASE, whose autonomous Enabling->Streaming transition
    /// should be queued onto `CisManager::streaming` once the data path is confirmed up.
    SetupDataPath(ConnHandle, IsoDataPathDirection, Option<u8>),
}

/// Bridges the ASE Control Point state machine to real CIS/ISO setup for up to `MAX_ASES`
/// concurrently-configured ASEs. See the module docs for how to wire this up.
pub struct CisManager<M: RawMutex, const MAX_ASES: usize> {
    slots: RefCell<[AseSlot; MAX_ASES]>,
    codecs: RefCell<[Option<Codec>; MAX_ASES]>,
    actions: Channel<M, CisAction, 4>,
    // A stereo stream decodes at up to 200 frames/sec combined (two ASEs @ 10ms each); 4 slots
    // left almost no slack for the consumer (e.g. `drive_led`) to fall even slightly behind a
    // burst - observed as near-constant "pcm_out channel full, dropping frame" warnings on
    // hardware. 16 gives ~80ms of burst headroom at that rate.
    pcm_out: Channel<M, DecodedPcm, 16>,
    streaming: Channel<M, u8, 4>,
}

impl<M: RawMutex, const MAX_ASES: usize> Default for CisManager<M, MAX_ASES> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M: RawMutex, const MAX_ASES: usize> CisManager<M, MAX_ASES> {
    /// Creates an empty manager, before any ASE has been configured.
    pub fn new() -> Self {
        Self {
            slots: RefCell::new([AseSlot::default(); MAX_ASES]),
            codecs: RefCell::new(core::array::from_fn(|_| None)),
            actions: Channel::new(),
            pcm_out: Channel::new(),
            streaming: Channel::new(),
        }
    }

    fn slot_index_for_ase(slots: &mut [AseSlot; MAX_ASES], ase_id: u8) -> Option<usize> {
        if let Some(idx) = slots.iter().position(|s| s.ase_id == Some(ase_id)) {
            return Some(idx);
        }
        let idx = slots.iter().position(|s| s.ase_id.is_none())?;
        slots[idx].ase_id = Some(ase_id);
        Some(idx)
    }

    /// Feeds one ASE Control Point operation into this manager's side-table: Config Codec
    /// entries record codec parameters, Config QoS entries record the CIG/CIS identity. Call
    /// this alongside [`crate::bap::drive_ase_control_point`] for every ASE Control Point write.
    pub fn observe_operation<RM: RawMutex, P: PacketPool, const MAX_CONNECTIONS: usize>(
        &self,
        server: &AttributeServer<'_, RM, P, MAX_SERVICES, MAX_CONNECTIONS>,
        ascs: &AscsServer<MAX_ASES>,
        operation: &AseControlPointOperation,
    ) {
        if let Some(entries) = operation.as_config_codec() {
            for (ase_id, _target_latency, _target_phy, _codec_id, config) in entries.iter() {
                let Some(direction) = find_ase_direction(server, ascs, *ase_id) else { continue };
                let Some(audio) = audio_params_from_config(config) else {
                    #[cfg(feature = "log")]
                    log::warn!("[cis] ASE {} Config Codec missing sampling frequency/frame duration", ase_id);
                    #[cfg(feature = "defmt")]
                    warn!("[cis] ASE {} Config Codec missing sampling frequency/frame duration", ase_id);
                    continue;
                };
                let mut slots = self.slots.borrow_mut();
                if let Some(idx) = Self::slot_index_for_ase(&mut slots, *ase_id) {
                    slots[idx].direction = Some(direction);
                    slots[idx].audio = Some(audio);
                }
            }
        }
        if let Some(entries) = operation.as_config_qos() {
            for (ase_id, cig_id, cis_id, ..) in entries.iter() {
                let mut slots = self.slots.borrow_mut();
                if let Some(idx) = Self::slot_index_for_ase(&mut slots, *ase_id) {
                    slots[idx].cig_id = Some(*cig_id);
                    slots[idx].cis_id = Some(*cis_id);
                }
            }
        }
    }

    /// Waits for the next decoded PCM frame from any Sink ASE, tagged with which ASE it came
    /// from - see [`DecodedPcm`].
    pub async fn receive_pcm(&self) -> DecodedPcm {
        self.pcm_out.receive().await
    }

    /// Waits for the next Sink ASE ready to autonomously move `Enabling` -> `Streaming`. The
    /// caller must apply that transition itself, e.g. via [`crate::bap::notify_ase_streaming`] -
    /// this manager has no access to the `AttributeServer`/`GattConnection` needed to do so.
    pub async fn next_streaming_ase(&self) -> u8 {
        self.streaming.receive().await
    }
}

impl<M: RawMutex, const MAX_ASES: usize> EventHandler for CisManager<M, MAX_ASES> {
    fn on_cis_request(&self, event: &LeCisRequest) {
        let mut slots = self.slots.borrow_mut();
        let matched = slots
            .iter()
            .position(|s| s.cig_id == Some(event.cig_id) && s.cis_id == Some(event.cis_id));
        match matched {
            Some(idx) => {
                slots[idx].cis_handle = Some(event.cis_handle.raw());
                drop(slots);
                #[cfg(feature = "log")]
                log::info!("[cis] accepting CIS request (cig={} cis={})", event.cig_id, event.cis_id);
                #[cfg(feature = "defmt")]
                info!("[cis] accepting CIS request (cig={} cis={})", event.cig_id, event.cis_id);
                let _ = self.actions.try_send(CisAction::Accept(event.cis_handle));
            }
            None => {
                drop(slots);
                #[cfg(feature = "log")]
                log::warn!(
                    "[cis] rejecting CIS request: no QoS-configured ASE for cig={} cis={}",
                    event.cig_id, event.cis_id
                );
                #[cfg(feature = "defmt")]
                warn!(
                    "[cis] rejecting CIS request: no QoS-configured ASE for cig={} cis={}",
                    event.cig_id, event.cis_id
                );
                let _ = self
                    .actions
                    .try_send(CisAction::Reject(event.cis_handle, REJECT_REASON_UNACCEPTABLE_CIG_PARAMETERS));
            }
        }
    }

    fn on_cis_established(&self, event: &LeCisEstablished) {
        if event.status != Status::SUCCESS {
            #[cfg(feature = "log")]
            log::warn!("[cis] CIS establishment failed");
            #[cfg(feature = "defmt")]
            warn!("[cis] CIS establishment failed");
            return;
        }

        let handle = event.handle.raw();
        let slot = {
            let slots = self.slots.borrow();
            let Some(idx) = slots.iter().position(|s| s.cis_handle == Some(handle)) else {
                #[cfg(feature = "log")]
                log::warn!("[cis] CIS established for an untracked handle {}", handle);
                #[cfg(feature = "defmt")]
                warn!("[cis] CIS established for an untracked handle {}", handle);
                return;
            };
            (idx, slots[idx])
        };
        let (idx, slot) = slot;
        let (Some(audio), Some(direction)) = (slot.audio, slot.direction) else {
            #[cfg(feature = "log")]
            log::warn!("[cis] CIS established but ASE was never Config Codec'd (no audio params)");
            #[cfg(feature = "defmt")]
            warn!("[cis] CIS established but ASE was never Config Codec'd (no audio params)");
            return;
        };

        // A reconnect (e.g. after the peer disconnects/reconnects mid-session, such as on a
        // track skip) re-runs the whole ASE Control Point state machine and lands back here even
        // though the negotiated audio params are typically unchanged. `Lc3MonoDecoder`/
        // `Lc3MonoEncoder` permanently `Box::leak` their working buffers (see `lc3.rs`), so
        // allocating a fresh one on every such reconnect leaks ~27-55KB per channel each time and
        // eventually exhausts the heap (`handle_alloc_error` panic). Reuse the existing codec
        // already sitting in this slot when it matches, rather than always allocating anew.
        let already_matches = matches!(
            (&self.codecs.borrow()[idx], direction),
            (Some(Codec::Decoder(params, _)), AseDirection::Sink) if *params == audio
        ) || matches!(
            (&self.codecs.borrow()[idx], direction),
            (Some(Codec::Encoder(params, _)), AseDirection::Source) if *params == audio
        );
        if already_matches {
            #[cfg(feature = "log")]
            log::info!("[cis] reusing existing codec for ase slot {} (reconnect, same audio params)", idx);
            #[cfg(feature = "defmt")]
            info!("[cis] reusing existing codec for ase slot {} (reconnect, same audio params)", idx);
        } else {
            let codec = match direction {
                AseDirection::Sink => Lc3MonoDecoder::new(audio.sampling_frequency, audio.frame_duration)
                    .ok()
                    .map(|d| Codec::Decoder(audio, d)),
                AseDirection::Source => Lc3MonoEncoder::new(audio.sampling_frequency, audio.frame_duration)
                    .ok()
                    .map(|e| Codec::Encoder(audio, e)),
            };
            match codec {
                Some(codec) => self.codecs.borrow_mut()[idx] = Some(codec),
                None => {
                    #[cfg(feature = "log")]
                    log::warn!("[cis] unsupported LC3 sampling frequency for established CIS");
                    #[cfg(feature = "defmt")]
                    warn!("[cis] unsupported LC3 sampling frequency for established CIS");
                    return;
                }
            }
        }

        let data_path_direction = match direction {
            AseDirection::Sink => IsoDataPathDirection::Output,
            AseDirection::Source => IsoDataPathDirection::Input,
        };
        // Only a Sink ASE's Enabling->Streaming transition is autonomous; a Source ASE instead
        // waits for the client's Receiver Start Ready operation.
        let streaming_ase_id = matches!(direction, AseDirection::Sink).then_some(()).and(slot.ase_id);
        #[cfg(feature = "log")]
        log::info!("[cis] CIS established, setting up ISO data path");
        #[cfg(feature = "defmt")]
        info!("[cis] CIS established, setting up ISO data path");
        let _ = self
            .actions
            .try_send(CisAction::SetupDataPath(event.handle, data_path_direction, streaming_ase_id));
    }

    fn on_iso_data(&self, packet: &IsoPacket<'_>) {
        let handle = packet.handle().raw();
        let (idx, ase_id, channel_allocation) = {
            let slots = self.slots.borrow();
            let Some(idx) = slots.iter().position(|s| s.cis_handle == Some(handle)) else {
                #[cfg(feature = "log")]
                log::warn!("[cis] on_iso_data: no slot for handle={}", handle);
                #[cfg(feature = "defmt")]
                warn!("[cis] on_iso_data: no slot for handle={}", handle);
                return;
            };
            let Some(ase_id) = slots[idx].ase_id else {
                #[cfg(feature = "log")]
                log::warn!("[cis] on_iso_data: slot {} has no ase_id", idx);
                #[cfg(feature = "defmt")]
                warn!("[cis] on_iso_data: slot {} has no ase_id", idx);
                return;
            };
            (idx, ase_id, slots[idx].audio.and_then(|a| a.channel_allocation))
        };
        let mut codecs = self.codecs.borrow_mut();
        let Some(Codec::Decoder(_, decoder)) = &mut codecs[idx] else {
            #[cfg(feature = "log")]
            log::warn!("[cis] on_iso_data: slot {} has no decoder", idx);
            #[cfg(feature = "defmt")]
            warn!("[cis] on_iso_data: slot {} has no decoder", idx);
            return;
        };

        let mut samples = PcmFrame::new();
        if samples.resize_default(decoder.samples_per_frame).is_err() {
            #[cfg(feature = "log")]
            log::warn!("[cis] on_iso_data: resize_default failed");
            #[cfg(feature = "defmt")]
            warn!("[cis] on_iso_data: resize_default failed");
            return;
        }
        match decoder.decode(packet.data(), &mut samples) {
            Ok(()) => {
                if self
                    .pcm_out
                    .try_send(DecodedPcm {
                        ase_id,
                        channel_allocation,
                        samples,
                    })
                    .is_err()
                {
                    #[cfg(feature = "log")]
                    log::warn!("[cis] on_iso_data: pcm_out channel full, dropping frame");
                    #[cfg(feature = "defmt")]
                    warn!("[cis] on_iso_data: pcm_out channel full, dropping frame");
                }
            }
            Err(_e) => {
                #[cfg(feature = "log")]
                log::warn!("[cis] LC3 decode error");
                #[cfg(feature = "defmt")]
                warn!("[cis] LC3 decode error");
            }
        }
    }
}

fn find_ase_direction<M: RawMutex, P: PacketPool, const MAX_ASES: usize, const MAX_CONNECTIONS: usize>(
    server: &AttributeServer<'_, M, P, MAX_SERVICES, MAX_CONNECTIONS>,
    ascs: &AscsServer<MAX_ASES>,
    ase_id: u8,
) -> Option<AseDirection> {
    ascs.ases().iter().find_map(|(direction, characteristic)| {
        let ase = characteristic.get(server).ok()?;
        (ase.id() == ase_id).then_some(*direction)
    })
}

/// Drives the HCI side of CIS/ISO setup: accepts/rejects CIS requests and sets up the ISO data
/// path, as decided synchronously by `manager`'s [`EventHandler`] callbacks. Must be polled
/// concurrently with [`RxRunner::run_with_handler`](trouble_host::prelude::RxRunner::run_with_handler)
/// (e.g. via `select`) for those decisions to actually reach the controller.
pub async fn drive_cis<C, M: RawMutex, const MAX_ASES: usize>(stack: &Stack<'_, C, impl PacketPool>, manager: &CisManager<M, MAX_ASES>) -> !
where
    C: Controller
        + ControllerCmdAsync<LeAcceptCisRequest>
        + ControllerCmdSync<LeRejectCisRequest>
        + for<'a> ControllerCmdSync<LeSetupIsoDataPath<'a>>
        + ControllerCmdSync<LeRemoveIsoDataPath>,
{
    let iso = stack.iso();
    loop {
        match manager.actions.receive().await {
            CisAction::Accept(handle) => {
                if let Err(_e) = iso.command_async(LeAcceptCisRequest::new(handle)).await {
                    #[cfg(feature = "log")]
                    log::warn!("[cis] LE Accept CIS Request failed");
                    #[cfg(feature = "defmt")]
                    warn!("[cis] LE Accept CIS Request failed");
                }
            }
            CisAction::Reject(handle, reason) => {
                if let Err(_e) = iso.command(LeRejectCisRequest::new(handle, reason)).await {
                    #[cfg(feature = "log")]
                    log::warn!("[cis] LE Reject CIS Request failed");
                    #[cfg(feature = "defmt")]
                    warn!("[cis] LE Reject CIS Request failed");
                }
            }
            CisAction::SetupDataPath(handle, direction, streaming_ase_id) => {
                let result = iso
                    .command(LeSetupIsoDataPath::new(
                        handle,
                        direction,
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
                        log::info!("[cis] ISO data path set up for handle {}", handle.raw());
                        #[cfg(feature = "defmt")]
                        info!("[cis] ISO data path set up for handle {}", handle.raw());
                        if let Some(ase_id) = streaming_ase_id {
                            let _ = manager.streaming.try_send(ase_id);
                        }
                    }
                    Err(_e) => {
                        #[cfg(feature = "log")]
                        log::warn!("[cis] LE Setup ISO Data Path failed");
                        #[cfg(feature = "defmt")]
                        warn!("[cis] LE Setup ISO Data Path failed");
                    }
                }
            }
        }
    }
}

/// LE feature bit for "Connected Isochronous Stream (Host Support)" (Core 6, Vol 6, Part B,
/// Section 4.6) - set via `HCI_LE_Set_Host_Feature` to opt in to using CIS. Without it, a peer's
/// link-layer feature exchange sees CIS as unsupported and never attempts `LE_Create_CIS`.
const CIS_HOST_SUPPORT_FEATURE_BIT: u8 = 32;

/// Enables CIS host support on `stack`'s controller - required once at startup before any CIS
/// can be created, by either side. Await concurrently with [`drive_cis`] and the connection
/// runner, and start it before advertising/scanning: doing this too late risks losing a race
/// against a startup resolving-list sync, rejected with "Command Disallowed" - in which case this
/// returns `false` and callers should retry after a short delay rather than give up permanently.
pub async fn enable_cis_host_support<C>(stack: &Stack<'_, C, impl PacketPool>) -> bool
where
    C: Controller + ControllerCmdSync<LeSetHostFeature> + ControllerCmdSync<LeReadLocalSupportedFeatures>,
{
    #[cfg(any(feature = "log", feature = "defmt"))]
    if let Ok(_features) = stack.command(LeReadLocalSupportedFeatures::new()).await {
        #[cfg(feature = "log")]
        log::info!(
            "[cis] controller CIS support: peripheral={} central={}",
            _features.supports_connected_isochronous_stream_peripheral(),
            _features.supports_connected_isochronous_stream_central()
        );
        #[cfg(feature = "defmt")]
        info!(
            "[cis] controller CIS support: peripheral={} central={}",
            _features.supports_connected_isochronous_stream_peripheral(),
            _features.supports_connected_isochronous_stream_central()
        );
    }
    match stack.command(LeSetHostFeature::new(CIS_HOST_SUPPORT_FEATURE_BIT, 1)).await {
        Ok(_) => {
            #[cfg(feature = "log")]
            log::info!("[cis] enabled Isochronous Channels (Host Support)");
            #[cfg(feature = "defmt")]
            info!("[cis] enabled Isochronous Channels (Host Support)");
            true
        }
        Err(_e) => {
            #[cfg(feature = "log")]
            log::warn!("[cis] LE Set Host Feature (CIS) failed: {:?}", _e);
            #[cfg(feature = "defmt")]
            warn!("[cis] LE Set Host Feature (CIS) failed");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec as AVec;

    use bt_hci::param::PhyKind;
    use bt_hci::FromHciBytes;
    use embassy_sync::blocking_mutex::raw::NoopRawMutex;

    use super::*;
    use crate::{
        ascs::{Ase, AscsServer, AseControlPointOperation, AseType, TargetLatency},
        generic_audio::{encode_list, FrameDuration, SamplingFrequency},
        lc3::Lc3MonoEncoder,
        CodecId,
    };

    const MAX_ASES: usize = 1;

    /// Builds a real (if minimal) ASCS service + attribute server with one Sink ASE, matching
    /// what `ServerBuilder`/the example apps build - `observe_operation` looks ASE direction up
    /// through these, so a synthetic pair is needed to test it without a live GATT connection.
    fn build_ascs_and_server() -> (
        AscsServer<MAX_ASES>,
        AttributeServer<'static, NoopRawMutex, DefaultPacketPool, MAX_SERVICES, 1>,
    ) {
        let mut table: AttributeTable<'static, NoopRawMutex, MAX_SERVICES> = AttributeTable::new();
        let mut ases = HVec::new();
        let _ = ases.push(AseType::Sink(Ase::new(0)));
        let ascs = AscsServer::new(&mut table, ases);
        let server = AttributeServer::new(table);
        (ascs, server)
    }

    fn config_codec_operation(ase_id: u8, sampling_frequency: SamplingFrequency, frame_duration: FrameDuration) -> AseControlPointOperation {
        let config = encode_list(&[
            CodecSpecificConfiguration::SamplingFrequency(sampling_frequency),
            CodecSpecificConfiguration::FrameDuration(frame_duration),
            CodecSpecificConfiguration::OctetsPerCodecFrame(100),
        ]);
        let entries: AVec<_> = [(ase_id, TargetLatency::Lower, PhySet::M1, CodecId::default(), config)].into();
        AseControlPointOperation::config_codec(entries)
    }

    fn config_qos_operation(ase_id: u8, cig_id: u8, cis_id: u8) -> AseControlPointOperation {
        let entries: AVec<_> = [(ase_id, cig_id, cis_id, [0, 0, 0], 0u8, PhySet::M1, 100u16, 0u8, 10u16, [0, 0, 0])].into();
        AseControlPointOperation::config_qos(entries)
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
            bn_c_to_p: 0,
            bn_p_to_c: 1,
            ft_c_to_p: 0,
            ft_p_to_c: 1,
            max_pdu_c_to_p: 0,
            max_pdu_p_to_c: 100,
            iso_interval: Default::default(),
        }
    }

    /// Hand-encodes a single, complete, non-timestamped HCI ISO data packet - mirrors the
    /// approach used to test the receive-path plumbing in the `trouble` fork itself, since
    /// `IsoPacket` has no public constructor other than parsing from wire bytes.
    fn iso_data_packet(cis_handle: u16, payload: &[u8]) -> AVec<u8> {
        const PB_COMPLETE: u16 = 0b10;
        let handle_word = (cis_handle & 0x0fff) | (PB_COMPLETE << 12);
        let data_load_len = 4 + payload.len();
        let mut out = AVec::new();
        out.extend_from_slice(&handle_word.to_le_bytes());
        out.extend_from_slice(&(data_load_len as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // sequence_num
        out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn full_cis_lifecycle_accepts_sets_up_data_path_and_decodes_audio() {
        let (ascs, server) = build_ascs_and_server();
        let manager = CisManager::<NoopRawMutex, MAX_ASES>::new();

        let sampling_frequency = SamplingFrequency::Hz48000;
        let frame_duration = FrameDuration::Duration10MS;
        manager.observe_operation(&server, &ascs, &config_codec_operation(0, sampling_frequency, frame_duration));
        manager.observe_operation(&server, &ascs, &config_qos_operation(0, 7, 9));

        // A CIS request for an unrelated (cig, cis) pair must be rejected...
        manager.on_cis_request(&LeCisRequest {
            acl_handle: ConnHandle::new(0x05),
            cis_handle: ConnHandle::new(0x50),
            cig_id: 1,
            cis_id: 1,
        });
        match manager.actions.try_receive() {
            Ok(CisAction::Reject(handle, _)) => assert_eq!(handle.raw(), 0x50),
            other => panic!("expected Reject, got {:?}", other.is_ok()),
        }

        // ...but a request matching the QoS-configured ASE must be accepted.
        manager.on_cis_request(&LeCisRequest {
            acl_handle: ConnHandle::new(0x05),
            cis_handle: ConnHandle::new(0x11),
            cig_id: 7,
            cis_id: 9,
        });
        match manager.actions.try_receive() {
            Ok(CisAction::Accept(handle)) => assert_eq!(handle.raw(), 0x11),
            other => panic!("expected Accept, got {:?}", other.is_ok()),
        }

        manager.on_cis_established(&established_event(0x11));
        match manager.actions.try_receive() {
            Ok(CisAction::SetupDataPath(handle, direction, streaming_ase_id)) => {
                assert_eq!(handle.raw(), 0x11);
                assert_eq!(direction, IsoDataPathDirection::Output); // Sink ASE
                assert_eq!(streaming_ase_id, Some(0)); // Sink ASE: autonomous transition
            }
            other => panic!("expected SetupDataPath, got {:?}", other.is_ok()),
        }

        // Encode a real LC3 frame at the negotiated config and feed it in as if it arrived over
        // the air, then check it comes out the other end as decoded PCM.
        let mut encoder = Lc3MonoEncoder::new(sampling_frequency, frame_duration).unwrap();
        let pcm_in: AVec<i16> = (0..encoder.samples_per_frame).map(|i| (i as i16).wrapping_mul(37)).collect();
        let mut frame = [0u8; 100];
        encoder.encode(&pcm_in, &mut frame).unwrap();

        let raw = iso_data_packet(0x11, &frame);
        let (packet, rest) = IsoPacket::from_hci_bytes(&raw).unwrap();
        assert!(rest.is_empty());
        manager.on_iso_data(&packet);

        let pcm_out = manager.pcm_out.try_receive().expect("expected a decoded PCM frame");
        assert_eq!(pcm_out.ase_id, 0);
        assert_eq!(pcm_out.samples.len(), encoder.samples_per_frame);
    }

    /// Regression test for the OOM (`handle_alloc_error`) crash a repeated disconnect/reconnect
    /// (e.g. skipping tracks) used to cause after enough reconnects burned through the heap - see
    /// the fix's comment in `on_cis_established`: it must stop allocating (and `Box::leak`ing) a
    /// brand new [`Lc3MonoDecoder`] on every reconnect.
    #[test]
    fn reconnecting_with_the_same_audio_params_reuses_the_existing_decoder() {
        let (ascs, server) = build_ascs_and_server();
        let manager = CisManager::<NoopRawMutex, MAX_ASES>::new();

        let sampling_frequency = SamplingFrequency::Hz48000;
        let frame_duration = FrameDuration::Duration10MS;
        manager.observe_operation(&server, &ascs, &config_codec_operation(0, sampling_frequency, frame_duration));
        manager.observe_operation(&server, &ascs, &config_qos_operation(0, 7, 9));

        manager.on_cis_request(&LeCisRequest {
            acl_handle: ConnHandle::new(0x05),
            cis_handle: ConnHandle::new(0x11),
            cig_id: 7,
            cis_id: 9,
        });
        let _ = manager.actions.try_receive(); // Accept

        let before_first = crate::test_alloc::allocated();
        manager.on_cis_established(&established_event(0x11));
        let _ = manager.actions.try_receive(); // SetupDataPath
        let allocated_first = crate::test_alloc::allocated() - before_first;
        assert!(
            allocated_first > 20_000,
            "expected the first establishment to really allocate a decoder (~27564 bytes at 48kHz/10ms), got {allocated_first}"
        );

        // Simulate a reconnect: the peer re-runs Config Codec/Config QoS with the exact same
        // params, and the controller hands out a fresh CIS handle for the new CIS instance -
        // `on_cis_established` must recognize the existing decoder in this ASE's slot still
        // matches and reuse it rather than allocating (and leaking) another one.
        manager.observe_operation(&server, &ascs, &config_codec_operation(0, sampling_frequency, frame_duration));
        manager.observe_operation(&server, &ascs, &config_qos_operation(0, 7, 9));
        manager.on_cis_request(&LeCisRequest {
            acl_handle: ConnHandle::new(0x06),
            cis_handle: ConnHandle::new(0x12),
            cig_id: 7,
            cis_id: 9,
        });
        let _ = manager.actions.try_receive(); // Accept

        let before_reconnect = crate::test_alloc::allocated();
        manager.on_cis_established(&established_event(0x12));
        let _ = manager.actions.try_receive(); // SetupDataPath
        let allocated_on_reconnect = crate::test_alloc::allocated() - before_reconnect;
        assert_eq!(
            allocated_on_reconnect, 0,
            "reconnecting with unchanged audio params must not allocate a new decoder (leaks ~27564 bytes/reconnect otherwise)"
        );

        // The reused decoder must still actually work.
        let mut encoder = Lc3MonoEncoder::new(sampling_frequency, frame_duration).unwrap();
        let pcm_in: AVec<i16> = (0..encoder.samples_per_frame).map(|i| (i as i16).wrapping_mul(37)).collect();
        let mut frame = [0u8; 100];
        encoder.encode(&pcm_in, &mut frame).unwrap();
        let raw = iso_data_packet(0x12, &frame);
        let (packet, rest) = IsoPacket::from_hci_bytes(&raw).unwrap();
        assert!(rest.is_empty());
        manager.on_iso_data(&packet);
        let pcm_out = manager.pcm_out.try_receive().expect("expected a decoded PCM frame from the reused decoder");
        assert_eq!(pcm_out.samples.len(), encoder.samples_per_frame);
    }
}
