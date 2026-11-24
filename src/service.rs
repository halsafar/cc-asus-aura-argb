// SPDX-License-Identifier: GPL-3.0-or-later
//
// gRPC DeviceService implementation.
// Structure based on the CoolerControl device-service template (GPL-3.0-or-later).
//
// Only `Lighting` and `ResetChannel` write to the controller (the latter only when it
// changes what a channel should show), plus the debounced auto-save they schedule.
// `Health`, `ListDevices`, `Status`, `InitializeDevice` and `Shutdown` never touch
// it. Nothing is ever read from it after startup.
//
// Besides one channel per effect channel there is `sync` ("All channels"). Which
// setting a physical channel shows is decided by the precedence records
// (`precedence.rs`): the most recent user change wins, and re-sent settings inside
// the re-apply window do not count as changes.

use crate::autosave::AutoSave;
use crate::device::{ApplyError, AuraDevice, Channel, SaveOutcome, Setting};
use crate::device_service::v1::device_service_server::DeviceService;
use crate::device_service::v1::{
    CustomFunctionOneRequest, CustomFunctionOneResponse, EnableManualFanControlRequest,
    EnableManualFanControlResponse, FixedDutyRequest, FixedDutyResponse, HealthRequest,
    HealthResponse, InitializeDeviceRequest, InitializeDeviceResponse, LcdRequest, LcdResponse,
    LightingRequest, LightingResponse, LightingSetting, ListDevicesRequest, ListDevicesResponse,
    ResetChannelRequest, ResetChannelResponse, ShutdownRequest, ShutdownResponse,
    SpeedProfileRequest, SpeedProfileResponse, StatusRequest, StatusResponse, health_response,
};
use crate::models::v1::{
    ChannelInfo, Device, DeviceInfo, DriverInfo, LightingModes, channel_info,
    lighting_modes::LightingMode,
};
use crate::precedence::{Outcome, PrecedenceStore, Request as PRequest, SYNC_ID};
use crate::protocol::{Effect, Rgb};
use crate::{SERVICE_NAME, VERSION};
use log::{debug, info, warn};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;
use tonic::{Request, Response, Status};

/// How long after `InitializeDevice` or a detected resume a re-sent, unchanged
/// setting counts as a re-apply rather than a user change. See docs/design.md.
pub const DEFAULT_REAPPLY_WINDOW: Duration = Duration::from_secs(20);

/// Growth of (boot time - monotonic time) that counts as a suspend.
const SUSPEND_THRESHOLD: Duration = Duration::from_secs(1);

