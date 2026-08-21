//! Auracast broadcast sink lifecycle: discover a Broadcast Audio Announcement, synchronize to its
//! periodic BASE, create a BIG sync, set up BIS data paths, and deliver LC3 or decoded PCM frames.
//!
//! [`BigSink`] is an [`EventHandler`] for
//! [`RxRunner::run_with_handler`](trouble_host::prelude::RxRunner::run_with_handler). HCI commands
//! selected by those synchronous callbacks are executed by [`drive_big_sink`], which must be
//! polled concurrently with the runner.

use core::cell::RefCell;

use alloc::vec::Vec;

use bt_hci::cmd::le::{
    LeBigCreateSync, LeBigTerminateSync, LePeriodicAdvCreateSync, LePeriodicAdvTerminateSync,
    LeRemoveIsoDataPath, LeSetupIsoDataPath,
};
use bt_hci::controller::{ControllerCmdAsync, ControllerCmdSync};
use bt_hci::data::IsoPacket;
use bt_hci::event::le::{
    LeBigSyncEstablished, LeBigSyncLost, LePeriodicAdvertisingReport,
    LePeriodicAdvertisingSyncEstablished, LePeriodicAdvertisingSyncLost,
};
use bt_hci::param::{
    BigHandle, BroadcastCode, ConnHandle, CteMask, DataPathDirection, DataPathId, DataStatus,
    Duration, EncryptionMode, ExtDuration, LePeriodicAdvCreateSyncOptions, SyncHandle,
};
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::channel::Channel;
use heapless::Vec as HVec;
use trouble_host::prelude::*;

use crate::CodingFormat;
use crate::big::{
    Base, BroadcastSource, MAX_BIS, parse_basic_audio_announcement,
    parse_broadcast_audio_announcement,
};
use crate::cis::{Lc3Frame, PcmFrame};
use crate::generic_audio::{AudioLocation, FrameDuration, SamplingFrequency};
use crate::lc3::Lc3MonoDecoder;

/// Maximum extended/periodic advertising payload defined by the Controller interface.
const MAX_PERIODIC_DATA_LEN: usize = 1_650;

/// Invalid host-side input rejected before starting an Auracast synchronization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastSinkConfigError {
    InvalidAdvertiserAddressType,
    InvalidAdvertisingSid,
    InvalidBigHandle,
    InvalidBis,
    DuplicateBis,
    InvalidSyncTimeout,
    InvalidMaxSubevents,
}

/// Failure to issue an Auracast synchronization command.
#[derive(Debug)]
pub enum BroadcastSinkError<E> {
    InvalidConfig(BroadcastSinkConfigError),
    Host(BleHostError<E>),
}

impl<E> From<BleHostError<E>> for BroadcastSinkError<E> {
    fn from(error: BleHostError<E>) -> Self {
        Self::Host(error)
    }
}

/// Controller parameters and BIS selection for one broadcast synchronization.
#[derive(Debug, Clone)]
pub struct BroadcastSinkConfig {
    /// Host-assigned BIG handle (0x00-0xEF).
    pub big_handle: u8,
    /// One-based BIS indices to synchronize to, in requested output order.
    pub bis: HVec<u8, MAX_BIS>,
    /// `Some` supplies the 16-octet Broadcast_Code for an encrypted broadcast.
    pub broadcast_code: Option<[u8; 16]>,
    /// Periodic advertising events the controller may skip.
    pub periodic_skip: u16,
    /// Periodic sync timeout in 10 ms units (0x000A-0x4000).
    pub periodic_sync_timeout_10ms: u16,
    /// Maximum BIG subevents to receive; zero asks the controller to receive all.
    pub max_subevents: u8,
    /// BIG sync timeout in 10 ms units (0x000A-0x4000).
    pub big_sync_timeout_10ms: u16,
}

