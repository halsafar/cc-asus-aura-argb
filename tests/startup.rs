// SPDX-License-Identifier: GPL-3.0-or-later
//
// Startup path without hardware: discovery in a fake sysfs tree, then the startup
// reads against a mock bus that answers like the tested board (and also shows
// replies meant for another process on the same node).

mod common;

use cc_asus_aura_argb::discovery::{self, fake::FakeSysfs};
use cc_asus_aura_argb::hid::QueryPolicy;
use cc_asus_aura_argb::hid::mock::{MockBus, Op, report};
use cc_asus_aura_argb::{build_device, detect_devices, probe};
use common::Z890_TABLE;
use std::time::{Duration, Instant};

const FAST: QueryPolicy = QueryPolicy {
    attempts: 3,
    timeout: Duration::from_millis(1),
};

fn firmware_reply() -> cc_asus_aura_argb::protocol::Report {
    let mut r = report(&[0xEC, 0x02]);
    r[2..17].copy_from_slice(b"AULA3-AR32-0309");
    r
}

fn config_reply() -> cc_asus_aura_argb::protocol::Report {
    let mut r = report(&[0xEC, 0x30]);
    r[4..64].copy_from_slice(&Z890_TABLE);
    r
}

#[test]
fn probe_and_build_like_the_tested_board() {
    let dir = tempfile::tempdir().unwrap();
    let sys = FakeSysfs::new(dir.path());
    sys.aura_19af("3-2", 0);
    sys.dmi("ASUSTeK COMPUTER INC.", "TUF GAMING Z890-PLUS WIFI");
    let found = discovery::discover(sys.root()).unwrap();
    assert_eq!(found.len(), 1);

    let mut bus = MockBus::new();
    bus.respond(&[0xEC, 0x82], vec![firmware_reply()]);
    // The other process's firmware reply arrives before our config reply.
    bus.respond(&[0xEC, 0xB0], vec![firmware_reply(), config_reply()]);
    let r = probe(&mut bus, FAST).unwrap();
    assert_eq!(r.firmware.as_deref(), Some("AULA3-AR32-0309"));
    assert_eq!(r.table.0, Z890_TABLE);
    // Exactly two writes, both requests; nothing else is sent at startup.
    assert_eq!(
        bus.writes_trimmed(),
        vec![vec![0xEC, 0x82], vec![0xEC, 0xB0]]
    );

    let board = discovery::board_name(sys.root());
    let dev = build_device(&found[0], board.as_deref(), &r, Box::new(bus.clone()));
    assert_eq!(dev.info.id, "aura-usb-3-2");
    assert_eq!(
        dev.info.model,
        "ASUSTeK COMPUTER INC. TUF GAMING Z890-PLUS WIFI: AURA LED Controller, firmware \
         AULA3-AR32-0309"
    );
    assert_eq!(dev.info.locations[0], "/dev/hidraw0");
    let ids: Vec<&str> = dev.channels.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["mainboard", "argb1", "argb2", "argb3"]);
}

#[test]
fn missing_firmware_is_not_fatal_but_missing_table_is() {
    let mut bus = MockBus::new();
    bus.respond(&[0xEC, 0xB0], vec![config_reply()]);
    let r = probe(&mut bus, FAST).unwrap();
    assert_eq!(r.firmware, None);
    // Three firmware attempts, then one config request.
    let writes = bus.writes_trimmed();
    assert_eq!(writes.len(), 4);

    let mut bus = MockBus::new();
    bus.respond(&[0xEC, 0x82], vec![firmware_reply()]);
    assert!(probe(&mut bus, FAST).is_err());
    let reads = bus.ops().iter().filter(|o| **o == Op::Read).count();
    assert!(reads >= 4, "bounded retries, then give up");
}

#[test]
fn no_controller_serves_nothing_after_a_bounded_wait() {
    let dir = tempfile::tempdir().unwrap();
    let sys = FakeSysfs::new(dir.path());
    let t = Instant::now();
    assert!(detect_devices(sys.root(), Duration::from_millis(600)).is_empty());
    assert!(t.elapsed() < Duration::from_secs(5));
    // No hidraw class at all: immediately empty.
    assert!(detect_devices(&dir.path().join("missing"), Duration::from_secs(60)).is_empty());
}
