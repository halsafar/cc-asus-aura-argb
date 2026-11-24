// SPDX-License-Identifier: GPL-3.0-or-later
//
// Shared test fixtures: a device built from the tested board's config table, on a
// mock bus.

#![allow(dead_code)]

use cc_asus_aura_argb::device::{AuraDevice, DeviceInfo, channels_from_table};
use cc_asus_aura_argb::device_service::v1::{LightingRequest, LightingSetting, Rgb};
use cc_asus_aura_argb::discovery::ControllerKey;
use cc_asus_aura_argb::hid::mock::MockBus;
use cc_asus_aura_argb::protocol::ConfigTable;

pub const DEVICE_ID: &str = "aura-usb-3-2";

/// Config table read from the tested board (firmware AULA3-AR32-0309).
pub const Z890_TABLE: [u8; 60] = [
    0x1e, 0x9f, 0x03, 0x01, 0x00, 0x00, //
    0x78, 0x3c, 0x00, 0x01, 0x00, 0x00, //
    0x78, 0x3c, 0x00, 0x01, 0x00, 0x00, //
    0x78, 0x3c, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x01, 0x04, 0x04, //
    0x01, 0xf4, 0x00, 0x00, 0x00, 0x00, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

pub fn key() -> ControllerKey {
    ControllerKey {
        vendor: 0x0b05,
        product: 0x19af,
        usb_name: "3-2".to_string(),
    }
}

pub fn device(bus: &MockBus) -> AuraDevice {
    AuraDevice::new(
        DeviceInfo {
            id: DEVICE_ID.to_string(),
            name: "ASUS Aura ARGB".to_string(),
            model: "test board: AURA LED Controller, firmware AULA3-AR32-0309".to_string(),
            key: key(),
            firmware: "AULA3-AR32-0309".to_string(),
            locations: vec!["/dev/hidraw0".to_string()],
        },
        channels_from_table(&ConfigTable(Z890_TABLE)),
        Box::new(bus.clone()),
    )
}

pub fn lighting(channel: &str, mode: &str, colors: &[(u32, u32, u32)]) -> LightingRequest {
    LightingRequest {
        device_id: DEVICE_ID.to_string(),
        channel_id: channel.to_string(),
        setting: Some(LightingSetting {
            mode: mode.to_string(),
            speed: None,
            backward: Some(false),
            colors: colors.iter().map(|&(r, g, b)| Rgb { r, g, b }).collect(),
        }),
    }
}

/// Number of save reports (`EC 3F`) written so far.
pub fn saves(bus: &MockBus) -> usize {
    bus.writes().iter().filter(|w| w[1] == 0x3F).count()
}