impl BroadcastSinkConfig {
    pub fn validate(&self, source: &BroadcastSource) -> Result<(), BroadcastSinkConfigError> {
        if source.advertiser_address_type.as_raw() > 3 {
            return Err(BroadcastSinkConfigError::InvalidAdvertiserAddressType);
        }
        if source.advertising_sid > 0x0f {
            return Err(BroadcastSinkConfigError::InvalidAdvertisingSid);
        }
        if self.big_handle > 0xef {
            return Err(BroadcastSinkConfigError::InvalidBigHandle);
        }
        if self.bis.is_empty() || self.bis.iter().any(|index| !(1..=31).contains(index)) {
            return Err(BroadcastSinkConfigError::InvalidBis);
        }
        if self
            .bis
            .iter()
            .enumerate()
            .any(|(i, index)| self.bis[..i].contains(index))
        {
            return Err(BroadcastSinkConfigError::DuplicateBis);
        }
        if !(0x000a..=0x4000).contains(&self.periodic_sync_timeout_10ms)
            || !(0x000a..=0x4000).contains(&self.big_sync_timeout_10ms)
        {
            return Err(BroadcastSinkConfigError::InvalidSyncTimeout);
        }
        if self.max_subevents > 0x1f {
            return Err(BroadcastSinkConfigError::InvalidMaxSubevents);
        }
        Ok(())
    }
}

/// A raw LC3 frame received from a BIS.
#[derive(Debug, Clone)]
pub struct RawBroadcastLc3 {
    pub bis_index: u8,
    pub channel_allocation: Option<AudioLocation>,
    pub frame: Lc3Frame,
}

