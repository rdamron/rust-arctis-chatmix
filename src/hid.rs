//! HID device discovery (sysfs uevent matching) and raw hidraw I/O.

use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

use crate::devices::{frame_command, DeviceSpec, SPECS, VENDOR_ID};
use crate::log;

fn hid_ids(spec: &DeviceSpec) -> Vec<String> {
    spec.product_ids
        .iter()
        .map(|p| format!("0003:{:08X}:{:08X}", VENDOR_ID, p))
        .collect()
}

/// Match a /sys/class/hidraw/*/device/uevent: HID_ID carries vendor/product,
/// HID_PHYS ends in "/input<usb interface number>".
fn uevent_matches(content: &str, ids: &[String], phys_suffix: &str) -> bool {
    let mut id_match = false;
    let mut phys_match = false;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("HID_ID=") {
            id_match = ids.iter().any(|id| v == id);
        } else if let Some(v) = line.strip_prefix("HID_PHYS=") {
            phys_match = v.ends_with(phys_suffix);
        }
    }
    id_match && phys_match
}

/// A discovered device: the hidraw node commands are written to, plus any
/// extra read-only nodes dial events may arrive on (some models split
/// status and dial reports across USB interfaces).
pub(crate) struct FoundDevice {
    pub(crate) spec: &'static DeviceSpec,
    pub(crate) command: PathBuf,
    pub(crate) extra_listen: Vec<PathBuf>,
}

pub(crate) fn find_hidraw() -> Option<FoundDevice> {
    let entries: Vec<(String, String)> = fs::read_dir("/sys/class/hidraw")
        .ok()?
        .flatten()
        .filter_map(|e| {
            let content = fs::read_to_string(e.path().join("device/uevent")).ok()?;
            Some((e.file_name().to_string_lossy().into_owned(), content))
        })
        .collect();

    for (name, content) in &entries {
        for spec in SPECS {
            let ids = hid_ids(spec);
            if !uevent_matches(content, &ids, &format!("/input{}", spec.command_interface)) {
                continue;
            }
            // Sibling interfaces of the *same physical device*: same
            // HID_PHYS up to the trailing /input<N>.
            let phys_base = content
                .lines()
                .find_map(|l| l.strip_prefix("HID_PHYS="))
                .and_then(|p| p.rsplit_once("/input"))
                .map(|(base, _)| base.to_string())
                .unwrap_or_default();
            let extra_listen = spec
                .extra_listen_interfaces
                .iter()
                .filter_map(|iface| {
                    let suffix = format!("{phys_base}/input{iface}");
                    entries
                        .iter()
                        .find(|(n, c)| {
                            n != name
                                && uevent_matches(c, &ids, &suffix)
                        })
                        .map(|(n, _)| PathBuf::from("/dev").join(n))
                })
                .collect();
            return Some(FoundDevice {
                spec,
                command: PathBuf::from("/dev").join(name),
                extra_listen,
            });
        }
    }
    None
}

/// Write a command with the spec's report framing. Some devices' framing is
/// untested: if the kernel rejects the report id layout (EINVAL), flip
/// between numbered/unnumbered framing once and remember what worked.
pub(crate) fn write_command(dev: &mut File, cmd: &[u8], unnumbered: &mut bool) -> std::io::Result<()> {
    match dev.write(&frame_command(cmd, *unnumbered)) {
        Ok(_) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
            *unnumbered = !*unnumbered;
            log(&format!(
                "hidraw rejected write, switching to {} report framing",
                if *unnumbered { "unnumbered" } else { "numbered" }
            ));
            dev.write(&frame_command(cmd, *unnumbered)).map(|_| ())
        }
        Err(e) => Err(e),
    }
}

/// True if the fd has data or an error condition pending (either way, the next
/// read() will not block: it returns a report or the disconnect error).
pub(crate) fn poll_ready(fd: i32, timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut pfd, 1, timeout_ms) > 0 && pfd.revents != 0 }
}

/// True if any of the fds has data or an error pending.
pub(crate) fn poll_any(fds: &[i32], timeout_ms: i32) -> bool {
    let mut pfds: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, timeout_ms) > 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{self, NOVA_PRO_WIRELESS};

    // Captured live from an Arctis Nova Pro Wireless base station.
    const REAL_UEVENT: &str = "DRIVER=hid-generic\n\
        HID_ID=0003:00001038:000012E0\n\
        HID_NAME=SteelSeries Arctis Nova Pro Wireless\n\
        HID_PHYS=usb-0000:80:14.0-10.1/input4\n\
        HID_UNIQ=\n\
        MODALIAS=hid:b0003g0001v00001038p000012E0";

    #[test]
    fn uevent_matches_real_device() {
        assert!(uevent_matches(REAL_UEVENT, &hid_ids(&NOVA_PRO_WIRELESS), "/input4"));
    }

    #[test]
    fn uevent_rejects_wrong_interface() {
        assert!(!uevent_matches(REAL_UEVENT, &hid_ids(&NOVA_PRO_WIRELESS), "/input3"));
    }

    #[test]
    fn uevent_rejects_other_device() {
        let other = REAL_UEVENT.replace("1038", "1532");
        assert!(!uevent_matches(&other, &hid_ids(&NOVA_PRO_WIRELESS), "/input4"));
    }

    #[test]
    fn uevent_matches_only_its_own_spec() {
        // A Nova 7 on interface 3 must not match the Nova Pro spec and
        // vice versa.
        let nova7 = REAL_UEVENT
            .replace("12E0", "22A1")
            .replace("input4", "input3");
        assert!(!uevent_matches(&nova7, &hid_ids(&NOVA_PRO_WIRELESS), "/input4"));
        assert!(uevent_matches(
            &nova7,
            &hid_ids(&devices::NOVA_7_PERCENT),
            "/input3"
        ));
    }
}
