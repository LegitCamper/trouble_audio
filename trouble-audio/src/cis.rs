//! Ties the ASE Control Point state machine to real CIS establishment and the ISO audio data
//! path: accepts/rejects incoming CIS requests, sets up the ISO data path once a CIS is
//! established, and runs LC3 encode/decode over the resulting ISO data - or, in passthrough mode
//! ([`CisManager::new_passthrough`]), hands the still-encoded LC3 frames straight to the
//! application with no codec at all.
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
use bt_hci::param::{CodecId, ConnHandle, DataPathDirection, DataPathId, ExtDuration, Status};
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::channel::Channel;
use heapless::Vec as HVec;
use trouble_host::prelude::*;


use crate::{
    ascs::{AscsServer, AseDirection, Operation},
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

/// Max encoded LC3 frame this crate handles - the ISO packet payload bound
/// ([`crate::iso_tx::MAX_ISO_PACKET_LEN`] minus its 8 header octets).
pub const MAX_LC3_FRAME_LEN: usize = crate::iso_tx::MAX_ISO_PACKET_LEN - 8;

/// One still-encoded LC3 frame, as delivered by a passthrough-mode [`CisManager`].
pub type Lc3Frame = HVec<u8, MAX_LC3_FRAME_LEN>;

/// A raw (still-encoded) LC3 frame from one Sink ASE - the passthrough-mode counterpart of
/// [`DecodedPcm`], for applications that consume LC3 directly (forwarding, file capture, an
/// external decoder) and don't want this crate to spend a decoder's ~28 KB and CPU on it.
#[derive(Debug, Clone)]
pub struct RawLc3 {
    pub ase_id: u8,
    pub channel_allocation: Option<AudioLocation>,
    pub frame: Lc3Frame,
}

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

/// Each variant is tagged with the [`AudioParams`] it was built for, so a reconnect with the same
/// params can reuse it instead of leaking a new one - see [`CisManager::on_cis_established`].
enum Codec {
    Encoder(AudioParams, Lc3MonoEncoder),
    Decoder(AudioParams, Lc3MonoDecoder),
}

/// What [`CisManager`] does with incoming ISO audio - fixed at construction.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SinkMode {
    /// Build an LC3 decoder per Sink ASE and deliver PCM via [`CisManager::receive_pcm`].
    Decode,
    /// Never construct a codec; deliver still-encoded frames via [`CisManager::receive_lc3`].
    Passthrough,
}

/// One frame on the shared sink queue - PCM in [`SinkMode::Decode`], raw LC3 in
/// [`SinkMode::Passthrough`], never mixed within one manager.
enum SinkFrame {
    Pcm(DecodedPcm),
    Lc3(RawLc3),
}

/// A pending action decided synchronously in an `EventHandler` callback, to be carried out by
/// [`drive_cis`] since it requires awaiting an HCI command.
enum CisAction {
    Accept(ConnHandle),
    Reject(ConnHandle, u8),
    /// `Some(ase_id)` if this is a Sink ASE, whose autonomous Enabling->Streaming transition
    /// should be queued onto `CisManager::streaming` once the data path is confirmed up.
    SetupDataPath(ConnHandle, DataPathDirection, Option<u8>),
}

