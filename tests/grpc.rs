// SPDX-License-Identifier: GPL-3.0-or-later
//
// End-to-end test: the real gRPC server on a Unix socket, a tonic client, and a
// mock HID bus in place of /dev/hidrawN.

mod common;

use cc_asus_aura_argb::autosave::{AutoSave, StateStore};
use cc_asus_aura_argb::device_service::v1::device_service_client::DeviceServiceClient;
use cc_asus_aura_argb::device_service::v1::{
    FixedDutyRequest, HealthRequest, InitializeDeviceRequest, ListDevicesRequest,
    ResetChannelRequest, ShutdownRequest, StatusRequest, health_response,
};
use cc_asus_aura_argb::hid::mock::MockBus;
use cc_asus_aura_argb::models::v1::channel_info::Options;
use cc_asus_aura_argb::protocol::Effect;
use cc_asus_aura_argb::server::serve_uds;
use cc_asus_aura_argb::service::AuraService;
use common::{DEVICE_ID, device, key, lighting, saves};
use hyper_util::rt::TokioIo;
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tonic::Code;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

struct Harness {
    client: DeviceServiceClient<Channel>,
    stop: Option<oneshot::Sender<()>>,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
    socket: PathBuf,
    _dir: tempfile::TempDir,
}

impl Harness {
    async fn start(service: AuraService) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("plugin.sock");
        // A stale socket from a previous run must not prevent startup.
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());

        let (tx, rx) = oneshot::channel::<()>();
        let sock = socket.clone();
        let server = tokio::spawn(async move {
            serve_uds(service, &sock, async {
                let _ = rx.await;
            })
            .await
        });

        let mut last_err = None;
        for _ in 0..100 {
            let sock = socket.clone();
            match Endpoint::try_from("http://[::]:50051")
                .unwrap()
                .connect_with_connector(service_fn(move |_: Uri| {
                    let sock = sock.clone();
                    async move {
                        Ok::<_, std::io::Error>(TokioIo::new(UnixStream::connect(sock).await?))
                    }
                }))
                .await
            {
                Ok(channel) => {
                    return Harness {
                        client: DeviceServiceClient::new(channel),
                        stop: Some(tx),
                        server,
                        socket,
                        _dir: dir,
                    };
                }
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
        panic!("server did not come up: {last_err:?}");
    }

    async fn stop(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        drop(self.client);
        self.server.await.unwrap().unwrap();
        assert!(!self.socket.exists(), "socket must be removed on shutdown");
    }
}

#[tokio::test]
async fn list_devices_and_read_only_calls_do_not_touch_the_device() {
    let bus = MockBus::new();
    let mut h = Harness::start(AuraService::new(vec![device(&bus)])).await;

    let health = h
        .client
        .health(HealthRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(health.name, "cc-asus-aura-argb");
    assert_eq!(health.status, health_response::Status::Ok as i32);

    let devices = h
        .client
        .list_devices(ListDevicesRequest {})
        .await
        .unwrap()
        .into_inner()
        .devices;
    assert_eq!(devices.len(), 1);
    let d = &devices[0];
    assert_eq!(d.id, DEVICE_ID);
    assert_eq!(d.name, "ASUS Aura ARGB");
    assert_eq!(d.uid_info.as_deref(), Some("0b05:19af at usb 3-2"));
    let info = d.info.as_ref().unwrap();
    assert!(info.lighting_speeds.is_empty());
    assert!(info.temps.is_empty());
    let mut ids: Vec<(&str, &str)> = info
        .channels
        .iter()
        .map(|(id, c)| (id.as_str(), c.label.as_deref().unwrap()))
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        vec![
            ("argb1", "ARGB header 1"),
            ("argb2", "ARGB header 2"),
            ("argb3", "ARGB header 3"),
            ("mainboard", "Mainboard LEDs"),
            ("sync", "All channels"),
        ]
    );
    for c in info.channels.values() {
        let Some(Options::LightingModes(modes)) = &c.options else {
            panic!("every channel must be a lighting channel");
        };
        let got: Vec<(&str, u32, u32, bool, bool)> = modes
            .lighting_mode
            .iter()
            .map(|m| {
                (
                    m.name.as_str(),
                    m.min_colors,
                    m.max_colors,
                    m.speed_enabled,
                    m.backward_enabled,
                )
            })
            .collect();
        let want: Vec<(&str, u32, u32, bool, bool)> = Effect::ALL
            .iter()
            .map(|e| {
                let n = u32::from(e.takes_color());
                (e.name(), n, n, false, false)
            })
            .collect();
        assert_eq!(got, want);
        assert!(got.iter().any(|m| m.0 == "rainbow"));
    }

    h.client
        .status(StatusRequest {
            device_id: DEVICE_ID.to_string(),
        })
        .await
        .unwrap();
    h.client
        .initialize_device(InitializeDeviceRequest {
            device_id: DEVICE_ID.to_string(),
        })
        .await
        .unwrap();
    h.client
        .reset_channel(ResetChannelRequest {
            device_id: DEVICE_ID.to_string(),
            channel_id: "argb2".to_string(),
        })
        .await
        .unwrap();
    h.client.shutdown(ShutdownRequest {}).await.unwrap();
    assert_eq!(bus.ops(), vec![], "no HID traffic expected");

    let err = h
        .client
        .fixed_duty(FixedDutyRequest::default())
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unimplemented);

    // Shutdown RPC must not stop the server.
    h.client.health(HealthRequest {}).await.unwrap();
    h.stop().await;
}

