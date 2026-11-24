// SPDX-License-Identifier: GPL-3.0-or-later
//
// Auto-save behaviour of the service, with a mock bus and paused tokio time.
// The RPCs are called directly on the service (no socket), so virtual time can be
// advanced freely.

mod common;

use cc_asus_aura_argb::autosave::{self, AutoSave, StateStore};
use cc_asus_aura_argb::device_service::v1::device_service_server::DeviceService;
use cc_asus_aura_argb::device_service::v1::{
    HealthRequest, InitializeDeviceRequest, ListDevicesRequest, ResetChannelRequest,
    ShutdownRequest, StatusRequest,
};
use cc_asus_aura_argb::hid::mock::MockBus;
use cc_asus_aura_argb::service::AuraService;
use common::{DEVICE_ID, device, key, lighting, saves};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tonic::Request;

const DELAY: Duration = Duration::from_secs(30);
const AFTER: Duration = Duration::from_secs(31);

struct Rig {
    bus: MockBus,
    svc: AuraService,
    auto: Option<Arc<AutoSave>>,
    store: Option<StateStore>,
}

impl Rig {
    fn new(state_dir: Option<&Path>) -> Self {
        let bus = MockBus::new();
        let store = state_dir.map(StateStore::new);
        let auto = store.clone().map(|s| AutoSave::new(s, DELAY));
        let svc = AuraService::new(vec![device(&bus)]).with_auto_save(auto.clone());
        Rig {
            bus,
            svc,
            auto,
            store,
        }
    }

    async fn apply(&self, channel: &str, mode: &str, color: Option<(u32, u32, u32)>) {
        self.try_apply(channel, mode, color).await.unwrap();
    }

    async fn try_apply(
        &self,
        channel: &str,
        mode: &str,
        color: Option<(u32, u32, u32)>,
    ) -> Result<(), tonic::Status> {
        let colors: Vec<(u32, u32, u32)> = color.into_iter().collect();
        self.svc
            .lighting(Request::new(lighting(channel, mode, &colors)))
            .await
            .map(|_| ())
    }

    fn saves(&self) -> usize {
        saves(&self.bus)
    }

    /// Waits until `n` save attempts (any outcome) have finished.
    async fn attempts(&self, n: u64) {
        let mut rx = self.auto.as_ref().unwrap().completed();
        tokio::time::timeout(Duration::from_secs(3600), rx.wait_for(|c| *c >= n))
            .await
            .expect("save attempts did not finish")
            .unwrap();
    }

    fn attempts_now(&self) -> u64 {
        *self.auto.as_ref().unwrap().completed().borrow()
    }

    fn record(&self) -> Option<Vec<String>> {
        self.store.as_ref().unwrap().load(&key())
    }
}

#[tokio::test(start_paused = true)]
async fn one_apply_saves_once_after_the_delay() {
    let dir = tempfile::tempdir().unwrap();
    let rig = Rig::new(Some(dir.path()));
    rig.apply("argb2", "rainbow", None).await;
    sleep(DELAY - Duration::from_secs(1)).await;
    assert_eq!(rig.saves(), 0, "nothing before the delay");
    assert_eq!(rig.attempts_now(), 0);
    sleep(Duration::from_secs(2)).await;
    rig.attempts(1).await;
    assert_eq!(rig.saves(), 1);
    assert_eq!(
        rig.record().unwrap()[2],
        "argb2 effect=05 channel=02 slots=2+1 color=000000"
    );
    // Nothing more happens while nothing changes.
    let ops = rig.bus.ops().len();
    sleep(Duration::from_secs(3600)).await;
    assert_eq!(rig.bus.ops().len(), ops);
}

