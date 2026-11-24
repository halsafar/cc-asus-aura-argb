// SPDX-License-Identifier: GPL-3.0-or-later
//
// Finds the controller's hidraw node through sysfs. Only sysfs is read here; no
// device node is opened.
//
// hidraw numbers are not stable across boots, so the node is found by USB IDs:
//
//   <sysfs>/class/hidraw/hidrawN/device -> .../3-2/3-2:1.2/0003:0B05:19AF.0004
//
// The HID device directory name carries bus type and USB vendor/product. Its parent
// is the USB interface, whose parent is the USB device (`idVendor`, `idProduct`,
// `bcdDevice`). The HID report descriptor (sysfs `report_descriptor`, a kernel
// cache) must be the lighting interface's, so other interfaces of the same USB
// device are never used.

use crate::descriptor;
use log::debug;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const DEFAULT_SYSFS_ROOT: &str = "/sys";

/// HID bus type of USB devices, as in the HID device directory name.
const BUS_USB: u16 = 0x0003;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SupportedController {
    pub vendor: u16,
    pub product: u16,
    pub name: &'static str,
}

/// Controllers this plugin talks to. Other Aura USB product ids exist (liquidctl
/// also lists 0x1939 and 0x18f3) but have not been checked against this code.
pub const SUPPORTED: &[SupportedController] = &[SupportedController {
    vendor: 0x0B05,
    product: 0x19AF,
    name: "ASUS Aura LED Controller",
}];

/// Identifies one controller: USB IDs and the USB port path (`3-2`). Keys the
/// saved-state record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControllerKey {
    pub vendor: u16,
    pub product: u16,
    pub usb_name: String,
}

impl fmt::Display for ControllerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04x}:{:04x} at usb {}",
            self.vendor, self.product, self.usb_name
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundController {
    pub model: &'static SupportedController,
    /// `hidrawN`
    pub hidraw: String,
    /// USB device name in sysfs, e.g. `3-2` (bus 3, port 2).
    pub usb_name: String,
    /// Resolved sysfs directory of the USB device.
    pub usb_path: PathBuf,
    pub interface: Option<u8>,
    pub bcd_device: Option<String>,
    pub manufacturer: Option<String>,
    pub product_string: Option<String>,
}

impl FoundController {
    pub fn dev_path(&self) -> PathBuf {
        PathBuf::from("/dev").join(&self.hidraw)
    }

    pub fn key(&self) -> ControllerKey {
        ControllerKey {
            vendor: self.model.vendor,
            product: self.model.product,
            usb_name: self.usb_name.clone(),
        }
    }
}

/// Parses a HID device directory name `BBBB:VVVV:PPPP.NNNN`.
fn parse_hid_name(name: &str) -> Option<(u16, u16, u16)> {
    let (ids, _) = name.split_once('.')?;
    let mut it = ids.split(':');
    let mut next = || u16::from_str_radix(it.next()?, 16).ok();
    let ids = (next()?, next()?, next()?);
    Some(ids)
}