/// A decoded PCM frame received from a BIS.
#[derive(Debug, Clone)]
pub struct DecodedBroadcastPcm {
    pub bis_index: u8,
    pub channel_allocation: Option<AudioLocation>,
    pub samples: PcmFrame,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SinkMode {
    Decode,
    Passthrough,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct AudioParams {
    sampling_frequency: SamplingFrequency,
    frame_duration: FrameDuration,
    channel_allocation: Option<AudioLocation>,
}

#[derive(Default)]
struct PartialAudioParams {
    sampling_frequency: Option<SamplingFrequency>,
    frame_duration: Option<FrameDuration>,
    channel_allocation: Option<AudioLocation>,
}

#[derive(Default)]
struct BisSlot {
    index: Option<u8>,
    handle: Option<ConnHandle>,
    audio: Option<AudioParams>,
    decoder: Option<Lc3MonoDecoder>,
}

#[derive(Clone)]
struct Selection {
    source: BroadcastSource,
    config: BroadcastSinkConfig,
}

enum BigSinkAction {
    CreateBigSync(SyncHandle, BroadcastSinkConfig),
    SetupDataPath(ConnHandle, u8),
}

enum SinkFrame {
    Pcm(alloc::boxed::Box<DecodedBroadcastPcm>),
    Lc3(RawBroadcastLc3),
}

fn apply_codec_configuration(data: &[u8], params: &mut PartialAudioParams) -> Option<()> {
    let mut remaining = data;
    while !remaining.is_empty() {
        let len = usize::from(*remaining.first()?);
        if len == 0 || remaining.len() < len + 1 {
            return None;
        }
        let ty = remaining[1];
        let value = &remaining[2..len + 1];
        match ty {
            1 if value.len() == 1 => {
                params.sampling_frequency = Some(SamplingFrequency::try_from(value[0]).ok()?)
            }
            2 if value.len() == 1 => {
                params.frame_duration = Some(FrameDuration::try_from(value[0]).ok()?)
            }
            3 if value.len() == 4 => {
                params.channel_allocation = Some(AudioLocation::from_bits_truncate(
                    u32::from_le_bytes(value.try_into().ok()?),
                ));
            }
            _ => {}
        }
        remaining = &remaining[len + 1..];
    }
    Some(())
}

fn audio_params_for_bis(base: &Base, bis_index: u8) -> Option<AudioParams> {
    for subgroup in &base.subgroups {
        let Some(bis) = subgroup.bis.iter().find(|bis| bis.index == bis_index) else {
            continue;
        };
        if subgroup.codec_id.coding_format != CodingFormat::LC3 {
            return None;
        }
        let mut params = PartialAudioParams::default();
        apply_codec_configuration(&subgroup.codec_configuration, &mut params)?;
        apply_codec_configuration(&bis.codec_configuration, &mut params)?;
        return Some(AudioParams {
            sampling_frequency: params.sampling_frequency?,
            frame_duration: params.frame_duration?,
            channel_allocation: params.channel_allocation,
        });
    }
    None
}

/// Owns the host-side state for one Auracast broadcast sink.
pub struct BigSink<M: RawMutex> {
    mode: SinkMode,
    selection: RefCell<Option<Selection>>,
    periodic_sync_handle: RefCell<Option<SyncHandle>>,
    big_sync_pending: RefCell<bool>,
    big_sync_established: RefCell<bool>,
    periodic_data: RefCell<Vec<u8>>,
    slots: RefCell<[BisSlot; MAX_BIS]>,
    actions: Channel<M, BigSinkAction, 4>,
    discovered: Channel<M, BroadcastSource, 4>,
    bases: Channel<M, Base, 2>,
    ready: Channel<M, u8, 4>,
    frames: Channel<M, SinkFrame, 16>,
}

impl<M: RawMutex> Default for BigSink<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M: RawMutex> BigSink<M> {
    /// Creates a sink that decodes received LC3 frames to PCM.
    pub fn new() -> Self {
        Self::with_mode(SinkMode::Decode)
    }

    /// Creates a sink that delivers LC3 without allocating decoders.
    pub fn new_passthrough() -> Self {
        Self::with_mode(SinkMode::Passthrough)
    }

    fn with_mode(mode: SinkMode) -> Self {
        Self {
            mode,
            selection: RefCell::new(None),
            periodic_sync_handle: RefCell::new(None),
            big_sync_pending: RefCell::new(false),
            big_sync_established: RefCell::new(false),
            periodic_data: RefCell::new(Vec::new()),
            slots: RefCell::new(core::array::from_fn(|_| BisSlot::default())),
            actions: Channel::new(),
            discovered: Channel::new(),
            bases: Channel::new(),
            ready: Channel::new(),
            frames: Channel::new(),
        }
    }

    fn select(&self, source: BroadcastSource, config: BroadcastSinkConfig) {
        let selected_bis = config.bis.clone();
        *self.selection.borrow_mut() = Some(Selection { source, config });
        *self.periodic_sync_handle.borrow_mut() = None;
        *self.big_sync_pending.borrow_mut() = false;
        *self.big_sync_established.borrow_mut() = false;
        self.periodic_data.borrow_mut().clear();
        let mut slots = self.slots.borrow_mut();
        for slot in slots.iter_mut() {
            *slot = BisSlot::default();
        }
        for (slot, index) in slots.iter_mut().zip(&selected_bis) {
            slot.index = Some(*index);
        }
    }

    /// Waits for a discovered Broadcast Audio Announcement.
    pub async fn next_broadcast(&self) -> BroadcastSource {
        self.discovered.receive().await
    }

    /// Waits for a complete, valid BASE from the selected broadcast.
    pub async fn next_base(&self) -> Base {
        self.bases.receive().await
    }

    /// Waits for a BIS data path and returns its one-based BIS index.
    pub async fn next_ready_bis(&self) -> u8 {
        self.ready.receive().await
    }

    /// Returns a synchronized BIS's controller connection handle.
    pub fn bis_handle(&self, bis_index: u8) -> Option<ConnHandle> {
        self.slots
            .borrow()
            .iter()
            .find(|slot| slot.index == Some(bis_index))
            .and_then(|slot| slot.handle)
    }

    /// Waits for a decoded broadcast PCM frame. Only the default mode produces these.
    pub async fn receive_pcm(&self) -> DecodedBroadcastPcm {
        loop {
            if let SinkFrame::Pcm(frame) = self.frames.receive().await {
                return *frame;
            }
        }
    }

    /// Waits for a raw broadcast LC3 frame. Only passthrough mode produces these.
    pub async fn receive_lc3(&self) -> RawBroadcastLc3 {
        loop {
            if let SinkFrame::Lc3(frame) = self.frames.receive().await {
                return frame;
            }
        }
    }

    fn configure_base(&self, base: &Base) -> bool {
        let mut slots = self.slots.borrow_mut();
        for slot in slots.iter_mut().filter(|slot| slot.index.is_some()) {
            let Some(audio) = audio_params_for_bis(base, slot.index.unwrap()) else {
                return false;
            };
            slot.audio = Some(audio);
            if self.mode == SinkMode::Decode {
                let Ok(decoder) =
                    Lc3MonoDecoder::new(audio.sampling_frequency, audio.frame_duration)
                else {
                    return false;
                };
                slot.decoder = Some(decoder);
            }
        }
        true
    }

    fn clear_sync(&self) {
        *self.periodic_sync_handle.borrow_mut() = None;
        *self.big_sync_pending.borrow_mut() = false;
        *self.big_sync_established.borrow_mut() = false;
        self.periodic_data.borrow_mut().clear();
        for slot in self.slots.borrow_mut().iter_mut() {
            slot.handle = None;
        }
    }
}

impl<M: RawMutex> EventHandler for BigSink<M> {
    fn on_ext_adv_reports(&self, reports: bt_hci::param::LeExtAdvReportsIter<'_>) {
        for report in reports.flatten() {
            let Ok(Some(broadcast_id)) = parse_broadcast_audio_announcement(report.data) else {
                continue;
            };
            let _ = self.discovered.try_send(BroadcastSource {
                advertiser_address_type: report.addr_kind,
                advertiser_address: report.addr,
                advertising_sid: report.adv_sid,
                broadcast_id,
            });
        }
    }

    fn on_periodic_adv_sync_established(&self, event: &LePeriodicAdvertisingSyncEstablished) {
        if event.status.to_result().is_err() {
            self.clear_sync();
            return;
        }
        let matches = self.selection.borrow().as_ref().is_some_and(|selection| {
            event.adv_sid == selection.source.advertising_sid
                && event.adv_addr_kind == selection.source.advertiser_address_type
                && event.adv_addr == selection.source.advertiser_address
        });
        if matches {
            *self.periodic_sync_handle.borrow_mut() = Some(event.sync_handle);
        }
    }

    fn on_periodic_adv_report(&self, event: &LePeriodicAdvertisingReport<'_>) {
        if *self.periodic_sync_handle.borrow() != Some(event.sync_handle)
            || *self.big_sync_pending.borrow()
        {
            return;
        }

        let data = match event.data_status {
            DataStatus::Incomplete => {
                let mut accumulated = self.periodic_data.borrow_mut();
                if accumulated.len().saturating_add(event.data.len()) > MAX_PERIODIC_DATA_LEN {
                    accumulated.clear();
                } else {
                    accumulated.extend_from_slice(event.data);
                }
                return;
            }
            DataStatus::Failed => {
                self.periodic_data.borrow_mut().clear();
                return;
            }
            DataStatus::Complete => {
                let mut accumulated = self.periodic_data.borrow_mut();
                if accumulated.len().saturating_add(event.data.len()) > MAX_PERIODIC_DATA_LEN {
                    accumulated.clear();
                    return;
                }
                if accumulated.is_empty() {
                    Vec::from(event.data)
                } else {
                    accumulated.extend_from_slice(event.data);
                    core::mem::take(&mut *accumulated)
                }
            }
        };
        let Ok(Some(base)) = parse_basic_audio_announcement(&data) else {
            return;
        };
        if !self.configure_base(&base) {
            return;
        }
        let Some(selection) = self.selection.borrow().as_ref().cloned() else {
            return;
        };
        if self
            .actions
            .try_send(BigSinkAction::CreateBigSync(
                event.sync_handle,
                selection.config,
            ))
            .is_ok()
        {
            *self.big_sync_pending.borrow_mut() = true;
            let _ = self.bases.try_send(base);
        }
    }

    fn on_periodic_adv_sync_lost(&self, event: &LePeriodicAdvertisingSyncLost) {
        if *self.periodic_sync_handle.borrow() == Some(event.sync_handle) {
            self.clear_sync();
        }
    }

    fn on_big_sync_established(&self, event: &LeBigSyncEstablished<'_>) {
        let Some(selection) = self.selection.borrow().as_ref().cloned() else {
            return;
        };
        if event.big_handle.into_inner() != selection.config.big_handle {
            return;
        }
        if event.status.to_result().is_err() {
            *self.big_sync_pending.borrow_mut() = false;
            *self.big_sync_established.borrow_mut() = false;
            return;
        }
        *self.big_sync_established.borrow_mut() = true;
        let mut slots = self.slots.borrow_mut();
        for (slot, bis) in slots.iter_mut().zip(event.bis_handles.iter()) {
            let Ok(handle) = bis.handle() else {
                continue;
            };
            slot.handle = Some(handle);
            if let Some(index) = slot.index {
                let _ = self
                    .actions
                    .try_send(BigSinkAction::SetupDataPath(handle, index));
            }
        }
    }

    fn on_big_sync_lost(&self, event: &LeBigSyncLost) {
        let matches =
            self.selection.borrow().as_ref().is_some_and(|selection| {
                event.big_handle.into_inner() == selection.config.big_handle
            });
        if matches {
            *self.big_sync_pending.borrow_mut() = false;
            *self.big_sync_established.borrow_mut() = false;
            for slot in self.slots.borrow_mut().iter_mut() {
                slot.handle = None;
            }
        }
    }

    fn on_iso_data(&self, packet: &IsoPacket<'_>) {
        let mut slots = self.slots.borrow_mut();
        let Some(slot) = slots
            .iter_mut()
            .find(|slot| slot.handle == Some(packet.handle()))
        else {
            return;
        };
        let (Some(bis_index), Some(audio)) = (slot.index, slot.audio) else {
            return;
        };
        if self.mode == SinkMode::Passthrough {
            let Ok(frame) = Lc3Frame::from_slice(packet.data()) else {
                return;
            };
            let _ = self.frames.try_send(SinkFrame::Lc3(RawBroadcastLc3 {
                bis_index,
                channel_allocation: audio.channel_allocation,
                frame,
            }));
            return;
        }
        let Some(decoder) = &mut slot.decoder else {
            return;
        };
        let mut samples = PcmFrame::new();
        if samples.resize_default(decoder.samples_per_frame).is_err() {
            return;
        }
        if decoder.decode(packet.data(), &mut samples).is_ok() {
            let _ = self.frames.try_send(SinkFrame::Pcm(alloc::boxed::Box::new(
                DecodedBroadcastPcm {
                    bis_index,
                    channel_allocation: audio.channel_allocation,
                    samples,
                },
            )));
        }
    }
}

/// Starts periodic-advertising synchronization to a discovered broadcast.
pub async fn start_broadcast_sync<C, M: RawMutex>(
    stack: &Stack<'_, C, impl PacketPool>,
    sink: &BigSink<M>,
    source: BroadcastSource,
    config: BroadcastSinkConfig,
) -> Result<(), BroadcastSinkError<C::Error>>
where
    C: Controller + ControllerCmdAsync<LePeriodicAdvCreateSync>,
{
    config
        .validate(&source)
        .map_err(BroadcastSinkError::InvalidConfig)?;
    sink.select(source, config.clone());
    let result = stack
        .iso()
        .command_async(LePeriodicAdvCreateSync::new(
            LePeriodicAdvCreateSyncOptions::new().enable_duplicate_filtering(true),
            source.advertising_sid,
            source.advertiser_address_type,
            source.advertiser_address,
            config.periodic_skip,
            Duration::from_u16(config.periodic_sync_timeout_10ms),
            CteMask::new(),
        ))
        .await;
    if let Err(error) = result {
        sink.clear_sync();
        *sink.selection.borrow_mut() = None;
        return Err(error.into());
    }
    Ok(())
}

/// Stops the BIG and periodic-advertising synchronizations owned by `sink`.
pub async fn stop_broadcast_sync<C, M: RawMutex>(
    stack: &Stack<'_, C, impl PacketPool>,
    sink: &BigSink<M>,
) -> Result<(), BleHostError<C::Error>>
where
    C: Controller
        + ControllerCmdSync<LeBigTerminateSync>
        + ControllerCmdSync<LePeriodicAdvTerminateSync>,
{
    let big_handle = sink
        .selection
        .borrow()
        .as_ref()
        .map(|selection| selection.config.big_handle);
    if *sink.big_sync_established.borrow() {
        if let Some(big_handle) = big_handle {
            stack
                .iso()
                .command(LeBigTerminateSync::new(BigHandle(big_handle)))
                .await?;
        }
    }
    let periodic_sync_handle = *sink.periodic_sync_handle.borrow();
    if let Some(sync_handle) = periodic_sync_handle {
        stack
            .command(LePeriodicAdvTerminateSync::new(sync_handle))
            .await?;
    }
    sink.clear_sync();
    Ok(())
}

/// Executes BIG synchronization and BIS data-path commands selected by [`BigSink`]'s event
/// callbacks. Poll concurrently with the Trouble runner.
pub async fn drive_big_sink<C, M: RawMutex>(
    stack: &Stack<'_, C, impl PacketPool>,
    sink: &BigSink<M>,
) -> !
where
    C: Controller
        + for<'a> ControllerCmdAsync<LeBigCreateSync<'a>>
        + for<'a> ControllerCmdSync<LeSetupIsoDataPath<'a>>
        + ControllerCmdSync<LeRemoveIsoDataPath>,
{
    let iso = stack.iso();
    loop {
        match sink.actions.receive().await {
            BigSinkAction::CreateBigSync(sync_handle, config) => {
                let (encryption, code) = match config.broadcast_code {
                    Some(code) => (EncryptionMode::Encrypted, code),
                    None => (EncryptionMode::Unencrypted, [0; 16]),
                };
                if iso
                    .command_async(LeBigCreateSync::new(
                        BigHandle(config.big_handle),
                        sync_handle,
                        encryption,
                        BroadcastCode::new(code),
                        config.max_subevents,
                        config.big_sync_timeout_10ms,
                        config.bis.len() as u8,
                        &config.bis,
                    ))
                    .await
                    .is_err()
                {
                    *sink.big_sync_pending.borrow_mut() = false;
                }
            }
            BigSinkAction::SetupDataPath(handle, bis_index) => {
                if iso
                    .command(LeSetupIsoDataPath::new(
                        handle,
                        DataPathDirection::Output,
                        DataPathId::HCI,
                        bt_hci::param::CodecId {
                            coding_format: u8::from(CodingFormat::Transparent),
                            company_id: 0,
                            vendor_specific_codec_id: 0,
                        },
                        ExtDuration::from_u32(0),
                        &[],
                    ))
                    .await
                    .is_ok()
                {
                    let _ = sink.ready.try_send(bis_index);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bt_hci::FromHciBytes;
    use bt_hci::event::le::LeBigSyncEstablished;
    use bt_hci::param::{AddrKind, BdAddr, CteKind, PhyKind, Status};
    use embassy_sync::blocking_mutex::raw::NoopRawMutex;

    use super::*;
    use crate::big::{BroadcastConfig, basic_audio_announcement, encode_base};
    use crate::generic_audio::{ContextType, Metadata};

    fn source() -> BroadcastSource {
        BroadcastSource {
            advertiser_address_type: AddrKind::RANDOM,
            advertiser_address: BdAddr::new([1, 2, 3, 4, 5, 0xc0]),
            advertising_sid: 3,
            broadcast_id: [0xaa, 0xbb, 0xcc],
        }
    }

    fn sink_config() -> BroadcastSinkConfig {
        let mut bis = HVec::new();
        bis.push(1).unwrap();
        bis.push(2).unwrap();
        BroadcastSinkConfig {
            big_handle: 1,
            bis,
            broadcast_code: None,
            periodic_skip: 0,
            periodic_sync_timeout_10ms: 100,
            max_subevents: 0,
            big_sync_timeout_10ms: 100,
        }
    }

    fn source_config() -> BroadcastConfig {
        let mut bis = HVec::new();
        bis.push(AudioLocation::FrontLeft).unwrap();
        bis.push(AudioLocation::FrontRight).unwrap();
        BroadcastConfig {
            big_handle: 1,
            adv_handle: 2,
            adv_sid: 3,
            random_addr: [1, 2, 3, 4, 5, 0xc0],
            broadcast_id: [0xaa, 0xbb, 0xcc],
            bis,
            sampling_frequency: SamplingFrequency::Hz48000,
            frame_duration: FrameDuration::Duration10MS,
            octets_per_frame: 100,
            sdu_interval_us: 10_000,
            max_transport_latency_ms: 20,
            rtn: 2,
            presentation_delay_us: 40_000,
            streaming_contexts: ContextType::Media,
            broadcast_code: None,
        }
    }

    fn establish_periodic_sync(sink: &BigSink<NoopRawMutex>) -> SyncHandle {
        let sync_handle = SyncHandle(0x20);
        sink.on_periodic_adv_sync_established(&LePeriodicAdvertisingSyncEstablished {
            status: Status::SUCCESS,
            sync_handle,
            adv_sid: source().advertising_sid,
            adv_addr_kind: source().advertiser_address_type,
            adv_addr: source().advertiser_address,
            adv_phy: PhyKind::Le1M,
            periodic_adv_interval: Duration::from_u16(80),
            adv_clock_accuracy: Default::default(),
        });
        sync_handle
    }

    fn big_sync_established() -> LeBigSyncEstablished<'static> {
        const BYTES: [u8; 18] = [
            0x00, // Status
            0x01, // BIG_Handle
            0x00, 0x00, 0x00, // Transport_Latency_BIG
            1, 1, 0, 1, // NSE, BN, PTO, IRC
            100, 0, // Max_PDU
            8, 0, // ISO_Interval
            2, // Num_BIS
            0x10, 0x00, 0x11, 0x00, // BIS connection handles
        ];
        LeBigSyncEstablished::from_hci_bytes_complete(&BYTES).unwrap()
    }

    fn iso_data_packet(handle: u16, payload: &[u8]) -> alloc::vec::Vec<u8> {
        let handle_word = (handle & 0x0fff) | (0b10 << 12);
        let mut out = alloc::vec::Vec::new();
        out.extend_from_slice(&handle_word.to_le_bytes());
        out.extend_from_slice(&(4u16 + payload.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn config_rejects_empty_duplicate_and_out_of_range_bis_indices() {
        let mut config = sink_config();
        config.bis.clear();
        assert_eq!(
            config.validate(&source()),
            Err(BroadcastSinkConfigError::InvalidBis)
        );

        config.bis.push(1).unwrap();
        config.bis.push(1).unwrap();
        assert_eq!(
            config.validate(&source()),
            Err(BroadcastSinkConfigError::DuplicateBis)
        );

        config.bis[1] = 32;
        assert_eq!(
            config.validate(&source()),
            Err(BroadcastSinkConfigError::InvalidBis)
        );

        let mut anonymous = source();
        anonymous.advertiser_address_type = AddrKind::ANONYMOUS_ADV;
        assert_eq!(
            sink_config().validate(&anonymous),
            Err(BroadcastSinkConfigError::InvalidAdvertiserAddressType)
        );
    }

    #[test]
    fn source_base_provides_lc3_parameters_for_each_selected_bis() {
        let base = crate::big::decode_base(&encode_base(&source_config())).unwrap();
        let left = audio_params_for_bis(&base, 1).unwrap();
        let right = audio_params_for_bis(&base, 2).unwrap();

        assert_eq!(left.sampling_frequency, SamplingFrequency::Hz48000);
        assert_eq!(left.frame_duration, FrameDuration::Duration10MS);
        assert_eq!(left.channel_allocation, Some(AudioLocation::FrontLeft));
        assert_eq!(right.channel_allocation, Some(AudioLocation::FrontRight));
    }

    #[test]
    fn unknown_codec_and_metadata_ltv_entries_do_not_break_audio_parameter_extraction() {
        let mut base = crate::big::decode_base(&encode_base(&source_config())).unwrap();
        base.subgroups[0]
            .codec_configuration
            .extend_from_slice(&[2, 0xfe, 0x99]);
        base.subgroups[0].metadata =
            crate::generic_audio::encode_list(&[Metadata::ProgramInfo("test".into())]);

        assert!(audio_params_for_bis(&base, 1).is_some());
    }

    #[test]
    fn periodic_advertisement_from_source_round_trips_into_sink_base() {
        let encoded = encode_base(&source_config());
        let periodic = basic_audio_announcement(&encoded);
        let parsed = parse_basic_audio_announcement(&periodic).unwrap().unwrap();

        assert_eq!(parsed.presentation_delay_us, 40_000);
        assert_eq!(parsed.subgroups.len(), 1);
        assert_eq!(parsed.subgroups[0].bis.len(), 2);
    }

    #[test]
    fn fragmented_periodic_base_is_reassembled_before_big_sync() {
        let sink = BigSink::<NoopRawMutex>::new_passthrough();
        sink.select(source(), sink_config());
        let sync_handle = establish_periodic_sync(&sink);
        let periodic = basic_audio_announcement(&encode_base(&source_config()));
        let split = periodic.len() / 2;

        sink.on_periodic_adv_report(&LePeriodicAdvertisingReport {
            sync_handle,
            tx_power: 0,
            rssi: -40,
            cte_kind: CteKind::NoCte,
            data_status: DataStatus::Incomplete,
            data: &periodic[..split],
        });
        assert!(sink.actions.try_receive().is_err());
        sink.on_periodic_adv_report(&LePeriodicAdvertisingReport {
            sync_handle,
            tx_power: 0,
            rssi: -40,
            cte_kind: CteKind::NoCte,
            data_status: DataStatus::Complete,
            data: &periodic[split..],
        });

        assert!(matches!(
            sink.actions.try_receive(),
            Ok(BigSinkAction::CreateBigSync(handle, _)) if handle == sync_handle
        ));
        assert!(sink.periodic_data.borrow().is_empty());
    }

    #[test]
    fn passthrough_lifecycle_reaches_both_bis_and_delivers_lc3() {
        let sink = BigSink::<NoopRawMutex>::new_passthrough();
        sink.select(source(), sink_config());
        let sync_handle = establish_periodic_sync(&sink);

        let periodic = basic_audio_announcement(&encode_base(&source_config()));
        sink.on_periodic_adv_report(&LePeriodicAdvertisingReport {
            sync_handle,
            tx_power: 0,
            rssi: -40,
            cte_kind: CteKind::NoCte,
            data_status: DataStatus::Complete,
            data: &periodic,
        });
        assert!(sink.bases.try_receive().is_ok());
        match sink.actions.try_receive() {
            Ok(BigSinkAction::CreateBigSync(handle, config)) => {
                assert_eq!(handle, sync_handle);
                assert_eq!(&config.bis[..], &[1, 2]);
            }
            other => panic!("expected CreateBigSync, got {:?}", other.is_ok()),
        }

        sink.on_big_sync_established(&big_sync_established());
        match sink.actions.try_receive() {
            Ok(BigSinkAction::SetupDataPath(handle, 1)) => assert_eq!(handle.raw(), 0x10),
            other => panic!("expected first SetupDataPath, got {:?}", other.is_ok()),
        }
        match sink.actions.try_receive() {
            Ok(BigSinkAction::SetupDataPath(handle, 2)) => assert_eq!(handle.raw(), 0x11),
            other => panic!("expected second SetupDataPath, got {:?}", other.is_ok()),
        }
        assert_eq!(sink.bis_handle(1).unwrap().raw(), 0x10);
        assert_eq!(sink.bis_handle(2).unwrap().raw(), 0x11);

        let payload = [0xc3; 100];
        let raw = iso_data_packet(0x10, &payload);
        let (packet, rest) = IsoPacket::from_hci_bytes(&raw).unwrap();
        assert!(rest.is_empty());
        sink.on_iso_data(&packet);
        let frame = match sink.frames.try_receive() {
            Ok(SinkFrame::Lc3(frame)) => frame,
            _ => panic!("expected raw LC3 frame"),
        };
        assert_eq!(frame.bis_index, 1);
        assert_eq!(frame.channel_allocation, Some(AudioLocation::FrontLeft));
        assert_eq!(&frame.frame[..], &payload);
    }

    #[test]
    fn decoded_lifecycle_turns_a_bis_lc3_frame_into_pcm() {
        let sink = BigSink::<NoopRawMutex>::new();
        sink.select(source(), sink_config());
        let sync_handle = establish_periodic_sync(&sink);
        let periodic = basic_audio_announcement(&encode_base(&source_config()));
        sink.on_periodic_adv_report(&LePeriodicAdvertisingReport {
            sync_handle,
            tx_power: 0,
            rssi: -40,
            cte_kind: CteKind::NoCte,
            data_status: DataStatus::Complete,
            data: &periodic,
        });
        sink.on_big_sync_established(&big_sync_established());

        let mut encoder = crate::lc3::Lc3MonoEncoder::new(
            SamplingFrequency::Hz48000,
            FrameDuration::Duration10MS,
        )
        .unwrap();
        let pcm_in: alloc::vec::Vec<i16> = (0..encoder.samples_per_frame)
            .map(|sample| if sample % 48 < 24 { 6_000 } else { -6_000 })
            .collect();
        let mut encoded = [0u8; 100];
        encoder.encode(&pcm_in, &mut encoded).unwrap();
        let raw = iso_data_packet(0x10, &encoded);
        let (packet, _) = IsoPacket::from_hci_bytes(&raw).unwrap();
        sink.on_iso_data(&packet);

        let frame = match sink.frames.try_receive() {
            Ok(SinkFrame::Pcm(frame)) => frame,
            _ => panic!("expected decoded PCM frame"),
        };
        assert_eq!(frame.bis_index, 1);
        assert_eq!(frame.samples.len(), encoder.samples_per_frame);
        assert!(frame.samples.iter().any(|sample| *sample != 0));
    }
}
