//! hidraw access: locating the panel's two interfaces through sysfs and
//! exchanging raw reports with them.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::protocol::{FRAME_PACKET_SIZE, PRODUCT_ID, VENDOR_ID};

/// The largest report the panel takes, plus the report-ID byte hidraw expects.
const MAX_REPORT_SIZE: usize = FRAME_PACKET_SIZE + 1;

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("Thermaltake LCD {VENDOR_ID:04x}:{PRODUCT_ID:04x} not found (interface {0} missing)")]
    NotFound(u8),
    #[error("cannot open {path}: {source} (is the udev rule installed?)")]
    Open { path: PathBuf, source: io::Error },
    #[error("the LCD did not answer within {0:?}")]
    Timeout(Duration),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// One hidraw node belonging to the panel.
#[derive(Debug)]
pub struct HidrawNode {
    pub path: PathBuf,
    pub interface: u8,
}

fn read_hex(path: &Path) -> Option<u32> {
    u32::from_str_radix(fs::read_to_string(path).ok()?.trim(), 16).ok()
}

/// Walks up from the hidraw sysfs node to the USB interface directory, whose
/// parent is the USB device carrying idVendor/idProduct.
fn usb_identity(sys_hidraw: &Path) -> Option<(u32, u32, u8)> {
    let device = fs::canonicalize(sys_hidraw.join("device")).ok()?;
    let interface_dir = device
        .ancestors()
        .find(|dir| dir.join("bInterfaceNumber").exists())?;
    let usb_device = interface_dir.parent()?;
    Some((
        read_hex(&usb_device.join("idVendor"))?,
        read_hex(&usb_device.join("idProduct"))?,
        read_hex(&interface_dir.join("bInterfaceNumber"))? as u8,
    ))
}

/// Lists the panel's hidraw nodes without opening them.
///
/// # Errors
///
/// When `/sys/class/hidraw` cannot be listed.
pub fn discover() -> io::Result<Vec<HidrawNode>> {
    let mut nodes = Vec::new();
    for entry in fs::read_dir("/sys/class/hidraw")? {
        let entry = entry?;
        if let Some((vendor, product, interface)) = usb_identity(&entry.path())
            && vendor == u32::from(VENDOR_ID)
            && product == u32::from(PRODUCT_ID)
        {
            nodes.push(HidrawNode {
                path: Path::new("/dev").join(entry.file_name()),
                interface,
            });
        }
    }
    nodes.sort_by_key(|node| node.interface);
    Ok(nodes)
}

/// An open hidraw interface.
#[derive(Debug)]
pub struct Channel {
    file: File,
}

impl Channel {
    /// # Errors
    ///
    /// When `nodes` holds no such interface, or the node cannot be opened for
    /// reading and writing, which usually means the udev rule is missing.
    pub fn open(nodes: &[HidrawNode], interface: u8) -> Result<Self, DeviceError> {
        let node = nodes
            .iter()
            .find(|node| node.interface == interface)
            .ok_or(DeviceError::NotFound(interface))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node.path)
            .map_err(|source| DeviceError::Open {
                path: node.path.clone(),
                source,
            })?;
        Ok(Self { file })
    }

    /// Sends one output report. The panel uses no report IDs, so hidraw wants
    /// a leading zero byte in front of the payload.
    ///
    /// # Errors
    ///
    /// When `payload` is larger than one report, or the write fails, which is
    /// what unplugging the panel looks like.
    pub fn send(&self, payload: &[u8]) -> Result<(), DeviceError> {
        let mut buffer = [0u8; MAX_REPORT_SIZE];
        let report = buffer
            .get_mut(..=payload.len())
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        report[1..].copy_from_slice(payload);
        (&self.file).write_all(report)?;
        Ok(())
    }

    fn readable(&self, timeout: Duration) -> io::Result<bool> {
        let mut pollfd = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: pollfd is a valid, exclusively borrowed array of length 1.
        match unsafe { libc::poll(&raw mut pollfd, 1, millis) } {
            -1 => Err(io::Error::last_os_error()),
            0 => Ok(false),
            _ => Ok(true),
        }
    }

    /// Reads one input report and throws it away: the panel's replies are
    /// acknowledgements whose content nothing uses.
    fn skip_report(&self) -> io::Result<()> {
        let mut buffer = [0u8; MAX_REPORT_SIZE];
        (&self.file).read(&mut buffer).map(drop)
    }

    /// Waits for the panel to acknowledge the last report.
    ///
    /// # Errors
    ///
    /// When nothing arrives within `timeout`, or the read fails.
    pub fn wait_reply(&self, timeout: Duration) -> Result<(), DeviceError> {
        if !self.readable(timeout)? {
            return Err(DeviceError::Timeout(timeout));
        }
        Ok(self.skip_report()?)
    }

    /// Discards pending input reports so the next `wait_reply` sees a fresh one.
    ///
    /// # Errors
    ///
    /// When a read fails.
    pub fn drain(&self) -> Result<(), DeviceError> {
        while self.readable(Duration::ZERO)? {
            self.skip_report()?;
        }
        Ok(())
    }
}
