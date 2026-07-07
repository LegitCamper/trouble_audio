//! A use-case-driven, single entry point for common LE Audio device shapes: pick a [`DeviceRole`]
//! ("a headset", "a mic", "a headset with a mic"), toggle whichever optional capabilities apply
//! (volume control, mic mute, coordinated-set membership, hearing-aid presets, call control,
//! gaming-role advertisement), and call [`Scenario::run`] - without needing to know PACS, ASCS,
//! or any of the dozen optional GATT services by name. This sits on top of, and does not replace,
//! the lower-level [`crate::le_audio::run_peripheral`]/[`crate::server::ServerBuilder`] - reach
//! for those directly if a scenario here doesn't fit.
//!
//! # Two known gaps (deliberately not hidden)
//!
//! 1. **Encoded mic/source audio is never actually sent over the air yet.** `DeviceRole::Microphone`
//!    and `DeviceRole::HeadsetWithMic` correctly negotiate a Source ASE (so a peer's Config
//!    Codec/QoS/Enable procedure completes normally) and expose a working Microphone Control
//!    Service mute toggle, but [`crate::cis::CisManager`] only *constructs* an LC3 encoder for a
//!    Source ASE (see `cis.rs`'s `on_cis_established`) - nothing anywhere in this crate calls
//!    `.encode()` on it or sends an outbound HCI ISO data packet. There's no "send" primitive at
//!    all yet. So a peer will see a plausible-looking mic, but no audio actually arrives.
//! 2. **Auracast (broadcast) isn't implemented at all.** There's no `LE Create BIG`/periodic
//!    advertising support anywhere in this crate or the underlying `trouble-host` fork.
//!    [`DeviceRole::AuracastSink`]/[`DeviceRole::AuracastSource`] exist as variants so this API's
//!    shape won't need to change once that support exists, but [`Scenario::run`] returns
//!    [`ScenarioError::NotImplemented`] immediately for them, before touching the radio.

use core::convert::Infallible;

use bt_hci::cmd::le::{LeReadLocalSupportedFeatures, LeSetHostFeature};
use bt_hci::controller::{ControllerCmdAsync, ControllerCmdSync};
use embassy_sync::blocking_mutex::raw::RawMutex;
use heapless::Vec as HVec;
use trouble_host::prelude::*;

use crate::{
    ascs::{Ase, AseType},
    cis::CisManager,
    csis::Sirk,
    generic_audio::{
        AudioLocation, CodecSpecificCapabilities, ContextType, OctetsPerCodecFrame, SamplingFrequency, SupportedFrameDurations,
        SupportedSamplingFrequencies,
    },
    gmas::GmapRole,
    has::{HearingAidFeatures, PresetRecord},
    iso::{LeAcceptCisRequest, LeRejectCisRequest, LeRemoveIsoDataPath, LeSetupIsoDataPath},
    le_audio::{self, BondStore},
    mics,
    pacs::{AudioContexts, PAC, PACRecord},
    server::{HAS_MAX_PRESETS, TBS_MAX_CALLS},
    tbs::{self, TbsInit, TbsStore},
    vcs::{self, VolumeFlags, VolumeState},
    CodecId, ServerBuilder,
};

/// Which physical role this device plays. See the module doc comment for what's real vs. stubbed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceRole {
    /// Sink-only (e.g. headphones/speaker) - works end-to-end today.
    Headset,
    /// Source-only (e.g. a standalone mic) - GATT/control-plane only, see gap #1 in the module doc.
    Microphone,
    /// Sink+source (e.g. a phone headset) - same caveat as `Microphone`.
    HeadsetWithMic,
    /// Broadcast receiver - not implemented, see gap #2 in the module doc.
    AuracastSink,
    /// Broadcast transmitter - not implemented, see gap #2 in the module doc.
    AuracastSource,
}

impl DeviceRole {
    fn has_sink(self) -> bool {
        matches!(self, Self::Headset | Self::HeadsetWithMic)
    }

