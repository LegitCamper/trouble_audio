//! Minimal Auracast sink application: scans for the first broadcast announcement, synchronizes to
//! its periodic BASE and BIG, then continuously drains decoded PCM frames.

use bt_hci::cmd::le::{
    LeAddDeviceToFilterAcceptList, LeBigCreateSync, LeClearFilterAcceptList,
    LePeriodicAdvCreateSync, LeRemoveIsoDataPath, LeSetExtScanEnable, LeSetExtScanParams,
    LeSetupIsoDataPath,
};
use bt_hci::controller::{ControllerCmdAsync, ControllerCmdSync};
use embassy_futures::select::select3;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use trouble_audio::big_sink::{BigSink, BroadcastSinkConfig, drive_big_sink, start_broadcast_sync};
use trouble_host::prelude::*;

#[cfg(feature = "defmt")]
use defmt::{info, warn};

/// How often synchronized BIS delivery counters are reported.
const STATS_INTERVAL_SECS: u64 = 10;

/// Scans for and plays the first compatible broadcast forever.
pub async fn run<C>(controller: C, random_address: [u8; 6], config: BroadcastSinkConfig) -> !
where
    C: Controller
        + ControllerCmdSync<LeSetExtScanEnable>
        + ControllerCmdSync<LeSetExtScanParams>
        + ControllerCmdSync<LeClearFilterAcceptList>
        + ControllerCmdSync<LeAddDeviceToFilterAcceptList>
        + ControllerCmdAsync<LePeriodicAdvCreateSync>
        + for<'a> ControllerCmdAsync<LeBigCreateSync<'a>>
        + for<'a> ControllerCmdSync<LeSetupIsoDataPath<'a>>
        + ControllerCmdSync<LeRemoveIsoDataPath>,
{
    let mut resources: HostResources<DefaultPacketPool, 1, 1> = HostResources::new();
    let stack = trouble_host::new(controller, &mut resources)
        .set_random_address(Address::random(random_address))
        .build();
    let mut runner = stack.runner();
    let mut central = stack.central();
    let sink = BigSink::<NoopRawMutex>::new();

    select3(
        async {
            loop {
                if runner.run_with_handler(&sink).await.is_err() {
                    #[cfg(feature = "defmt")]
                    warn!("[auracast_sink] host runner stopped");
                }
            }
        },
        drive_big_sink(&stack, &sink),
        async {
            let mut scanner = Scanner::new(&mut central);
            let Ok(scan) = scanner.scan_ext(&ScanConfig::default()).await else {
                #[cfg(feature = "defmt")]
                warn!("[auracast_sink] extended scan failed");
                core::future::pending::<()>().await;
                unreachable!();
            };
            let source = sink.next_broadcast().await;
            #[cfg(feature = "defmt")]
            info!("[auracast_sink] broadcast discovered");
            let bis_indices = config.bis.clone();
            if start_broadcast_sync(&stack, &sink, source, config)
                .await
                .is_err()
            {
                #[cfg(feature = "defmt")]
                warn!("[auracast_sink] periodic sync command failed");
                core::future::pending::<()>().await;
            }
            if !sink.wait_periodic_sync().await {
                #[cfg(feature = "defmt")]
                warn!("[auracast_sink] periodic sync failed");
                core::future::pending::<()>().await;
            }
            scan.stop().await;
            let mut next_report = embassy_time::Instant::now()
                + embassy_time::Duration::from_secs(STATS_INTERVAL_SECS);
            loop {
                let frame = sink.receive_pcm().await;
                let _ = frame;
                #[cfg(feature = "defmt")]
                info!("[auracast_sink] decoded BIS {}", frame.bis_index);
                // Markers alone cannot distinguish a clean stream from one dropping a third of its
                // SDUs, so the harness asserts on these counters instead.
                if embassy_time::Instant::now() >= next_report {
                    next_report = embassy_time::Instant::now()
                        + embassy_time::Duration::from_secs(STATS_INTERVAL_SECS);
                    for bis_index in bis_indices.iter() {
                        let Some(_stats) = sink.bis_stats(*bis_index) else {
                            continue;
                        };
                        #[cfg(feature = "defmt")]
                        info!(
                            "[auracast_sink] stats bis={} received={} lost={} missing={} dup={} reordered={} burst={} loss_permille={}",
                            bis_index,
                            _stats.received,
                            _stats.flagged_lost,
                            _stats.missing,
                            _stats.duplicates,
                            _stats.reordered,
                            _stats.max_burst_missing,
                            _stats.loss_per_mille()
                        );
                    }
                }
            }
        },
    )
    .await;
    unreachable!("Auracast sink tasks run forever")
}
