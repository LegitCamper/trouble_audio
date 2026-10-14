#![no_std]

extern crate alloc;

pub mod bond_store;
pub mod csprng;

use heapless::Vec as HVec;
use trouble_audio::big_sink::BroadcastSinkConfig;
use trouble_audio::presets::{AudioPreset, BROADCAST_48_2_1, broadcast_preset};

/// Broadcast preset name, overridable at build time so one scenario per preset can be flashed
/// without editing source: `TROUBLE_AUDIO_PRESET=48_1_gs cargo build --bin broadcast_source`.
/// Number of unicast audio channels (CIS) to set up: `1` for mono, `2` (the default) for a
/// stereo pair. Set `TROUBLE_AUDIO_CHANNELS` at build time.
///
/// Mono is useful both for genuinely mono peers and because the controller may refuse two CIS at
/// short ISO intervals while accepting one.
pub fn audio_channels() -> usize {
    match option_env!("TROUBLE_AUDIO_CHANNELS") {
        None => 2,
        Some("1") => 1,
        Some("2") => 2,
        Some(_) => panic!("TROUBLE_AUDIO_CHANNELS must be 1 or 2"),
    }
}

pub fn audio_preset() -> AudioPreset {
    match option_env!("TROUBLE_AUDIO_PRESET") {
        Some(name) => match broadcast_preset(name) {
            Some(preset) => preset,
            // A typo must fail loudly rather than silently testing the default preset.
            None => panic!("unknown TROUBLE_AUDIO_PRESET"),
        },
        None => BROADCAST_48_2_1,
    }
}

/// Optional 16-octet Broadcast_Code, as up to 16 ASCII bytes:
/// `TROUBLE_AUDIO_BROADCAST_CODE=Auracast cargo build --bin broadcast_source`.
fn broadcast_code() -> Option<[u8; 16]> {
    let text = option_env!("TROUBLE_AUDIO_BROADCAST_CODE")?;
    let bytes = text.as_bytes();
    if bytes.is_empty() || bytes.len() > 16 {
        panic!("TROUBLE_AUDIO_BROADCAST_CODE must be 1-16 bytes");
    }
    let mut code = [0u8; 16];
    code[..bytes.len()].copy_from_slice(bytes);
    Some(code)
}

/// Runs the shared Auracast source loop on nRF SDC. The caller's SDC builder must enable
/// `support_adv()`, `support_bis_source()`, and sufficient ISO TX buffers.
pub async fn run_auracast_source(controller: nrf_sdc::SoftdeviceController<'_>) -> ! {
    trouble_audio_example_apps::auracast_source::run(
        controller,
        [0x51, 0x52, 0x53, 0x54, 0x55, 0xc0],
        [0x12, 0x34, 0x56],
        audio_preset(),
        broadcast_code(),
    )
    .await
}

/// Runs the shared decoded-PCM Auracast sink loop on nRF SDC. The caller's SDC builder must enable
/// `support_scan()`, `support_bis_sink()`, and sufficient ISO RX buffers.
pub async fn run_auracast_sink(controller: nrf_sdc::SoftdeviceController<'_>) -> ! {
    let mut bis = HVec::new();
    bis.push(1).unwrap();
    bis.push(2).unwrap();
    trouble_audio_example_apps::auracast_sink::run(
        controller,
        [0x61, 0x62, 0x63, 0x64, 0x65, 0xc0],
        BroadcastSinkConfig {
            big_handle: 0,
            bis,
            broadcast_code: broadcast_code(),
            periodic_skip: 0,
            periodic_sync_timeout_10ms: 1_000,
            max_subevents: 0,
            big_sync_timeout_10ms: 1_000,
        },
    )
    .await
}