    fn has_source(self) -> bool {
        matches!(self, Self::Microphone | Self::HeadsetWithMic)
    }

    fn is_broadcast(self) -> bool {
        matches!(self, Self::AuracastSink | Self::AuracastSource)
    }

    fn appearance(self) -> BluetoothUuid16 {
        match self {
            Self::Headset | Self::HeadsetWithMic => appearance::wearable_audio_device::HEADSET,
            Self::Microphone => appearance::audio_source::MICROPHONE,
            Self::AuracastSink => appearance::audio_sink::GENERIC_AUDIO_SINK,
            Self::AuracastSource => appearance::audio_source::BROADCASTING_DEVICE,
        }
    }
}

/// Returned by [`Scenario::run`] before it does anything else, if `role` isn't implemented yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScenarioError {
    NotImplemented(&'static str),
}

/// Checks whether `role` can actually run today. Split out from [`Scenario::run`] so it's
/// testable without a real `Controller`.
fn validate_role(role: DeviceRole) -> Result<(), ScenarioError> {
    if role.is_broadcast() {
        return Err(ScenarioError::NotImplemented(
            "Auracast (broadcast) isn't implemented yet - no BIG/BIS or periodic advertising support exists in trouble-audio or trouble-host",
        ));
    }
    Ok(())
}

/// The ASE endpoints `role` implies: a Sink ASE (id 0) if `role` has a sink, then a Source ASE
/// (the next free id) if `role` has a source. Split out from [`Scenario::run`] so it's testable
/// without a real `Controller`.
fn ase_list<const MAX_ASES: usize>(role: DeviceRole) -> HVec<AseType, MAX_ASES> {
    let mut ases = HVec::new();
    if role.has_sink() {
        let _ = ases.push(AseType::Sink(Ase::new(0)));
    }
    if role.has_source() {
        let _ = ases.push(AseType::Source(Ase::new(if role.has_sink() { 1 } else { 0 })));
    }
    ases
}

/// The Supported/Available_Audio_Contexts `role` implies - a sink context of Media|Conversational
/// for playback, a source context of Conversational for a mic (voice, not media, is what a
/// microphone's audio is normally used for). Split out from [`Scenario::run`] so it's testable
/// without a real `Controller`.
fn audio_contexts(role: DeviceRole) -> AudioContexts {
    AudioContexts {
        sink_contexts: if role.has_sink() {
            ContextType::Media | ContextType::Conversational
        } else {
            ContextType::empty()
        },
        source_contexts: if role.has_source() { ContextType::Conversational } else { ContextType::empty() },
    }
}

fn pac_record(sample_rate: SamplingFrequency) -> PACRecord {
    PACRecord {
        codec_id: CodecId::default(), // LC3
        codec_specific_capabilities: alloc::vec![
            CodecSpecificCapabilities::SupportedSamplingFrequencies(SupportedSamplingFrequencies::new(&[sample_rate])),
            // Both of these are mandatory per the PACS spec - see the analogous comment in
            // basic_audio_sink.rs for what breaks without them.
            CodecSpecificCapabilities::SupportedFrameDurations(SupportedFrameDurations::default()), // 10ms only
            CodecSpecificCapabilities::SupportedOctetsPerCodecFrame(OctetsPerCodecFrame::new(26, 155)),
        ],
        metadata: alloc::vec![],
    }
}

/// A device's coordinated-set membership (see `csis`) - the SIRK shared by every member of the
/// set, this member's rank, and the set's total size.
#[derive(Clone, Copy)]
pub struct CoordinatedSetOptions {
    pub sirk: Sirk,
    pub rank: u8,
    pub size: u8,
}

/// A device's call-control bearer identity (see `tbs`).
#[derive(Clone, Copy)]
pub struct CallControlOptions<'a> {
    pub provider_name: &'a str,
    pub technology: tbs::BearerTechnology,
}