fn read_trimmed(p: &Path) -> Option<String> {
    fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_hex16(p: &Path) -> Option<u16> {
    let s = read_trimmed(p)?;
    u16::from_str_radix(s.strip_prefix("0x").unwrap_or(&s), 16).ok()
}

/// Lists every supported controller's lighting interface under `sysfs_root`, sorted
/// by USB name. Empty when none is present.
pub fn discover(sysfs_root: &Path) -> io::Result<Vec<FoundController>> {
    let class_dir = sysfs_root.join("class/hidraw");
    let root = fs::canonicalize(sysfs_root)?;
    let mut found = Vec::new();
    for entry in fs::read_dir(&class_dir)? {
        let entry = entry?;
        let hidraw = entry.file_name().to_string_lossy().into_owned();
        if !hidraw.starts_with("hidraw") {
            continue;
        }
        if let Some(f) = inspect(&root, &entry.path(), &hidraw) {
            found.push(f);
        }
    }
    found.sort_by(|a, b| (&a.usb_name, &a.hidraw).cmp(&(&b.usb_name, &b.hidraw)));
    Ok(found)
}

fn inspect(root: &Path, class_entry: &Path, hidraw: &str) -> Option<FoundController> {
    let hid_dir = fs::canonicalize(class_entry.join("device")).ok()?;
    if !hid_dir.starts_with(root) {
        return None;
    }
    let hid_name = hid_dir.file_name()?.to_string_lossy().into_owned();
    let (bus, vendor, product) = parse_hid_name(&hid_name)?;
    let model = SUPPORTED
        .iter()
        .find(|c| bus == BUS_USB && c.vendor == vendor && c.product == product)?;

    let desc = match fs::read(hid_dir.join("report_descriptor")) {
        Ok(d) => d,
        Err(e) => {
            debug!("{hidraw}: cannot read report descriptor: {e}");
            return None;
        }
    };
    if let Err(e) = descriptor::check_lighting_interface(&desc) {
        debug!("{hidraw} ({hid_name}): not the lighting interface: {e}");
        return None;
    }

    let interface_dir = hid_dir.parent()?;
    let usb_dir = interface_dir.parent()?;
    if !usb_dir.starts_with(root) || usb_dir == root {
        return None;
    }
    if read_hex16(&usb_dir.join("idVendor")) != Some(vendor)
        || read_hex16(&usb_dir.join("idProduct")) != Some(product)
    {
        debug!("{hidraw}: USB device above {hid_name} does not match its ids");
        return None;
    }
    let usb_name = usb_dir.file_name()?.to_string_lossy().into_owned();
    debug!(
        "{hidraw} is the lighting interface of {} at usb {usb_name}",
        model.name
    );
    Some(FoundController {
        model,
        hidraw: hidraw.to_string(),
        usb_name,
        usb_path: usb_dir.to_path_buf(),
        interface: read_trimmed(&interface_dir.join("bInterfaceNumber"))
            .and_then(|s| u8::from_str_radix(&s, 16).ok()),
        bcd_device: read_trimmed(&usb_dir.join("bcdDevice")),
        manufacturer: read_trimmed(&usb_dir.join("manufacturer")),
        product_string: read_trimmed(&usb_dir.join("product")),
    })
}

/// Finds the hidraw node of the controller with `key` again (after re-enumeration).
pub fn locate(sysfs_root: &Path, key: &ControllerKey) -> io::Result<PathBuf> {
    discover(sysfs_root)?
        .into_iter()
        .find(|f| f.key() == *key)
        .map(|f| f.dev_path())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("controller {key} not found in sysfs"),
            )
        })
}

/// Mainboard vendor and name from DMI, e.g. `ASUSTeK COMPUTER INC. TUF GAMING ...`.
pub fn board_name(sysfs_root: &Path) -> Option<String> {
    let dmi = sysfs_root.join("class/dmi/id");
    let name = read_trimmed(&dmi.join("board_name"))?;
    Some(match read_trimmed(&dmi.join("board_vendor")) {
        Some(v) => format!("{v} {name}"),
        None => name,
    })
}