#[tokio::test]
async fn each_channel_gets_its_own_effect() {
    let bus = MockBus::new();
    let mut h = Harness::start(AuraService::new(vec![device(&bus)])).await;

    h.client
        .lighting(lighting("argb2", "static", &[(255, 0, 0)]))
        .await
        .unwrap();
    h.client
        .lighting(lighting("argb1", "rainbow", &[]))
        .await
        .unwrap();
    h.client
        .lighting(lighting("argb3", "breathing", &[(0, 0, 255)]))
        .await
        .unwrap();
    h.client
        .lighting(lighting("mainboard", "off", &[]))
        .await
        .unwrap();
    assert_eq!(
        bus.writes_trimmed(),
        vec![
            // argb2: effect channel 2 static, red in slot 2.
            vec![0xEC, 0x35, 0x02, 0, 0, 0x01],
            vec![0xEC, 0x36, 0, 0x04, 0, 0, 0, 0, 0, 0, 0, 0xff],
            // argb1: effect channel 1 rainbow, black in slot 1.
            vec![0xEC, 0x35, 0x01, 0, 0, 0x05],
            vec![0xEC, 0x36, 0, 0x02],
            // argb3: effect channel 3 breathing, blue in slot 3.
            vec![0xEC, 0x35, 0x03, 0, 0, 0x02],
            vec![
                0xEC, 0x36, 0, 0x08, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff
            ],
            // mainboard: effect channel 0, mode 0 (off): all zero after 0x35, trimmed.
            vec![0xEC, 0x35],
            vec![0xEC, 0x36, 0, 0x01],
        ]
    );
    assert_eq!(saves(&bus), 0, "no save without auto-save");
    h.stop().await;
}

#[tokio::test]
async fn invalid_lighting_requests_are_rejected_without_io() {
    let bus = MockBus::new();
    let mut h = Harness::start(AuraService::new(vec![device(&bus)])).await;

    let mut unknown_device = lighting("argb1", "static", &[(1, 2, 3)]);
    unknown_device.device_id = "nope".to_string();
    let mut no_setting = lighting("argb1", "static", &[]);
    no_setting.setting = None;

    let cases = [
        (unknown_device, Code::NotFound),
        (lighting("led2", "static", &[(1, 2, 3)]), Code::NotFound),
        (no_setting, Code::InvalidArgument),
        (lighting("argb1", "music", &[]), Code::InvalidArgument),
        (
            lighting("argb1", "direct_fill", &[(1, 1, 1)]),
            Code::InvalidArgument,
        ),
        (lighting("argb1", "static", &[]), Code::InvalidArgument),
        (
            lighting("argb1", "static", &[(300, 0, 0)]),
            Code::InvalidArgument,
        ),
    ];
    for (req, code) in cases {
        let err = h.client.lighting(req.clone()).await.unwrap_err();
        assert_eq!(err.code(), code, "{req:?}: {}", err.message());
    }
    assert_eq!(
        bus.ops(),
        vec![],
        "rejected requests must not reach the bus"
    );

    // Bus failure surfaces as an error, and the server keeps running.
    bus.fail_writes(Some(libc::EIO));
    let err = h
        .client
        .lighting(lighting("argb1", "static", &[(1, 2, 3)]))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unavailable);
    bus.fail_writes(None);
    h.client
        .lighting(lighting("argb1", "static", &[(1, 2, 3)]))
        .await
        .unwrap();
    h.stop().await;
}

/// Over the real socket: with auto-save on, only a real change writes `EC 3F 55`.
#[tokio::test]
async fn auto_save_only_on_change_and_never_from_other_rpcs() {
    let bus = MockBus::new();
    let state = tempfile::tempdir().unwrap();
    let store = StateStore::new(state.path());
    let auto = AutoSave::new(store.clone(), Duration::from_millis(100));
    let service = AuraService::new(vec![device(&bus)]).with_auto_save(Some(auto));
    let mut h = Harness::start(service).await;

    h.client
        .lighting(lighting("argb2", "static", &[(255, 0, 0)]))
        .await
        .unwrap();
    assert_eq!(saves(&bus), 0, "nothing before the delay");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(saves(&bus), 1);
    assert_eq!(
        bus.writes_trimmed().last().unwrap(),
        &vec![0xEC, 0x3F, 0x55]
    );
    let record = store.load(&key()).unwrap();
    assert_eq!(record.len(), 4);
    assert_eq!(
        record[2],
        "argb2 effect=01 channel=02 slots=2+1 color=ff0000"
    );
    assert_eq!(record[0], "mainboard unknown");

    // A daemon restart: InitializeDevice, then the same setting re-sent. It is
    // written (the controller may have changed), but it is no change: no save.
    h.client
        .initialize_device(InitializeDeviceRequest {
            device_id: DEVICE_ID.to_string(),
        })
        .await
        .unwrap();
    h.client
        .lighting(lighting("argb2", "static", &[(255, 0, 0)]))
        .await
        .unwrap();
    for _ in 0..3 {
        h.client.health(HealthRequest {}).await.unwrap();
        h.client
            .status(StatusRequest {
                device_id: DEVICE_ID.to_string(),
            })
            .await
            .unwrap();
        h.client.list_devices(ListDevicesRequest {}).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(saves(&bus), 1);
    assert_eq!(
        bus.writes().len(),
        2 + 1 + 2,
        "apply, save, and the re-apply after InitializeDevice"
    );
    h.stop().await;
}

#[tokio::test]
async fn zero_devices_still_serves() {
    let mut h = Harness::start(AuraService::new(Vec::new())).await;
    let devices = h
        .client
        .list_devices(ListDevicesRequest {})
        .await
        .unwrap()
        .into_inner()
        .devices;
    assert!(devices.is_empty());
    let err = h
        .client
        .lighting(lighting("argb1", "static", &[(1, 2, 3)]))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    h.stop().await;
}
