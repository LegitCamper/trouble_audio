#![no_std]

extern crate alloc;

pub mod bond_store;

use heapless::Vec as HVec;
use trouble_audio::big_sink::BroadcastSinkConfig;

/// Runs the shared Auracast source loop on nRF SDC. The caller's SDC builder must enable
/// `support_adv()`, `support_bis_source()`, and sufficient ISO TX buffers.
pub async fn run_auracast_source(controller: nrf_sdc::SoftdeviceController<'_>) -> ! {
    trouble_audio_example_apps::auracast_source::run(
        controller,
        [0x51, 0x52, 0x53, 0x54, 0x55, 0xc0],
        [0x12, 0x34, 0x56],
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
            broadcast_code: None,
            periodic_skip: 0,
            periodic_sync_timeout_10ms: 1_000,
            max_subevents: 0,
            big_sync_timeout_10ms: 1_000,
        },
    )
    .await
}