/// Fake sysfs trees for tests (also used by the integration tests).
#[doc(hidden)]
pub mod fake {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};

    /// The 36-byte report descriptor of interface 2 of 0b05:19af.
    pub const AURA_19AF_DESCRIPTOR: [u8; 36] = [
        0x06, 0x72, 0xff, 0x09, 0xa1, 0xa1, 0x01, 0x85, 0xec, 0x09, 0x10, 0x15, 0x00, 0x26, 0xff,
        0x00, 0x75, 0x08, 0x95, 0x40, 0x81, 0x02, 0x09, 0x11, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75,
        0x08, 0x95, 0x40, 0x91, 0x02, 0xc0,
    ];

    /// A sysfs tree under `root` (normally a temporary directory).
    pub struct FakeSysfs {
        root: PathBuf,
    }

    impl FakeSysfs {
        pub fn new(root: &Path) -> Self {
            fs::create_dir_all(root.join("class/hidraw")).unwrap();
            FakeSysfs {
                root: root.to_path_buf(),
            }
        }

        pub fn root(&self) -> &Path {
            &self.root
        }

        /// A USB device at `devices/pci0000:00/0000:00:14.0/<usb_bus>/<usb_name>`.
        pub fn usb_device(&self, usb_name: &str, vid: u16, pid: u16) -> PathBuf {
            let bus = format!("usb{}", usb_name.split('-').next().unwrap());
            let d = self
                .root()
                .join("devices/pci0000:00/0000:00:14.0")
                .join(bus)
                .join(usb_name);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("idVendor"), format!("{vid:04x}\n")).unwrap();
            fs::write(d.join("idProduct"), format!("{pid:04x}\n")).unwrap();
            fs::write(d.join("bcdDevice"), "0100\n").unwrap();
            fs::write(d.join("manufacturer"), "AsusTek Computer Inc.\n").unwrap();
            fs::write(d.join("product"), "AURA LED Controller\n").unwrap();
            d
        }

        /// A HID interface `iface` of `usb_dir` with hidraw node `hidrawN`.
        pub fn hid_interface(
            &self,
            usb_dir: &Path,
            iface: u8,
            ids: (u16, u16, u16),
            hidraw_n: u32,
            descriptor: &[u8],
        ) {
            let usb_name = usb_dir.file_name().unwrap().to_string_lossy().into_owned();
            let iface_dir = usb_dir.join(format!("{usb_name}:1.{iface}"));
            fs::create_dir_all(&iface_dir).unwrap();
            fs::write(iface_dir.join("bInterfaceNumber"), format!("{iface:02x}\n")).unwrap();
            let hid_dir = iface_dir.join(format!(
                "{:04X}:{:04X}:{:04X}.{:04X}",
                ids.0,
                ids.1,
                ids.2,
                hidraw_n + 1
            ));
            let node = hid_dir.join("hidraw").join(format!("hidraw{hidraw_n}"));
            fs::create_dir_all(&node).unwrap();
            fs::write(hid_dir.join("report_descriptor"), descriptor).unwrap();
            symlink("../..", node.join("device")).unwrap();
            let rel = node.strip_prefix(self.root()).unwrap();
            symlink(
                PathBuf::from("../..").join(rel),
                self.root().join(format!("class/hidraw/hidraw{hidraw_n}")),
            )
            .unwrap();
        }

        /// The controller as on the reference machine: 3-2, interface 2, hidraw0.
        pub fn aura_19af(&self, usb_name: &str, hidraw_n: u32) {
            let usb = self.usb_device(usb_name, 0x0b05, 0x19af);
            self.hid_interface(
                &usb,
                2,
                (3, 0x0b05, 0x19af),
                hidraw_n,
                &AURA_19AF_DESCRIPTOR,
            );
        }

        pub fn dmi(&self, vendor: &str, name: &str) {
            let d = self.root().join("class/dmi/id");
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("board_vendor"), format!("{vendor}\n")).unwrap();
            fs::write(d.join("board_name"), format!("{name}\n")).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{AURA_19AF_DESCRIPTOR, FakeSysfs};
    use super::*;

    fn tree() -> (tempfile::TempDir, FakeSysfs) {
        let dir = tempfile::tempdir().unwrap();
        let fs_ = FakeSysfs::new(dir.path());
        (dir, fs_)
    }

    #[test]
    fn descriptor_constant_matches() {
        assert_eq!(AURA_19AF_DESCRIPTOR, crate::descriptor::tests::AURA_19AF);
    }

    #[test]
    fn finds_the_lighting_interface() {
        let (_d, fs_) = tree();
        fs_.aura_19af("3-2", 0);
        // A keyboard elsewhere.
        let kbd = fs_.usb_device("1-4", 0x046d, 0xc31c);
        fs_.hid_interface(&kbd, 0, (3, 0x046d, 0xc31c), 1, &[0x05, 0x01, 0xc0]);
        let found = discover(fs_.root()).unwrap();
        assert_eq!(found.len(), 1);
        let f = &found[0];
        assert_eq!(f.hidraw, "hidraw0");
        assert_eq!(f.dev_path(), PathBuf::from("/dev/hidraw0"));
        assert_eq!(f.usb_name, "3-2");
        assert_eq!(f.interface, Some(2));
        assert_eq!(f.bcd_device.as_deref(), Some("0100"));
        assert_eq!(f.product_string.as_deref(), Some("AURA LED Controller"));
        assert_eq!(f.model.product, 0x19af);
        assert_eq!(f.key().to_string(), "0b05:19af at usb 3-2");
        assert_eq!(locate(fs_.root(), &f.key()).unwrap(), f.dev_path());
    }

    #[test]
    fn skips_other_interfaces_of_the_same_device() {
        let (_d, fs_) = tree();
        let usb = fs_.usb_device("3-2", 0x0b05, 0x19af);
        // Same USB ids, but a different descriptor: not the lighting interface.
        fs_.hid_interface(&usb, 0, (3, 0x0b05, 0x19af), 4, &[0x05, 0x01, 0xc0]);
        assert!(discover(fs_.root()).unwrap().is_empty());
        fs_.hid_interface(&usb, 2, (3, 0x0b05, 0x19af), 7, &AURA_19AF_DESCRIPTOR);
        let found = discover(fs_.root()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].hidraw, "hidraw7");
    }

    #[test]
    fn rejects_mismatches() {
        let (_d, fs_) = tree();
        // Another ASUS product id.
        let a = fs_.usb_device("3-3", 0x0b05, 0x1939);
        fs_.hid_interface(&a, 2, (3, 0x0b05, 0x1939), 0, &AURA_19AF_DESCRIPTOR);
        // Right ids on a non-USB bus (Bluetooth = 0005).
        let b = fs_.usb_device("3-4", 0x0b05, 0x19af);
        fs_.hid_interface(&b, 2, (5, 0x0b05, 0x19af), 1, &AURA_19AF_DESCRIPTOR);
        // HID name says 19af but the USB device above says otherwise.
        let c = fs_.usb_device("3-5", 0x0b05, 0x1234);
        fs_.hid_interface(&c, 2, (3, 0x0b05, 0x19af), 2, &AURA_19AF_DESCRIPTOR);
        assert!(discover(fs_.root()).unwrap().is_empty());
        assert!(
            locate(
                fs_.root(),
                &ControllerKey {
                    vendor: 0x0b05,
                    product: 0x19af,
                    usb_name: "3-2".into()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn several_sorted_and_relocated() {
        let (_d, fs_) = tree();
        fs_.aura_19af("5-1", 2);
        fs_.aura_19af("3-2", 9);
        let got: Vec<(String, String)> = discover(fs_.root())
            .unwrap()
            .into_iter()
            .map(|f| (f.usb_name, f.hidraw))
            .collect();
        assert_eq!(
            got,
            vec![
                ("3-2".to_string(), "hidraw9".to_string()),
                ("5-1".to_string(), "hidraw2".to_string())
            ]
        );
    }

    #[test]
    fn empty_and_missing_sysfs() {
        let (_d, fs_) = tree();
        assert!(discover(fs_.root()).unwrap().is_empty());
        let dir = tempfile::tempdir().unwrap();
        assert!(discover(&dir.path().join("nope")).is_err());
    }

    #[test]
    fn board() {
        let (_d, fs_) = tree();
        assert_eq!(board_name(fs_.root()), None);
        fs_.dmi("ASUSTeK COMPUTER INC.", "TUF GAMING Z890-PLUS WIFI");
        assert_eq!(
            board_name(fs_.root()).as_deref(),
            Some("ASUSTeK COMPUTER INC. TUF GAMING Z890-PLUS WIFI")
        );
    }

    #[test]
    fn hid_names() {
        assert_eq!(
            parse_hid_name("0003:0B05:19AF.0004"),
            Some((3, 0x0b05, 0x19af))
        );
        assert_eq!(parse_hid_name("0003:0B05"), None);
        assert_eq!(parse_hid_name("hidraw0"), None);
    }
}