/// A use-case-driven LE Audio device description. Start from one of the role constructors
/// ([`Scenario::headset`], [`Scenario::microphone`], [`Scenario::headset_with_mic`],
/// [`Scenario::auracast_sink`], [`Scenario::auracast_source`]), chain whichever `with_*`
/// capability toggles apply, then call [`Scenario::run`].
pub struct Scenario<'a> {
    name: &'a str,
    sample_rate: SamplingFrequency,
    role: DeviceRole,
    volume_control: bool,
    microphone_control: bool,
    coordinated_set: Option<CoordinatedSetOptions>,
    hearing_aid_presets: Option<HVec<PresetRecord, HAS_MAX_PRESETS>>,
    call_control: Option<CallControlOptions<'a>>,
    gaming: Option<GmapRole>,
}

impl<'a> Scenario<'a> {
    fn new(name: &'a str, role: DeviceRole) -> Self {
        Self {
            name,
            sample_rate: SamplingFrequency::Hz48000,
            role,
            volume_control: false,
            microphone_control: false,
            coordinated_set: None,
            hearing_aid_presets: None,
            call_control: None,
            gaming: None,
        }
    }

    /// A sink-only device (e.g. headphones, a speaker).
    pub fn headset(name: &'a str) -> Self {
        Self::new(name, DeviceRole::Headset)
    }
    /// A source-only device (e.g. a standalone mic). See the module doc comment's gap #1.
    pub fn microphone(name: &'a str) -> Self {
        Self::new(name, DeviceRole::Microphone)
    }
    /// A sink+source device (e.g. a phone headset). See the module doc comment's gap #1.
    pub fn headset_with_mic(name: &'a str) -> Self {
        Self::new(name, DeviceRole::HeadsetWithMic)
    }
    /// A broadcast receiver. Not implemented yet - see the module doc comment's gap #2.
    pub fn auracast_sink(name: &'a str) -> Self {
        Self::new(name, DeviceRole::AuracastSink)
    }
    /// A broadcast transmitter. Not implemented yet - see the module doc comment's gap #2.
    pub fn auracast_source(name: &'a str) -> Self {
        Self::new(name, DeviceRole::AuracastSource)
    }

    /// Sets the LC3 sampling frequency both PAC records advertise. Defaults to 48 kHz.
    pub fn sample_rate(mut self, sample_rate: SamplingFrequency) -> Self {
        self.sample_rate = sample_rate;
        self
    }

    /// Adds Volume Control Service support (initial volume 100/255, step size 8).
    pub fn with_volume_control(mut self) -> Self {
        self.volume_control = true;
        self
    }

    /// Adds Microphone Control Service support (starts unmuted). Independent of [`DeviceRole`] -
    /// this is a mute *control surface*, not the LE Audio Source ASE audio path itself (see the
    /// module doc comment's gap #1), so it's meaningful even on a plain `Headset` with its own
    /// non-LE-Audio microphone.
    pub fn with_microphone_control(mut self) -> Self {
        self.microphone_control = true;
        self
    }

    /// Adds Coordinated Set Identification Service support - this device is one member of a
    /// coordinated set (e.g. one earbud of a pair) identified by `options.sirk`.
    pub fn with_coordinated_set(mut self, options: CoordinatedSetOptions) -> Self {
        self.coordinated_set = Some(options);
        self
    }

    /// Adds Hearing Access Service support with the given initial preset list (capped at
    /// [`HAS_MAX_PRESETS`] presets).
    pub fn with_hearing_aid_presets(mut self, presets: HVec<PresetRecord, HAS_MAX_PRESETS>) -> Self {
        self.hearing_aid_presets = Some(presets);
        self
    }

    /// Adds (Generic) Telephone Bearer Service support (call control), identified by
    /// `options.provider_name`/`options.technology`.
    pub fn with_call_control(mut self, options: CallControlOptions<'a>) -> Self {
        self.call_control = Some(options);
        self
    }