/// Bridges the ASE Control Point state machine to real CIS/ISO setup for up to `MAX_ASES`
/// concurrently-configured ASEs. See the module docs for how to wire this up.
pub struct CisManager<M: RawMutex, const MAX_ASES: usize> {
    mode: SinkMode,
    slots: RefCell<[AseSlot; MAX_ASES]>,
    codecs: RefCell<[Option<Codec>; MAX_ASES]>,
    actions: Channel<M, CisAction, 4>,
    // 16, not 4: gives the consumer (e.g. `drive_led`) headroom against bursts at up to 200
    // frames/sec combined (stereo, 10ms each) - 4 caused near-constant drops on hardware.
    frames_out: Channel<M, SinkFrame, 16>,
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
        Self::with_mode(SinkMode::Decode)
    }

    /// Creates an empty manager in LC3 passthrough mode: no decoder is ever constructed (zero
    /// codec heap), and incoming frames arrive still-encoded via [`Self::receive_lc3`] instead of
    /// as PCM.
    pub fn new_passthrough() -> Self {
        Self::with_mode(SinkMode::Passthrough)
    }

    fn with_mode(mode: SinkMode) -> Self {
        Self {
            mode,
            slots: RefCell::new([AseSlot::default(); MAX_ASES]),
            codecs: RefCell::new(core::array::from_fn(|_| None)),
            actions: Channel::new(),
            frames_out: Channel::new(),
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

    /// Feeds one decoded ASE Control Point operation into this manager's side-table: Config
    /// Codec entries record codec parameters, Config QoS entries record the CIG/CIS identity.
    /// Call this alongside [`crate::bap::drive_ase_control_point`] for every ASE Control Point
    /// write (decode once via [`crate::ascs::AseControlPointOperation::operation`]).
    pub fn observe_operation<RM: RawMutex, P: PacketPool, const MAX_CONNECTIONS: usize>(
        &self,
        server: &AttributeServer<'_, RM, P, MAX_SERVICES, MAX_CONNECTIONS>,
        ascs: &AscsServer<MAX_ASES>,
        operation: &Operation,
    ) {
        if let Operation::ConfigCodec(entries) = operation {
            for (ase_id, _target_latency, _target_phy, _codec_id, config) in entries.iter() {
                let Some(direction) = find_ase_direction(server, ascs, *ase_id) else { continue };
                let Some(audio) = audio_params_from_config(config) else {
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
        if let Operation::ConfigQos(entries) = operation {
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
    /// from - see [`DecodedPcm`]. Only produces frames in the default decode mode.
    pub async fn receive_pcm(&self) -> DecodedPcm {
        loop {
            if let SinkFrame::Pcm(pcm) = self.frames_out.receive().await {
                return pcm;
            }
        }
    }

    /// Waits for the next still-encoded LC3 frame from any Sink ASE - see [`RawLc3`]. Only
    /// produces frames when built with [`Self::new_passthrough`].
    pub async fn receive_lc3(&self) -> RawLc3 {
        loop {
            if let SinkFrame::Lc3(raw) = self.frames_out.receive().await {
                return raw;
            }
        }
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
                info!("[cis] accepting CIS request (cig={} cis={})", event.cig_id, event.cis_id);
                let _ = self.actions.try_send(CisAction::Accept(event.cis_handle));
            }
            None => {
                drop(slots);
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
            warn!("[cis] CIS establishment failed");
            return;
        }

        let handle = event.handle.raw();
        let slot = {
            let slots = self.slots.borrow();
            let Some(idx) = slots.iter().position(|s| s.cis_handle == Some(handle)) else {
                warn!("[cis] CIS established for an untracked handle {}", handle);
                return;
            };
            (idx, slots[idx])
        };
        let (idx, slot) = slot;
        let (Some(audio), Some(direction)) = (slot.audio, slot.direction) else {
            warn!("[cis] CIS established but ASE was never Config Codec'd (no audio params)");
            return;
        };

        if self.mode == SinkMode::Passthrough {
            // Raw LC3 in/out: no codec at all.
        } else {
            // A reconnect lands back here with typically-unchanged audio params - reuse the
            // existing codec when it matches, rather than orphaning it for a fresh one.
            let already_matches = matches!(
                (&self.codecs.borrow()[idx], direction),
                (Some(Codec::Decoder(params, _)), AseDirection::Sink) if *params == audio
            ) || matches!(
                (&self.codecs.borrow()[idx], direction),
                (Some(Codec::Encoder(params, _)), AseDirection::Source) if *params == audio
            );
            if already_matches {
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
                        warn!("[cis] unsupported LC3 sampling frequency for established CIS");
                        return;
                    }
                }
            }
        }

        let data_path_direction = match direction {
            AseDirection::Sink => DataPathDirection::Output,
            AseDirection::Source => DataPathDirection::Input,
        };
        // Only a Sink ASE's Enabling->Streaming transition is autonomous; a Source ASE instead
        // waits for the client's Receiver Start Ready operation.
        let streaming_ase_id = matches!(direction, AseDirection::Sink).then_some(()).and(slot.ase_id);
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
                warn!("[cis] on_iso_data: no slot for handle={}", handle);
                return;
            };
            let Some(ase_id) = slots[idx].ase_id else {
                warn!("[cis] on_iso_data: slot {} has no ase_id", idx);
                return;
            };
            (idx, ase_id, slots[idx].audio.and_then(|a| a.channel_allocation))
        };

        if self.mode == SinkMode::Passthrough {
            let Ok(frame) = Lc3Frame::from_slice(packet.data()) else {
                warn!("[cis] on_iso_data: frame larger than MAX_LC3_FRAME_LEN, dropping");
                return;
            };
            if self
                .frames_out
                .try_send(SinkFrame::Lc3(RawLc3 {
                    ase_id,
                    channel_allocation,
                    frame,
                }))
                .is_err()
            {
                warn!("[cis] on_iso_data: frame channel full, dropping frame");
            }
            return;
        }

        let mut codecs = self.codecs.borrow_mut();
        let Some(Codec::Decoder(_, decoder)) = &mut codecs[idx] else {
            warn!("[cis] on_iso_data: slot {} has no decoder", idx);
            return;
        };

        let mut samples = PcmFrame::new();
        if samples.resize_default(decoder.samples_per_frame).is_err() {
            warn!("[cis] on_iso_data: resize_default failed");
            return;
        }
        match decoder.decode(packet.data(), &mut samples) {
            Ok(()) => {
                if self
                    .frames_out
                    .try_send(SinkFrame::Pcm(DecodedPcm {
                        ase_id,
                        channel_allocation,
                        samples,
                    }))
                    .is_err()
                {
                    warn!("[cis] on_iso_data: frame channel full, dropping frame");
                }
            }
            Err(_e) => {
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
                    warn!("[cis] LE Accept CIS Request failed");
                }
            }
            CisAction::Reject(handle, reason) => {
                if let Err(_e) = iso.command(LeRejectCisRequest::new(handle, reason)).await {
                    warn!("[cis] LE Reject CIS Request failed");
                }
            }
            CisAction::SetupDataPath(handle, direction, streaming_ase_id) => {
                let result = iso
                    .command(LeSetupIsoDataPath::new(
                        handle,
                        direction,
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
                        info!("[cis] ISO data path set up for handle {}", handle.raw());
                        if let Some(ase_id) = streaming_ase_id {
                            let _ = manager.streaming.try_send(ase_id);
                        }
                    }
                    Err(_e) => {
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
        info!(
            "[cis] controller CIS support: peripheral={} central={}",
            _features.supports_connected_isochronous_stream_peripheral(),
            _features.supports_connected_isochronous_stream_central()
        );
    }
    match stack.command(LeSetHostFeature::new(CIS_HOST_SUPPORT_FEATURE_BIT, 1)).await {
        Ok(_) => {
            info!("[cis] enabled Isochronous Channels (Host Support)");
            true
        }
        Err(_e) => {
            #[cfg(feature = "log")]
            log::warn!("[cis] LE Set Host Feature (CIS) failed: {:?}", _e);
            #[cfg(feature = "defmt")]
            defmt::warn!("[cis] LE Set Host Feature (CIS) failed");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use alloc::vec::Vec as AVec;

    use bt_hci::param::PhyKind;
    use bt_hci::FromHciBytes;
    use embassy_sync::blocking_mutex::raw::NoopRawMutex;

    use super::*;
    use crate::{
        ascs::{
            Ase, AscsServer, AseControlPointOperation, AseType, TargetLatency, ASE_CONTROL_POINT_STORE_SIZE,
            ASE_STORE_SIZE,
        },
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
        // Test fixtures leak their `'static` GATT stores (see the README's Miri note).
        let control_point_store: &'static mut [u8] = Box::leak(Box::new([0u8; ASE_CONTROL_POINT_STORE_SIZE]));
        let ase_store: &'static mut [u8] = Box::leak(Box::new([0u8; ASE_STORE_SIZE]));
        let ascs = AscsServer::new(&mut table, ases, control_point_store, [ase_store]);
        let server = AttributeServer::new(table);
        (ascs, server)
    }

    /// Round-trips through the wire encoding so the decoded `Operation` matches what a real
    /// central's write would produce.
    fn config_codec_operation(ase_id: u8, sampling_frequency: SamplingFrequency, frame_duration: FrameDuration) -> Operation {
        let config = encode_list(&[
            CodecSpecificConfiguration::SamplingFrequency(sampling_frequency),
            CodecSpecificConfiguration::FrameDuration(frame_duration),
            CodecSpecificConfiguration::OctetsPerCodecFrame(100),
        ]);
        let entries: AVec<_> = [(ase_id, TargetLatency::Lower, PhySet::M1, CodecId::default(), config)].into();
        AseControlPointOperation::config_codec(entries).operation().unwrap()
    }

    fn config_qos_operation(ase_id: u8, cig_id: u8, cis_id: u8) -> Operation {
        let entries: AVec<_> = [(ase_id, cig_id, cis_id, [0, 0, 0], 0u8, PhySet::M1, 100u16, 0u8, 10u16, [0, 0, 0])].into();
        AseControlPointOperation::config_qos(entries).operation().unwrap()
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
    #[cfg_attr(miri, ignore)] // constructs a real LC3 codec; libm's x86 asm sqrt is unsupported by Miri
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
                assert_eq!(direction, DataPathDirection::Output); // Sink ASE
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

        let pcm_out = match manager.frames_out.try_receive() {
            Ok(SinkFrame::Pcm(pcm)) => pcm,
            _ => panic!("expected a decoded PCM frame"),
        };
        assert_eq!(pcm_out.ase_id, 0);
        assert_eq!(pcm_out.samples.len(), encoder.samples_per_frame);
    }

    /// Regression test for the OOM (`handle_alloc_error`) crash repeated reconnects used to cause
    /// by leaking a new [`Lc3MonoDecoder`] every time.
    #[test]
    #[cfg_attr(miri, ignore)] // constructs a real LC3 codec; libm's x86 asm sqrt is unsupported by Miri
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

        // Simulate a reconnect: same Config Codec/QoS params, fresh CIS handle.
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
        let pcm_out = match manager.frames_out.try_receive() {
            Ok(SinkFrame::Pcm(pcm)) => pcm,
            _ => panic!("expected a decoded PCM frame from the reused decoder"),
        };
        assert_eq!(pcm_out.samples.len(), encoder.samples_per_frame);
    }

    /// Regression test for the param-change leak: replacing a cached decoder whose negotiated
    /// params changed used to orphan the old decoder's leaked working buffers (~27.5 KB at
    /// 48kHz/10ms) - repeated renegotiation would OOM.
    #[test]
    #[cfg_attr(miri, ignore)] // constructs a real LC3 codec; libm's x86 asm sqrt is unsupported by Miri
    fn renegotiating_different_audio_params_frees_the_replaced_decoder() {
        let (ascs, server) = build_ascs_and_server();
        let manager = CisManager::<NoopRawMutex, MAX_ASES>::new();

        manager.observe_operation(&server, &ascs, &config_codec_operation(0, SamplingFrequency::Hz48000, FrameDuration::Duration10MS));
        manager.observe_operation(&server, &ascs, &config_qos_operation(0, 7, 9));
        manager.on_cis_request(&LeCisRequest {
            acl_handle: ConnHandle::new(0x05),
            cis_handle: ConnHandle::new(0x11),
            cig_id: 7,
            cis_id: 9,
        });
        let _ = manager.actions.try_receive(); // Accept
        manager.on_cis_established(&established_event(0x11));
        let _ = manager.actions.try_receive(); // SetupDataPath

        // Renegotiate at a different sampling frequency: the 48 kHz decoder must be freed, not
        // orphaned, when the 16 kHz one replaces it.
        manager.observe_operation(&server, &ascs, &config_codec_operation(0, SamplingFrequency::Hz16000, FrameDuration::Duration10MS));
        manager.observe_operation(&server, &ascs, &config_qos_operation(0, 7, 9));
        manager.on_cis_request(&LeCisRequest {
            acl_handle: ConnHandle::new(0x06),
            cis_handle: ConnHandle::new(0x12),
            cig_id: 7,
            cis_id: 9,
        });
        let _ = manager.actions.try_receive(); // Accept

        let freed_before = crate::test_alloc::freed();
        manager.on_cis_established(&established_event(0x12));
        let _ = manager.actions.try_receive(); // SetupDataPath
        let freed_by_replacement = crate::test_alloc::freed() - freed_before;
        assert!(
            freed_by_replacement > 20_000,
            "replacing the 48kHz decoder must free its ~27564-byte working buffers, freed only {freed_by_replacement}"
        );
    }

    /// Passthrough mode: raw LC3 out, no decoder ever built (zero codec heap).
    #[test]
    fn passthrough_delivers_raw_lc3_frames_without_allocating_a_decoder() {
        let (ascs, server) = build_ascs_and_server();
        let manager = CisManager::<NoopRawMutex, MAX_ASES>::new_passthrough();

        manager.observe_operation(&server, &ascs, &config_codec_operation(0, SamplingFrequency::Hz48000, FrameDuration::Duration10MS));
        manager.observe_operation(&server, &ascs, &config_qos_operation(0, 7, 9));
        manager.on_cis_request(&LeCisRequest {
            acl_handle: ConnHandle::new(0x05),
            cis_handle: ConnHandle::new(0x11),
            cig_id: 7,
            cis_id: 9,
        });
        let _ = manager.actions.try_receive(); // Accept

        let before = crate::test_alloc::allocated();
        manager.on_cis_established(&established_event(0x11));
        assert_eq!(
            crate::test_alloc::allocated() - before,
            0,
            "passthrough must not allocate a decoder"
        );
        let _ = manager.actions.try_receive(); // SetupDataPath

        let payload = [0xC3u8; 100];
        let raw = iso_data_packet(0x11, &payload);
        let (packet, _) = IsoPacket::from_hci_bytes(&raw).unwrap();
        manager.on_iso_data(&packet);

        let frame = match manager.frames_out.try_receive() {
            Ok(SinkFrame::Lc3(raw)) => raw,
            _ => panic!("expected a raw LC3 frame"),
        };
        assert_eq!(frame.ase_id, 0);
        assert_eq!(&frame.frame[..], &payload[..]);
    }
}