#[tokio::test(start_paused = true)]
async fn changes_on_several_channels_cost_one_save() {
    let dir = tempfile::tempdir().unwrap();
    let rig = Rig::new(Some(dir.path()));
    // CoolerControl at startup: every channel, one after another.
    rig.apply("mainboard", "static", Some((255, 255, 255)))
        .await;
    rig.apply("argb1", "spectrum_cycle", None).await;
    sleep(Duration::from_secs(10)).await;
    // A rejected request changes nothing.
    assert!(rig.try_apply("argb2", "static", None).await.is_err());
    rig.apply("argb2", "static", Some((255, 0, 0))).await;
    sleep(Duration::from_secs(25)).await;
    assert_eq!(rig.saves(), 0, "the timer restarts on every apply");
    rig.apply("argb3", "chase", Some((0, 0, 255))).await;
    sleep(AFTER).await;
    rig.attempts(4).await;
    assert_eq!(rig.saves(), 1);
    assert_eq!(
        rig.record().unwrap(),
        vec![
            "mainboard effect=01 channel=00 slots=0+1 color=ffffff",
            "argb1 effect=04 channel=01 slots=1+1 color=000000",
            "argb2 effect=01 channel=02 slots=2+1 color=ff0000",
            "argb3 effect=09 channel=03 slots=3+1 color=0000ff",
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn reapplying_the_saved_settings_costs_nothing_even_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let rig = Rig::new(Some(dir.path()));
        rig.apply("argb2", "breathing", Some((1, 2, 3))).await;
        sleep(AFTER).await;
        rig.attempts(1).await;
        assert_eq!(rig.saves(), 1);
    }
    // A fresh service over the same state directory; CoolerControl re-sends.
    let rig = Rig::new(Some(dir.path()));
    rig.apply("argb2", "breathing", Some((1, 2, 3))).await;
    sleep(AFTER).await;
    rig.attempts(1).await;
    assert_eq!(rig.saves(), 0, "written, but already saved");
    // The user clicks the same setting again (outside any re-apply window): the
    // reports are written again, a save attempt runs, but the change-only
    // comparison writes no save packet.
    rig.apply("argb2", "breathing", Some((1, 2, 3))).await;
    assert_eq!(
        rig.bus.writes().len(),
        4,
        "a repeated click always rewrites"
    );
    sleep(AFTER).await;
    rig.attempts(2).await;
    assert_eq!(rig.saves(), 0);
    assert_eq!(rig.bus.writes().len(), 4);
}

#[tokio::test(start_paused = true)]
async fn equivalent_requests_do_not_save() {
    let dir = tempfile::tempdir().unwrap();
    let rig = Rig::new(Some(dir.path()));
    rig.apply("argb1", "rainbow", None).await;
    sleep(AFTER).await;
    rig.attempts(1).await;
    assert_eq!(rig.saves(), 1);
    // A colour sent with a colour-less effect is ignored: the same setting. As a
    // user click it is written again, but it is not saved again.
    let n = rig.bus.writes().len();
    rig.apply("argb1", "rainbow", Some((9, 9, 9))).await;
    assert_eq!(rig.bus.writes().len(), n + 2);
    sleep(AFTER).await;
    rig.attempts(2).await;
    assert_eq!(rig.saves(), 1);
}

#[tokio::test(start_paused = true)]
async fn failed_save_leaves_record_and_is_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let rig = Rig::new(Some(dir.path()));
    rig.apply("argb1", "static", Some((1, 1, 1))).await;
    sleep(AFTER).await;
    rig.attempts(1).await;
    let old = rig.record().unwrap();

    rig.apply("argb1", "static", Some((2, 2, 2))).await;
    rig.bus.fail_writes(Some(libc::EIO));
    sleep(AFTER).await;
    rig.attempts(2).await;
    assert_eq!(rig.record(), Some(old), "record unchanged");
    sleep(Duration::from_secs(24 * 3600)).await;
    assert_eq!(rig.attempts_now(), 2, "no retry loop");
    assert_eq!(rig.saves(), 1);

    // Clicking the same setting again is written and schedules a save, which now
    // succeeds.
    rig.bus.fail_writes(None);
    rig.apply("argb1", "static", Some((2, 2, 2))).await;
    sleep(AFTER).await;
    rig.attempts(3).await;
    assert_eq!(rig.saves(), 2);
    assert_eq!(
        rig.record().unwrap()[1],
        "argb1 effect=01 channel=01 slots=1+1 color=020202"
    );
}