    /// Adds Gaming Audio Service support, advertising the given GMAP role bitmask.
    pub fn with_gaming_role(mut self, role: GmapRole) -> Self {
        self.gaming = Some(role);
        self
    }

    /// Runs this scenario forever: advertises, accepts connections, and services them (see
    /// [`crate::le_audio::run_peripheral`], which this builds on). Returns
    /// [`ScenarioError::NotImplemented`] immediately, without touching `controller`, if `role` is
    /// `AuracastSink`/`AuracastSource`.
    ///
    /// `MAX_ASES` must be at least 1 for `Headset`/`Microphone`, at least 2 for `HeadsetWithMic`.
    /// `cis_manager` is caller-owned so the caller can concurrently drain
    /// [`CisManager::receive_pcm`] for decoded audio, exactly as in `run_peripheral`.
    pub async fn run<C, M, const MAX_ASES: usize, const CONNECTIONS_MAX: usize, const L2CAP_CHANNELS_MAX: usize>(
        &self,
        controller: C,
        address: Address,
        io_capabilities: IoCapabilities,
        bond_store: Option<&dyn BondStore>,
        cis_manager: &CisManager<M, MAX_ASES>,
    ) -> Result<Infallible, ScenarioError>
    where
        C: Controller
            + ControllerCmdAsync<LeAcceptCisRequest>
            + ControllerCmdSync<LeRejectCisRequest>
            + for<'x> ControllerCmdSync<LeSetupIsoDataPath<'x>>
            + ControllerCmdSync<LeRemoveIsoDataPath>
            + ControllerCmdSync<LeSetHostFeature>
            + ControllerCmdSync<LeReadLocalSupportedFeatures>,
        M: RawMutex,
    {
        validate_role(self.role)?;

        let mut resources: HostResources<C, DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> = HostResources::new();
        let stack = trouble_host::new(controller, &mut resources)
            .set_random_address(address)
            .set_io_capabilities(io_capabilities)
            .build();
        if let Some(store) = bond_store {
            if let Some(bond) = store.load() {
                let _ = stack.add_bond_information(bond);
            }
        }
        let runner = stack.runner();
        let peripheral = stack.peripheral();

        // Every GATT characteristic store buffer this scenario might need is declared up front,
        // before `builder` (the value that will end up borrowing whichever of them are actually
        // used) exists at all. That's required, not just tidy: `ServerBuilder`/`AttributeTable`
        // must not be dropped before the buffers they borrow are - and Rust drops locals in
        // reverse declaration order, so a buffer declared *after* `builder` would (per that rule)
        // be dropped *before* it, which the borrow checker correctly rejects.
        let mut sink_pac_store = [0u8; 90];
        let mut source_pac_store = [0u8; 90];
        let mut sink_audio_locations_store = [0u8; 90];
        let mut source_audio_locations_store = [0u8; 90];
        let mut available_audio_contexts_store = [0u8; 90];
        let mut mics_store = [0u8; 1];
        let mut vcs_state_store = [0u8; 3];
        let mut vcs_cp_store = [0u8; 3];
        let mut vcs_flags_store = [0u8; 1];
        let mut sirk_store = [0u8; 17];
        let mut set_size_store = [0u8; 1];
        let mut lock_store = [0u8; 1];
        let mut rank_store = [0u8; 1];
        let mut has_features_store = [0u8; 1];
        let mut has_cp_store = [0u8; 2 + crate::has::MAX_PRESET_NAME_LEN];
        let mut has_active_store = [0u8; 1];
        let mut tbs_name_store = [0u8; 32];
        let mut tbs_tech_store = [0u8; 1];
        let mut tbs_signal_store = [0u8; 1];
        let mut tbs_call_state_store = [0u8; 3 * TBS_MAX_CALLS];
        let mut tbs_list_calls_store = [0u8; (3 + tbs::MAX_URI_LEN) * TBS_MAX_CALLS];
        let mut tbs_ccid_store = [0u8; 1];
        let mut tbs_flags_store = [0u8; 2];
        let mut tbs_cp_store = [0u8; 1 + tbs::MAX_URI_LEN];
        let mut gmas_role_store = [0u8; 1];
        let mut gmas_ugg_store = [0u8; 1];
        let mut gmas_ugt_store = [0u8; 1];
        let mut gmas_bgs_store = [0u8; 1];
        let mut gmas_bgr_store = [0u8; 1];

        let sink_pac = self.role.has_sink().then(|| PAC::new(&[pac_record(self.sample_rate)]));
        let source_pac = self.role.has_source().then(|| PAC::new(&[pac_record(self.sample_rate)]));
        // A single (mono) location, matching the single Sink/Source ASE each direction gets - see
        // the analogous comment in basic_audio_sink.rs for why stereo locations with only one ASE
        // per direction breaks Android's stream-config derivation.
        let sink_locations = self.role.has_sink().then_some(AudioLocation::FrontLeft);
        let source_locations = self.role.has_source().then_some(AudioLocation::FrontLeft);
        let contexts = audio_contexts(self.role);
        let ases: HVec<AseType, MAX_ASES> = ase_list(self.role);
        let appearance = self.role.appearance();

        let mut builder = ServerBuilder::<MAX_ASES, CONNECTIONS_MAX, M>::new(self.name.as_bytes(), &appearance)
            .add_pacs(
                sink_pac.as_ref().map(|pac| (pac, &mut sink_pac_store[..])),
                sink_locations.as_ref().map(|loc| (loc, &mut sink_audio_locations_store[..])),
                source_pac.as_ref().map(|pac| (pac, &mut source_pac_store[..])),
                source_locations.as_ref().map(|loc| (loc, &mut source_audio_locations_store[..])),
                &contexts,
                &contexts,
                &mut available_audio_contexts_store[..],
            )
            .add_ascs(ases)
            .add_cis_manager(cis_manager);

        if self.microphone_control {
            builder = builder.add_mics(mics::Mute::NotMuted, &mut mics_store);
        }

        if self.volume_control {
            builder = builder.add_vcs(
                VolumeState { volume_setting: 100, mute: vcs::Mute::NotMuted, change_counter: 0 },
                VolumeFlags::empty(),
                8,
                &mut vcs_state_store,
                &mut vcs_cp_store,
                &mut vcs_flags_store,
            );
        }

        if let Some(options) = self.coordinated_set {
            builder = builder.add_csis(
                options.sirk,
                Some(options.size),
                crate::csis::Lock::Unlocked,
                Some(options.rank),
                &mut sirk_store,
                &mut set_size_store,
                &mut lock_store,
                &mut rank_store,
            );
        }

        if let Some(presets) = self.hearing_aid_presets.clone() {
            builder = builder.add_has(
                HearingAidFeatures::default(),
                presets,
                0,
                &mut has_features_store,
                &mut has_cp_store,
                &mut has_active_store,
            );
        }

        if let Some(options) = self.call_control {
            builder = builder.add_tbs(
                TbsInit {
                    bearer_provider_name: heapless::String::try_from(options.provider_name).unwrap_or_default(),
                    bearer_technology: options.technology,
                    content_control_id: 1,
                    status_flags: tbs::StatusFlags::empty(),
                },
                TbsStore {
                    bearer_provider_name: &mut tbs_name_store,
                    bearer_technology: &mut tbs_tech_store,
                    signal_strength: &mut tbs_signal_store,
                    call_state: &mut tbs_call_state_store,
                    bearer_list_current_calls: &mut tbs_list_calls_store,
                    content_control_id: &mut tbs_ccid_store,
                    status_flags: &mut tbs_flags_store,
                    call_control_point: &mut tbs_cp_store,
                },
            );
        }

        if let Some(role) = self.gaming {
            builder = builder.add_gmas(
                role,
                None,
                None,
                None,
                None,
                &mut gmas_role_store,
                &mut gmas_ugg_store,
                &mut gmas_ugt_store,
                &mut gmas_bgs_store,
                &mut gmas_bgr_store,
            );
        }

        let server = builder.build();

        // `run_event_loop` returns `!` - it never actually produces a value to wrap in `Ok`, so
        // this coerces directly to `Result<Infallible, ScenarioError>` rather than needing (and
        // triggering an "unreachable code" warning on) an explicit `Ok(...)`.
        le_audio::run_event_loop(&stack, runner, peripheral, cis_manager, &server, self.name.as_bytes(), bond_store).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ase_list_matches_role() {
        let sink: HVec<AseType, 2> = ase_list(DeviceRole::Headset);
        assert_eq!(sink.len(), 1);
        assert!(matches!(sink[0], AseType::Sink(_)));

        let source: HVec<AseType, 2> = ase_list(DeviceRole::Microphone);
        assert_eq!(source.len(), 1);
        assert!(matches!(source[0], AseType::Source(_)));

        let both: HVec<AseType, 2> = ase_list(DeviceRole::HeadsetWithMic);
        assert_eq!(both.len(), 2);
        assert!(matches!(both[0], AseType::Sink(_)));
        assert!(matches!(both[1], AseType::Source(_)));
    }

    #[test]
    fn ase_ids_do_not_collide_for_headset_with_mic() {
        let both: HVec<AseType, 2> = ase_list(DeviceRole::HeadsetWithMic);
        let ids: alloc::vec::Vec<u8> = both
            .iter()
            .map(|a| match a {
                AseType::Sink(ase) | AseType::Source(ase) => ase.id(),
            })
            .collect();
        assert_eq!(ids, alloc::vec![0, 1]);
    }

    #[test]
    fn audio_contexts_reflect_role_directions() {
        let headset = audio_contexts(DeviceRole::Headset);
        assert!(headset.sink_contexts.contains(ContextType::Media));
        assert!(headset.source_contexts.is_empty());

        let mic = audio_contexts(DeviceRole::Microphone);
        assert!(mic.sink_contexts.is_empty());
        assert!(mic.source_contexts.contains(ContextType::Conversational));

        let both = audio_contexts(DeviceRole::HeadsetWithMic);
        assert!(both.sink_contexts.contains(ContextType::Media));
        assert!(both.source_contexts.contains(ContextType::Conversational));
    }

    #[test]
    fn auracast_roles_are_rejected_before_touching_the_radio() {
        assert_eq!(
            validate_role(DeviceRole::AuracastSink),
            Err(ScenarioError::NotImplemented(
                "Auracast (broadcast) isn't implemented yet - no BIG/BIS or periodic advertising support exists in trouble-audio or trouble-host"
            ))
        );
        assert_eq!(
            validate_role(DeviceRole::AuracastSource),
            Err(ScenarioError::NotImplemented(
                "Auracast (broadcast) isn't implemented yet - no BIG/BIS or periodic advertising support exists in trouble-audio or trouble-host"
            ))
        );
    }

    #[test]
    fn implemented_roles_pass_validation() {
        for role in [DeviceRole::Headset, DeviceRole::Microphone, DeviceRole::HeadsetWithMic] {
            assert_eq!(validate_role(role), Ok(()));
        }
    }

    #[test]
    fn scenario_builder_chains() {
        let sirk = Sirk::plaintext([0x11; 16]);
        let scenario = Scenario::headset_with_mic("Earbud")
            .sample_rate(SamplingFrequency::Hz32000)
            .with_volume_control()
            .with_microphone_control()
            .with_coordinated_set(CoordinatedSetOptions { sirk, rank: 1, size: 2 });
        assert_eq!(scenario.name, "Earbud");
        assert_eq!(scenario.sample_rate, SamplingFrequency::Hz32000);
        assert!(scenario.volume_control);
        assert!(scenario.microphone_control);
        assert!(scenario.coordinated_set.is_some());
    }
}
