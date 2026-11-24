// SPDX-License-Identifier: GPL-3.0-or-later
//
// The `sync` channel and the precedence rules: the most recent user change wins,
// whatever order CoolerControl re-sends its saved settings in. RPCs are called
// directly on the service with paused tokio time, a mock bus and a fake suspend
// clock.

mod common;

use cc_asus_aura_argb::autosave::{AutoSave, StateStore};
use cc_asus_aura_argb::device_service::v1::device_service_server::DeviceService;
use cc_asus_aura_argb::device_service::v1::{
    InitializeDeviceRequest, ListDevicesRequest, ResetChannelRequest,
};
use cc_asus_aura_argb::hid::mock::MockBus;
use cc_asus_aura_argb::models::v1::channel_info::Options;
use cc_asus_aura_argb::precedence::{self, PrecedenceStore};
use cc_asus_aura_argb::service::AuraService;
use common::{DEVICE_ID, device, key, lighting, saves};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::sleep;
use tonic::Request;

type Color = (u32, u32, u32);
const RED: Color = (255, 0, 0);
const GREEN: Color = (0, 255, 0);
const BLUE: Color = (0, 0, 255);
const WHITE: Color = (255, 255, 255);

/// (mode byte, colour) per effect channel, from the reports written so far.
fn shown(bus: &MockBus) -> BTreeMap<u8, (u8, [u8; 3])> {
    let mut mode: BTreeMap<u8, u8> = BTreeMap::new();
    let mut color: BTreeMap<u8, [u8; 3]> = BTreeMap::new();
    for w in bus.writes() {
        match w[1] {
            0x35 => {
                mode.insert(w[2], w[5]);
            }
            0x36 => {
                let mask = u16::from_be_bytes([w[2], w[3]]);
                for slot in 0..16u8 {
                    if mask & (1 << slot) != 0 {
                        let i = 5 + 3 * usize::from(slot);
                        color.insert(slot, [w[i], w[i + 1], w[i + 2]]);
                    }
                }
            }
            _ => {}
        }
    }
    // On the tested board effect channel n uses colour slot n.
    mode.into_iter()
        .map(|(ch, m)| (ch, (m, color.get(&ch).copied().unwrap_or_default())))
        .collect()
}

const RAINBOW: (u8, [u8; 3]) = (0x05, [0, 0, 0]);
fn stat(c: Color) -> (u8, [u8; 3]) {
    (0x01, [c.0 as u8, c.1 as u8, c.2 as u8])
}

struct Rig {
    bus: MockBus,
    svc: AuraService,
    slept_ms: Arc<AtomicU64>,
    auto: Option<Arc<AutoSave>>,
}

impl Rig {
    fn new(state: Option<&Path>) -> Self {
        Self::build(state, false)
    }

    fn with_auto_save(state: &Path) -> Self {
        Self::build(Some(state), true)
    }

    fn build(state: Option<&Path>, auto_save: bool) -> Self {
        let bus = MockBus::new();
        let slept_ms = Arc::new(AtomicU64::new(0));
        let clock = Arc::clone(&slept_ms);
        let auto = if auto_save {
            Some(AutoSave::new(
                StateStore::new(state.unwrap()),
                Duration::from_secs(30),
            ))
        } else {
            None
        };
        let svc = AuraService::new(vec![device(&bus)])
            .with_precedence_dir(state)
            .with_auto_save(auto.clone())
            .with_sleep_clock(Arc::new(move || {
                Some(Duration::from_millis(clock.load(Ordering::SeqCst)))
            }));
        Rig {
            bus,
            svc,
            slept_ms,
            auto,
        }
    }

    async fn set(&self, channel: &str, mode: &str, color: Option<Color>) {
        let colors: Vec<Color> = color.into_iter().collect();
        self.svc
            .lighting(Request::new(lighting(channel, mode, &colors)))
            .await
            .unwrap();
    }

    async fn reset(&self, channel: &str) {
        self.svc
            .reset_channel(Request::new(ResetChannelRequest {
                device_id: DEVICE_ID.to_string(),
                channel_id: channel.to_string(),
            }))
            .await
            .unwrap();
    }

    async fn initialize(&self) {
        self.svc
            .initialize_device(Request::new(InitializeDeviceRequest {
                device_id: DEVICE_ID.to_string(),
            }))
            .await
            .unwrap();
    }