#[tokio::test(start_paused = true)]
async fn failed_apply_blocks_saving_until_that_channel_is_set_again() {
    let dir = tempfile::tempdir().unwrap();
    let rig = Rig::new(Some(dir.path()));
    rig.apply("argb1", "static", Some((1, 1, 1))).await;
    rig.bus.fail_writes(Some(libc::EIO));
    assert_eq!(
        rig.try_apply("argb2", "static", Some((2, 2, 2)))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unavailable
    );
    rig.bus.fail_writes(None);
    sleep(AFTER).await;
    rig.attempts(1).await;
    assert_eq!(
        rig.saves(),
        0,
        "the failed apply cancelled the pending save"
    );
    // Another channel changes: argb2 is still unknown, so still no save.
    rig.apply("argb3", "static", Some((3, 3, 3))).await;
    sleep(AFTER).await;
    rig.attempts(2).await;
    assert_eq!(rig.saves(), 0);
    assert_eq!(rig.record(), None);
    // argb2 set successfully: saved.
    rig.apply("argb2", "static", Some((2, 2, 2))).await;
    sleep(AFTER).await;
    rig.attempts(3).await;
    assert_eq!(rig.saves(), 1);
}

#[tokio::test(start_paused = true)]
async fn corrupt_record_means_one_save() {
    let dir = tempfile::tempdir().unwrap();
    let store = StateStore::new(dir.path());
    std::fs::write(store.path_for(&key()), "not a record\n").unwrap();
    let rig = Rig::new(Some(dir.path()));
    rig.apply("argb2", "rainbow", None).await;
    sleep(AFTER).await;
    rig.attempts(1).await;
    assert_eq!(rig.saves(), 1);
    assert!(rig.record().is_some());
}

#[tokio::test(start_paused = true)]
async fn auto_save_off_never_saves() {
    let rig = Rig::new(None);
    rig.apply("argb1", "static", Some((1, 2, 3))).await;
    sleep(Duration::from_secs(24 * 3600)).await;
    rig.svc.flusher().flush(Duration::from_secs(5)).await;
    assert_eq!(rig.saves(), 0);
    let dir = tempfile::tempdir().unwrap();
    assert!(autosave::configure(true, Some(dir.path()), None, DELAY).is_none());
    assert!(autosave::configure(false, None, None, DELAY).is_none());
    let a = autosave::configure(
        false,
        Some(dir.path()),
        Some("/nonexistent".as_ref()),
        DELAY,
    )
    .unwrap();
    assert_eq!(a.store().dir(), dir.path());
    let list = format!("{}:/other", dir.path().display());
    let b = autosave::configure(false, None, Some(list.as_ref()), DELAY).unwrap();
    assert_eq!(b.store().dir(), dir.path());
    assert_eq!(b.delay(), DELAY);
}

#[tokio::test(start_paused = true)]
async fn shutdown_flushes_a_pending_save() {
    let dir = tempfile::tempdir().unwrap();
    let rig = Rig::new(Some(dir.path()));
    rig.apply("argb2", "flashing", Some((7, 7, 7))).await;
    let flusher = rig.svc.flusher();
    flusher.flush(Duration::from_secs(5)).await;
    assert_eq!(rig.saves(), 1);
    sleep(AFTER).await;
    rig.attempts(2).await;
    assert_eq!(rig.saves(), 1, "the timer finds nothing pending");
    flusher.flush(Duration::from_secs(5)).await;
    assert_eq!(rig.saves(), 1);
}

#[tokio::test(start_paused = true)]
async fn non_lighting_rpcs_never_touch_the_device() {
    let dir = tempfile::tempdir().unwrap();
    let rig = Rig::new(Some(dir.path()));
    let id = || DEVICE_ID.to_string();
    for _ in 0..3 {
        rig.svc
            .health(Request::new(HealthRequest {}))
            .await
            .unwrap();
        rig.svc
            .list_devices(Request::new(ListDevicesRequest {}))
            .await
            .unwrap();
        rig.svc
            .status(Request::new(StatusRequest { device_id: id() }))
            .await
            .unwrap();
        rig.svc
            .initialize_device(Request::new(InitializeDeviceRequest { device_id: id() }))
            .await
            .unwrap();
        rig.svc
            .reset_channel(Request::new(ResetChannelRequest {
                device_id: id(),
                channel_id: "argb1".to_string(),
            }))
            .await
            .unwrap();
        rig.svc
            .shutdown(Request::new(ShutdownRequest {}))
            .await
            .unwrap();
    }
    sleep(Duration::from_secs(3600)).await;
    rig.svc.flusher().flush(Duration::from_secs(5)).await;
    assert!(rig.bus.ops().is_empty());
}