/// Time the system has spent suspended since boot: `CLOCK_BOOTTIME` counts it,
/// `CLOCK_MONOTONIC` does not. `None` if either clock is unavailable.
pub fn suspended_time() -> Option<Duration> {
    let read = |id: libc::clockid_t| {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid timespec that outlives the call.
        if unsafe { libc::clock_gettime(id, &mut ts) } != 0 {
            return None;
        }
        Some(Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
    };
    let boot = read(libc::CLOCK_BOOTTIME)?;
    let mono = read(libc::CLOCK_MONOTONIC)?;
    Some(boot.saturating_sub(mono))
}

/// Source of [`suspended_time`]; replaceable in tests.
pub type SleepClock = Arc<dyn Fn() -> Option<Duration> + Send + Sync>;

fn device_model(dev: &AuraDevice) -> Device {
    let mut channels: HashMap<String, ChannelInfo> = dev
        .channels
        .iter()
        .map(|c| (c.id.clone(), channel_model(&c.label)))
        .collect();
    if !dev.channels.is_empty() {
        channels.insert(SYNC_ID.to_string(), channel_model("All channels"));
    }
    let info = &dev.info;
    Device {
        id: info.id.clone(),
        name: info.name.clone(),
        uid_info: Some(info.key.to_string()),
        info: Some(DeviceInfo {
            channels,
            temps: HashMap::new(),
            // The controller has no speed setting for its effects.
            lighting_speeds: Vec::new(),
            temp_min: None,
            temp_max: None,
            profile_min_length: None,
            profile_max_length: None,
            model: Some(info.model.clone()),
            driver_info: Some(DriverInfo {
                name: Some(SERVICE_NAME.to_string()),
                version: Some(VERSION.to_string()),
                locations: info.locations.clone(),
            }),
        }),
    }
}

fn channel_model(label: &str) -> ChannelInfo {
    let lighting_mode = Effect::ALL
        .iter()
        .map(|e| LightingMode {
            name: e.name().to_string(),
            frontend_name: Some(e.frontend_name().to_string()),
            min_colors: u32::from(e.takes_color()),
            max_colors: u32::from(e.takes_color()),
            // The controller has no speed or direction setting for its effects.
            speed_enabled: false,
            backward_enabled: false,
        })
        .collect();
    ChannelInfo {
        label: Some(label.to_string()),
        options: Some(channel_info::Options::LightingModes(LightingModes {
            lighting_mode,
        })),
    }
}

/// Validates a `LightingSetting` from CoolerControl for one channel.
pub fn parse_setting(channel: &Channel, setting: &LightingSetting) -> Result<Setting, Status> {
    let colors = setting
        .colors
        .iter()
        .map(|c| {
            let v = |x: u32, which: &str| {
                u8::try_from(x).map_err(|_| {
                    Status::invalid_argument(format!(
                        "colour {which} value {x} is out of range 0-255"
                    ))
                })
            };
            Ok(Rgb::new(v(c.r, "red")?, v(c.g, "green")?, v(c.b, "blue")?))
        })
        .collect::<Result<Vec<Rgb>, Status>>()?;
    if setting.speed.as_deref().is_some_and(|s| !s.is_empty()) || setting.backward == Some(true) {
        debug!("ignoring speed/backward: the controller has neither");
    }
    channel
        .setting(&setting.mode, &colors)
        .map_err(Status::invalid_argument)
}

/// Per-device precedence state, behind an async lock that is held for the whole
/// request so precedence and hardware writes stay in step.
struct Lane {
    store: PrecedenceStore,
    /// Re-apply window end, if one is open.
    window_until: Option<tokio::time::Instant>,
    /// Last seen [`suspended_time`].
    slept: Option<Duration>,
}

pub struct AuraService {
    devices: Vec<Arc<AuraDevice>>,
    lanes: Vec<AsyncMutex<Lane>>,
    auto_save: Option<Arc<AutoSave>>,
    started: Instant,
    window: Duration,
    sleep_clock: SleepClock,
}

/// Flushes pending saves at shutdown. Obtained before the service is moved into
/// the server.
#[derive(Clone)]
pub struct Flusher {
    devices: Vec<Arc<AuraDevice>>,
    auto_save: Option<Arc<AutoSave>>,
}

impl Flusher {
    /// Runs a compare-then-save for every device with a pending save, giving up after
    /// `timeout` so shutdown cannot hang.
    pub async fn flush(&self, timeout: Duration) {
        let Some(auto) = &self.auto_save else {
            return;
        };
        let work = async {
            for dev in &self.devices {
                if auto.run(Arc::clone(dev), None).await == SaveOutcome::Saved {
                    info!("{}: pending save flushed at shutdown", dev.id());
                }
            }
        };
        if tokio::time::timeout(timeout, work).await.is_err() {
            warn!("Pending save not flushed within {timeout:?}; giving up");
        }
    }
}

impl AuraService {
    /// A service without auto-save: it never writes the save report.
    /// Precedence is kept in memory until [`Self::with_precedence_dir`] is used.
    pub fn new(devices: Vec<AuraDevice>) -> Self {
        let clock: SleepClock = Arc::new(suspended_time);
        let lanes = devices
            .iter()
            .map(|_| {
                AsyncMutex::new(Lane {
                    store: PrecedenceStore::in_memory(),
                    window_until: None,
                    slept: clock(),
                })
            })
            .collect();
        AuraService {
            devices: devices.into_iter().map(Arc::new).collect(),
            lanes,
            auto_save: None,
            started: Instant::now(),
            window: DEFAULT_REAPPLY_WINDOW,
            sleep_clock: clock,
        }
    }

    /// Keeps the precedence records in `dir` (the state directory), so they survive
    /// restarts. `None`: memory only, logged once.
    pub fn with_precedence_dir(mut self, dir: Option<&Path>) -> Self {
        match dir {
            Some(d) => {
                for (lane, dev) in self.lanes.iter_mut().zip(&self.devices) {
                    lane.get_mut().store = PrecedenceStore::in_dir(d, &dev.info.key);
                }
            }
            None if !self.devices.is_empty() => warn!(
                "No state directory: which of 'sync' and a channel was changed last is kept \
                 in memory only; after a service restart the first re-apply decides"
            ),
            None => {}
        }
        self
    }

    pub fn with_reapply_window(mut self, window: Duration) -> Self {
        self.window = window;
        self
    }

    /// Replaces the suspend detector (tests).
    pub fn with_sleep_clock(mut self, clock: SleepClock) -> Self {
        let now = clock();
        for lane in &mut self.lanes {
            lane.get_mut().slept = now;
        }
        self.sleep_clock = clock;
        self
    }

    pub fn with_auto_save(mut self, auto_save: Option<Arc<AutoSave>>) -> Self {
        self.auto_save = auto_save;
        self
    }

    pub fn flusher(&self) -> Flusher {
        Flusher {
            devices: self.devices.clone(),
            auto_save: self.auto_save.clone(),
        }
    }

    fn find_index(&self, device_id: &str) -> Result<usize, Status> {
        self.devices
            .iter()
            .position(|d| d.id() == device_id)
            .ok_or_else(|| Status::not_found(format!("unknown device '{device_id}'")))
    }

    /// Opens the re-apply window and forgets what the controller shows.
    fn open_window(&self, lane: &mut Lane, dev: &AuraDevice) {
        dev.invalidate();
        lane.window_until = Some(tokio::time::Instant::now() + self.window);
    }

    /// Notices a suspend since the last call (and then opens the window, because
    /// CoolerControl re-applies after resume without calling `InitializeDevice`).
    /// Returns whether the window is open.
    fn check_window(&self, lane: &mut Lane, dev: &AuraDevice) -> bool {
        if let Some(now) = (self.sleep_clock)() {
            if lane
                .slept
                .is_some_and(|before| now > before + SUSPEND_THRESHOLD)
            {
                info!(
                    "{}: the system was suspended; re-writing every channel",
                    dev.id()
                );
                self.open_window(lane, dev);
            }
            lane.slept = Some(now);
        }
        lane.window_until
            .is_some_and(|until| tokio::time::Instant::now() < until)
    }

    /// The physical channels a logical channel covers.
    fn covered(dev: &AuraDevice, logical: &str) -> Result<Vec<usize>, Status> {
        if logical == SYNC_ID && !dev.channels.is_empty() {
            return Ok((0..dev.channels.len()).collect());
        }
        dev.index_of(logical).map(|i| vec![i]).ok_or_else(|| {
            let mut ids: Vec<&str> = dev.channels.iter().map(|c| c.id.as_str()).collect();
            ids.push(SYNC_ID);
            Status::not_found(format!(
                "unknown channel '{logical}' (channels: {})",
                ids.join(", ")
            ))
        })
    }

    /// Records the request, then writes the effective settings of the covered
    /// channels (skipping what the controller already shows) and schedules auto-save.
    async fn handle(
        &self,
        index: usize,
        logical: &str,
        covered: Vec<usize>,
        request: PRequest,
    ) -> Result<(), Status> {
        let dev = Arc::clone(&self.devices[index]);
        let mut lane = self.lanes[index].lock().await;
        let in_window = self.check_window(&mut lane, &dev);
        let mut records = lane.store.load();
        let outcome = records.request(logical, request, in_window);
        if matches!(outcome, Outcome::Changed | Outcome::Dropped) {
            if let Err(e) = lane.store.store(&records) {
                warn!("{}: cannot write the precedence record: {e}", dev.id());
            }
        }
        let plan: Vec<(usize, Setting)> = covered
            .iter()
            .filter_map(|&i| records.effective(&dev.channels[i].id).map(|s| (i, s)))
            .collect();
        let what = match request {
            PRequest::Set(s) => format!("{s}"),
            PRequest::Reset => "reset".to_string(),
        };
        let shows: Vec<String> = plan
            .iter()
            .map(|(i, s)| format!("{}={s}", dev.channels[*i].id))
            .collect();
        info!(
            "{} {logical}: {what} ({}); shows {}",
            dev.id(),
            match outcome {
                Outcome::Reapply => "re-apply",
                Outcome::Changed => "change",
                Outcome::Dropped => "record dropped",
                Outcome::Unchanged => "nothing recorded",
            },
            if shows.is_empty() {
                "nothing new".to_string()
            } else {
                shows.join(" ")
            }
        );
        // A user change always writes every channel it covers: the plugin's belief of
        // what the controller shows can be wrong (another driver or tool writing, a
        // controller reset), and clicking the same setting again must fix that. Only a
        // re-apply (or a reset that changes nothing) may skip channels known to show
        // their setting, and that knowledge is dropped after InitializeDevice or a
        // detected suspend.
        let force = matches!(outcome, Outcome::Changed | Outcome::Dropped);
        let applying = Arc::clone(&dev);
        let result = tokio::task::spawn_blocking(move || applying.apply_many(&plan, force, false))
            .await
            .map_err(|e| Status::internal(format!("lighting task failed: {e}")))?;
        drop(lane);
        match result {
            Ok(Some(generation)) => {
                if let Some(auto) = &self.auto_save {
                    auto.schedule(dev, generation);
                }
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(ApplyError::UnknownChannel(c)) => {
                Err(Status::not_found(format!("unknown channel '{c}'")))
            }
            Err(e @ ApplyError::Io(_)) => {
                warn!("{e}");
                Err(Status::unavailable(e.to_string()))
            }
        }
    }
}

#[tonic::async_trait]
impl DeviceService for AuraService {
    async fn health(
        &self,
        _request: Request<HealthRequest>,
    ) -> Result<Response<HealthResponse>, Status> {
        Ok(Response::new(HealthResponse {
            name: SERVICE_NAME.to_string(),
            version: VERSION.to_string(),
            status: health_response::Status::Ok.into(),
            uptime_seconds: self.started.elapsed().as_secs(),
        }))
    }

    async fn list_devices(
        &self,
        _request: Request<ListDevicesRequest>,
    ) -> Result<Response<ListDevicesResponse>, Status> {
        Ok(Response::new(ListDevicesResponse {
            devices: self.devices.iter().map(|d| device_model(d)).collect(),
        }))
    }

    /// Called when CoolerControl connects (daemon start). CoolerControl then re-applies
    /// its saved settings, so this opens the re-apply window and makes the next write
    /// to every channel real. No I/O.
    async fn initialize_device(
        &self,
        request: Request<InitializeDeviceRequest>,
    ) -> Result<Response<InitializeDeviceResponse>, Status> {
        let index = self.find_index(&request.get_ref().device_id)?;
        let dev = &self.devices[index];
        let mut lane = self.lanes[index].lock().await;
        self.check_window(&mut lane, dev);
        self.open_window(&mut lane, dev);
        debug!("InitializeDevice {}: re-apply window open", dev.id());
        Ok(Response::new(InitializeDeviceResponse {}))
    }

    /// Does not exit: this service is run by systemd, not by CoolerControl, and must
    /// still be there when the daemon starts again.
    async fn shutdown(
        &self,
        _request: Request<ShutdownRequest>,
    ) -> Result<Response<ShutdownResponse>, Status> {
        info!("CoolerControl sent Shutdown; staying up for the next connection");
        Ok(Response::new(ShutdownResponse {}))
    }

    /// No sensors: always empty, never touches the controller.
    async fn status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        Ok(Response::new(StatusResponse { status: Vec::new() }))
    }

    /// Drops the logical channel's record: resetting `sync` lets the channels' own
    /// settings show, resetting a channel lets `sync` show through. A channel left
    /// with neither keeps what it shows. Writes only what changes.
    async fn reset_channel(
        &self,
        request: Request<ResetChannelRequest>,
    ) -> Result<Response<ResetChannelResponse>, Status> {
        let req = request.into_inner();
        let index = self.find_index(&req.device_id)?;
        let covered = Self::covered(&self.devices[index], &req.channel_id)?;
        self.handle(index, &req.channel_id, covered, PRequest::Reset)
            .await?;
        Ok(Response::new(ResetChannelResponse {}))
    }

    async fn enable_manual_fan_control(
        &self,
        _request: Request<EnableManualFanControlRequest>,
    ) -> Result<Response<EnableManualFanControlResponse>, Status> {
        Err(Status::unimplemented("No Fans"))
    }

    async fn fixed_duty(
        &self,
        _request: Request<FixedDutyRequest>,
    ) -> Result<Response<FixedDutyResponse>, Status> {
        Err(Status::unimplemented("No Fans"))
    }

    async fn speed_profile(
        &self,
        _request: Request<SpeedProfileRequest>,
    ) -> Result<Response<SpeedProfileResponse>, Status> {
        Err(Status::unimplemented("No Firmware Profiles"))
    }

    async fn lighting(
        &self,
        request: Request<LightingRequest>,
    ) -> Result<Response<LightingResponse>, Status> {
        let req = request.into_inner();
        let index = self.find_index(&req.device_id)?;
        let dev = &self.devices[index];
        let covered = Self::covered(dev, &req.channel_id)?;
        let setting = req
            .setting
            .ok_or_else(|| Status::invalid_argument("missing lighting setting"))?;
        let parsed = parse_setting(&dev.channels[covered[0]], &setting)?;
        self.handle(index, &req.channel_id, covered, PRequest::Set(parsed))
            .await?;
        Ok(Response::new(LightingResponse {}))
    }

    async fn lcd(&self, _request: Request<LcdRequest>) -> Result<Response<LcdResponse>, Status> {
        Err(Status::unimplemented("No LCD Channels"))
    }

    async fn custom_function_one(
        &self,
        _request: Request<CustomFunctionOneRequest>,
    ) -> Result<Response<CustomFunctionOneResponse>, Status> {
        Err(Status::unimplemented("No Custom Function"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::channels_from_table;
    use crate::device_service::v1::Rgb as PbRgb;
    use crate::protocol::ConfigTable;

    fn channels() -> Vec<Channel> {
        let mut t = [0u8; 60];
        t[2] = 1;
        t[0x1B] = 1;
        channels_from_table(&ConfigTable(t))
    }

    fn setting(mode: &str, colors: &[(u32, u32, u32)], backward: Option<bool>) -> LightingSetting {
        LightingSetting {
            mode: mode.to_string(),
            speed: Some("fast".to_string()),
            backward,
            colors: colors.iter().map(|&(r, g, b)| PbRgb { r, g, b }).collect(),
        }
    }

    #[test]
    fn parses_and_rejects() {
        let ch = channels();
        assert_eq!(
            parse_setting(&ch[0], &setting("breathing", &[(1, 2, 3)], Some(true))).unwrap(),
            Setting {
                effect: Effect::Breathing,
                color: Rgb::new(1, 2, 3)
            }
        );
        // Colour-less effect: colours ignored, black sent.
        assert_eq!(
            parse_setting(&ch[1], &setting("spectrum_cycle", &[(9, 9, 9)], None))
                .unwrap()
                .color,
            Rgb::BLACK
        );
        let code = |c: &Channel, s: LightingSetting| parse_setting(c, &s).unwrap_err().code();
        assert_eq!(
            code(&ch[0], setting("static", &[(256, 0, 0)], None)),
            tonic::Code::InvalidArgument
        );
        assert_eq!(
            code(&ch[0], setting("static", &[], None)),
            tonic::Code::InvalidArgument
        );
        assert_eq!(
            code(&ch[1], setting("music", &[], None)),
            tonic::Code::InvalidArgument
        );
    }
}
