// SPDX-License-Identifier: GPL-3.0-or-later
//
// cc-asus-aura-argb: CoolerControl device-service plugin for independent per-channel
// effects on ASUS Aura USB mainboard lighting controllers.
//
// Copyright (C) the cc-asus-aura-argb contributors.
// Service skeleton derived from the CoolerControl device-service template
// (GPL-3.0-or-later).
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. See the LICENSE file.

pub mod autosave;
pub mod descriptor;
pub mod device;
pub mod discovery;
pub mod hid;
pub mod precedence;
pub mod protocol;
pub mod server;
pub mod service;

pub const SERVICE_NAME: &str = env!("CARGO_PKG_NAME");
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default socket path, matching `dist/manifest.toml`.
pub const DEFAULT_SOCKET: &str =
    "/var/lib/coolercontrol/plugins/asus-aura-argb/asus-aura-argb.sock";

#[allow(clippy::all, clippy::pedantic)]
pub mod models {
    pub mod v1 {
        tonic::include_proto!("coolercontrol.models.v1");
    }
}

#[allow(clippy::all, clippy::pedantic)]
pub mod device_service {
    pub mod v1 {
        tonic::include_proto!("coolercontrol.device_service.v1");
    }
}

use device::{AuraDevice, DeviceInfo};
use discovery::FoundController;
use hid::{HidBus, LinuxHidraw, QueryError, QueryPolicy};
use log::{error, info, warn};
use protocol::{ConfigTable, Report};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Name shown in CoolerControl; distinct from its built-in "ASUS Aura LED Controller".
pub const DEVICE_NAME: &str = "ASUS Aura ARGB";

/// What the startup requests returned.
#[derive(Clone, Debug)]
pub struct ProbeResult {
    /// `None` if the firmware request got no answer (not fatal).
    pub firmware: Option<String>,
    pub firmware_reply: Option<Report>,
    pub config_reply: Report,
    pub table: ConfigTable,
}

/// The startup reads: firmware string, then the config table. These are the only
/// reads the plugin ever makes. Only a missing config table is an error.
pub fn probe<B: HidBus + ?Sized>(
    bus: &mut B,
    policy: QueryPolicy,
) -> Result<ProbeResult, QueryError> {
    let (firmware, firmware_reply) = match hid::query(
        bus,
        &protocol::firmware_request(),
        protocol::REPLY_FIRMWARE,
        policy,
    ) {
        Ok(r) => (protocol::parse_firmware_reply(&r).ok(), Some(r)),
        Err(QueryError::Io(e)) => return Err(QueryError::Io(e)),
        Err(e) => {
            warn!("Firmware version not read: {e}");
            (None, None)
        }
    };
    let config_reply = hid::query(
        bus,
        &protocol::config_request(),
        protocol::REPLY_CONFIG,
        policy,
    )?;
    let table = protocol::parse_config_reply(&config_reply)
        .expect("query only returns replies with the requested id");
    Ok(ProbeResult {
        firmware,
        firmware_reply,
        config_reply,
        table,
    })
}

/// Builds the device from a discovery result and a probe. No I/O apart from what
/// `bus` was already used for.
pub fn build_device(
    found: &FoundController,
    board: Option<&str>,
    probe: &ProbeResult,
    bus: Box<dyn HidBus + Send>,
) -> AuraDevice {
    let channels = device::channels_from_table(&probe.table);
    let firmware = probe
        .firmware
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let product = found.product_string.as_deref().unwrap_or(found.model.name);
    let model = match board {
        Some(b) => format!("{b}: {product}, firmware {firmware}"),
        None => format!("{product}, firmware {firmware}"),
    };
    AuraDevice::new(
        DeviceInfo {
            id: format!("aura-usb-{}", found.usb_name),
            name: DEVICE_NAME.to_string(),
            model,
            key: found.key(),
            firmware,
            locations: vec![
                found.dev_path().display().to_string(),
                found.usb_path.display().to_string(),
            ],
        },
        channels,
        bus,
    )
}

/// Opens one discovered controller's node (checking its USB ids with the kernel).
pub fn open_bus(found: &FoundController, sysfs_root: &Path) -> anyhow::Result<LinuxHidraw> {
    let path = found.dev_path();
    let key = found.key();
    let root: PathBuf = sysfs_root.to_path_buf();
    let bus = LinuxHidraw::open(&path, found.model.vendor, found.model.product)
        .map_err(|e| {
            let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
                " (run as root)"
            } else {
                ""
            };
            anyhow::anyhow!("cannot open {}: {e}{hint}", path.display())
        })?
        .with_locator(Box::new(move || discovery::locate(&root, &key)));
    Ok(bus)
}

/// Opens, probes and builds one controller.
pub fn open_device(found: &FoundController, sysfs_root: &Path) -> anyhow::Result<AuraDevice> {
    let mut bus = open_bus(found, sysfs_root)?;
    let result = probe(&mut bus, QueryPolicy::default())
        .map_err(|e| anyhow::anyhow!("{}: reading the config table failed: {e}", found.hidraw))?;
    let board = discovery::board_name(sysfs_root);
    let dev = build_device(found, board.as_deref(), &result, Box::new(bus));
    info!(
        "Found {} at usb {} ({}), firmware {}, channels: {}",
        found.model.name,
        found.usb_name,
        found.dev_path().display(),
        dev.info.firmware,
        dev.channels
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if dev.channels.is_empty() {
        warn!("The config table lists no lighting channels");
    }
    Ok(dev)
}

/// Finds and probes every supported controller. Waits up to `wait` for one to
/// appear (USB enumeration can finish after the service starts). Failures are
/// logged, never fatal: the result may be empty.
pub fn detect_devices(sysfs_root: &Path, wait: Duration) -> Vec<AuraDevice> {
    let deadline = Instant::now() + wait;
    let found = loop {
        match discovery::discover(sysfs_root) {
            Ok(f) if !f.is_empty() => break f,
            Ok(_) => {}
            Err(e) => {
                error!("Cannot scan {}/class/hidraw: {e}", sysfs_root.display());
                return Vec::new();
            }
        }
        if Instant::now() >= deadline {
            warn!("No supported controller found (USB 0b05:19af). Serving with no devices.");
            return Vec::new();
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    found
        .iter()
        .filter_map(|f| match open_device(f, sysfs_root) {
            Ok(d) => Some(d),
            Err(e) => {
                error!("{e:#}. Not listing this device.");
                None
            }
        })
        .collect()
}