    fn suspend(&self, secs: u64) {
        self.slept_ms.fetch_add(secs * 1000, Ordering::SeqCst);
    }

    fn shown(&self) -> BTreeMap<u8, (u8, [u8; 3])> {
        shown(&self.bus)
    }

    async fn attempts(&self, n: u64) {
        let mut rx = self.auto.as_ref().unwrap().completed();
        tokio::time::timeout(Duration::from_secs(3600), rx.wait_for(|c| *c >= n))
            .await
            .unwrap()
            .unwrap();
    }
}

/// One saved setting as CoolerControl keeps it: (channel, mode, colour).
type Saved = (&'static str, &'static str, Option<Color>);

/// A fresh service over `state` (a service or daemon restart), then CoolerControl's
/// start sequence: InitializeDevice, then every saved setting in `order`.
async fn restart_and_storm(state: &Path, order: &[Saved]) -> BTreeMap<u8, (u8, [u8; 3])> {
    let rig = Rig::new(Some(state));
    rig.initialize().await;
    for (ch, mode, c) in order {
        rig.set(ch, mode, *c).await;
    }
    rig.shown()
}

/// Runs the user's history, then checks that both arrival orders of the storm
/// give `want`, and that the storms did not change the records.
async fn check_storm_orders(history: &[Saved], saved: &[Saved], want: [(u8, [u8; 3]); 4]) {
    let dir = tempfile::tempdir().unwrap();
    {
        let rig = Rig::new(Some(dir.path()));
        for (ch, mode, c) in history {
            rig.set(ch, mode, *c).await;
        }
        let expect: BTreeMap<u8, (u8, [u8; 3])> = (0u8..4).zip(want).collect();
        assert_eq!(rig.shown(), expect, "live result");
    }
    let path = precedence::path_for(dir.path(), &key());
    let records = std::fs::read_to_string(&path).unwrap();
    let expect: BTreeMap<u8, (u8, [u8; 3])> = (0u8..4).zip(want).collect();
    let forward = restart_and_storm(dir.path(), saved).await;
    let mut rev = saved.to_vec();
    rev.reverse();
    let backward = restart_and_storm(dir.path(), &rev).await;
    assert_eq!(forward, expect, "storm in saved order");
    assert_eq!(backward, expect, "storm in reverse order");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        records,
        "re-applies must not change the records"
    );
}

#[tokio::test(start_paused = true)]
async fn list_devices_offers_sync() {
    let rig = Rig::new(None);
    let d = rig
        .svc
        .list_devices(Request::new(ListDevicesRequest {}))
        .await
        .unwrap()
        .into_inner()
        .devices;
    let ch = &d[0].info.as_ref().unwrap().channels;
    assert_eq!(ch.len(), 5);
    assert_eq!(ch["sync"].label.as_deref(), Some("All channels"));
    let Some(Options::LightingModes(m)) = &ch["sync"].options else {
        panic!()
    };
    assert_eq!(m.lighting_mode.len(), 18);
}

#[tokio::test(start_paused = true)]
async fn sync_newer_than_a_channel() {
    // argb2 red, then sync rainbow: rainbow everywhere.
    check_storm_orders(
        &[("argb2", "static", Some(RED)), ("sync", "rainbow", None)],
        &[("sync", "rainbow", None), ("argb2", "static", Some(RED))],
        [RAINBOW; 4],
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn channel_newer_than_sync() {
    check_storm_orders(
        &[("sync", "rainbow", None), ("argb2", "static", Some(RED))],
        &[("sync", "rainbow", None), ("argb2", "static", Some(RED))],
        [RAINBOW, RAINBOW, stat(RED), RAINBOW],
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn mixed_precedence() {
    // argb3 older than sync, argb1 newer, mainboard never set on its own.
    check_storm_orders(
        &[
            ("argb3", "static", Some(BLUE)),
            ("sync", "rainbow", None),
            ("argb1", "breathing", Some(GREEN)),
        ],
        &[
            ("argb1", "breathing", Some(GREEN)),
            ("sync", "rainbow", None),
            ("argb3", "static", Some(BLUE)),
        ],
        [RAINBOW, (0x02, [0, 255, 0]), RAINBOW, RAINBOW],
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn clicking_sync_again_wins_everywhere() {
    // sync rainbow, argb2 red, sync rainbow again (same setting, outside any window).
    check_storm_orders(
        &[
            ("sync", "rainbow", None),
            ("argb2", "static", Some(RED)),
            ("sync", "rainbow", None),
        ],
        &[("argb2", "static", Some(RED)), ("sync", "rainbow", None)],
        [RAINBOW; 4],
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn same_setting_after_the_window_is_a_change() {
    let dir = tempfile::tempdir().unwrap();
    let rig = Rig::new(Some(dir.path()));
    rig.set("sync", "rainbow", None).await;
    rig.set("argb2", "static", Some(RED)).await;
    rig.initialize().await;
    rig.set("sync", "rainbow", None).await;
    assert_eq!(rig.shown()[&2], stat(RED), "inside the window: a re-apply");
    sleep(Duration::from_secs(21)).await;
    rig.set("sync", "rainbow", None).await;
    assert_eq!(rig.shown()[&2], RAINBOW, "after the window: a click");
}

#[tokio::test(start_paused = true)]
async fn differing_request_inside_the_window_is_a_change() {
    let dir = tempfile::tempdir().unwrap();
    let rig = Rig::new(Some(dir.path()));
    rig.set("argb2", "static", Some(RED)).await;
    rig.set("sync", "rainbow", None).await;
    rig.initialize().await;
    rig.set("argb2", "static", Some(WHITE)).await;
    assert_eq!(rig.shown()[&2], stat(WHITE));
    // And it stays the newest after a restart storm.
    let got = restart_and_storm(
        dir.path(),
        &[("argb2", "static", Some(WHITE)), ("sync", "rainbow", None)],
    )
    .await;
    assert_eq!(got[&2], stat(WHITE));
    assert_eq!(got[&0], RAINBOW);
}

#[tokio::test(start_paused = true)]
async fn user_changes_always_write_reapplies_may_skip() {
    let dir = tempfile::tempdir().unwrap();
    let rig = Rig::new(Some(dir.path()));
    rig.set("sync", "rainbow", None).await;
    assert_eq!(rig.bus.writes().len(), 8, "four channels, two reports each");
    // The user clicks the same again (outside any window): rewritten, because the
    // controller may not show it any more (another driver, a reset, ...).
    rig.set("sync", "rainbow", None).await;
    assert_eq!(rig.bus.writes().len(), 16);
    rig.set("argb2", "rainbow", None).await;
    assert_eq!(rig.bus.writes().len(), 18);

    // Daemon restart: InitializeDevice, then the storm. The first re-apply writes
    // everything; re-applies after it skip what the controller is known to show.
    rig.initialize().await;
    rig.set("sync", "rainbow", None).await;
    assert_eq!(rig.bus.writes().len(), 26);
    rig.set("argb2", "rainbow", None).await;
    rig.set("sync", "rainbow", None).await;
    assert_eq!(
        rig.bus.writes().len(),
        26,
        "re-applies of what is shown are skipped"
    );

    // Resume: no InitializeDevice in CoolerControl 5.1.2, but the suspend clock moved.
    sleep(Duration::from_secs(60)).await;
    rig.suspend(600);
    rig.set("sync", "rainbow", None).await;
    assert_eq!(
        rig.bus.writes().len(),
        34,
        "first re-apply after resume writes"
    );
    // ... and opened the window: a different setting is a change (written), the
    // storm's other re-sends are re-applies (skipped where shown).
    rig.set("argb2", "static", Some(RED)).await;
    assert_eq!(rig.bus.writes().len(), 36);
    rig.set("sync", "rainbow", None).await;
    assert_eq!(rig.bus.writes().len(), 36);
    assert_eq!(rig.shown()[&2], stat(RED));
}

#[tokio::test(start_paused = true)]
async fn repeated_click_outside_the_window_rewrites_but_does_not_save() {
    let dir = tempfile::tempdir().unwrap();
    let rig = Rig::with_auto_save(dir.path());
    rig.set("sync", "rainbow", None).await;
    sleep(Duration::from_secs(31)).await;
    rig.attempts(1).await;
    assert_eq!(saves(&rig.bus), 1);
    let n = rig.bus.writes().len();
    rig.set("sync", "rainbow", None).await;
    assert_eq!(rig.bus.writes().len(), n + 8, "reports written again");
    sleep(Duration::from_secs(31)).await;
    rig.attempts(2).await;
    assert_eq!(saves(&rig.bus), 1, "no second save packet");
    // A reset that changes what a channel shows is a change too: forced.
    rig.set("argb2", "static", Some(RED)).await;
    let n = rig.bus.writes().len();
    rig.reset("argb2").await;
    assert_eq!(rig.bus.writes().len(), n + 2);
    assert_eq!(rig.shown()[&2], RAINBOW);
}

#[tokio::test(start_paused = true)]
async fn reset_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let rig = Rig::new(Some(dir.path()));
    rig.set("argb1", "static", Some(GREEN)).await;
    rig.set("sync", "rainbow", None).await;
    rig.set("argb2", "static", Some(RED)).await;
    // Resetting a channel lets sync show through.
    rig.reset("argb2").await;
    assert_eq!(rig.shown()[&2], RAINBOW);
    // Resetting sync shows each channel's own setting; channels without one keep
    // what they show.
    rig.set("argb2", "static", Some(RED)).await;
    rig.reset("sync").await;
    let s = rig.shown();
    assert_eq!(s[&1], stat(GREEN));
    assert_eq!(s[&2], stat(RED));
    assert_eq!(s[&0], RAINBOW);
    // A reset of something with no record writes nothing.
    let n = rig.bus.writes().len();
    rig.reset("sync").await;
    rig.reset("mainboard").await;
    assert_eq!(rig.bus.writes().len(), n);
    let err = rig
        .svc
        .reset_channel(Request::new(ResetChannelRequest {
            device_id: DEVICE_ID.to_string(),
            channel_id: "led9".to_string(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test(start_paused = true)]
async fn sync_causes_no_extra_saves() {
    let dir = tempfile::tempdir().unwrap();
    {
        let rig = Rig::with_auto_save(dir.path());
        rig.set("sync", "rainbow", None).await;
        rig.set("argb2", "static", Some(RED)).await;
        sleep(Duration::from_secs(31)).await;
        rig.attempts(2).await;
        assert_eq!(saves(&rig.bus), 1, "one save for the whole burst");
    }
    // Restart and storm in both orders: writes happen, but nothing new to save.
    for order in [
        [("sync", "rainbow", None), ("argb2", "static", Some(RED))],
        [("argb2", "static", Some(RED)), ("sync", "rainbow", None)],
    ] {
        let rig = Rig::with_auto_save(dir.path());
        rig.initialize().await;
        for (ch, mode, c) in order {
            rig.set(ch, mode, c).await;
        }
        // In the first order the sync re-apply already writes argb2 (it is newer),
        // so argb2's own re-send writes nothing: one or two save attempts.
        sleep(Duration::from_secs(31)).await;
        rig.attempts(1).await;
        sleep(Duration::from_secs(60)).await;
        assert!(!rig.bus.writes().is_empty());
        assert_eq!(saves(&rig.bus), 0, "{order:?}");
        let s = rig.shown();
        assert_eq!((s[&0], s[&2]), (RAINBOW, stat(RED)));
    }
}

#[tokio::test(start_paused = true)]
async fn corrupt_or_missing_precedence_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = precedence::path_for(dir.path(), &key());
    std::fs::write(&path, "garbage\n").unwrap();
    let rig = Rig::new(Some(dir.path()));
    rig.set("sync", "rainbow", None).await;
    assert_eq!(rig.shown()[&3], RAINBOW);
    // Rewritten as a valid record.
    let records = PrecedenceStore::in_dir(dir.path(), &key()).load();
    assert_eq!(records.get("sync").unwrap().seq, 1);
    // Removed while running: the next request starts afresh, never fails.
    std::fs::remove_file(&path).unwrap();
    rig.set("argb1", "static", Some(BLUE)).await;
    assert_eq!(rig.shown()[&1], stat(BLUE));
    assert!(path.exists());
}

#[tokio::test(start_paused = true)]
async fn without_a_state_directory_precedence_lives_in_memory() {
    let rig = Rig::new(None);
    rig.set("sync", "rainbow", None).await;
    rig.set("argb2", "static", Some(RED)).await;
    // A daemon restart while the service keeps running still resolves correctly.
    rig.initialize().await;
    rig.set("argb2", "static", Some(RED)).await;
    rig.set("sync", "rainbow", None).await;
    assert_eq!(rig.shown()[&2], stat(RED));
    assert_eq!(rig.shown()[&0], RAINBOW);
}
