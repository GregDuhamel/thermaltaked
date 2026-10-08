//! The panel's two hidraw interfaces: finding them and exchanging raw reports
//! with them.
//!
//! The transport is the [`hidraw`] crate: it lists the nodes through sysfs,
//! sends output reports, waits for input with a timeout, and tells an
//! unplugged device from a transfer that merely failed. What is here is the
//! panel's side of it: which nodes are its two interfaces, the report-ID byte
//! its reports start with, and that its replies are acknowledgements whose
//! content nothing reads.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use hidraw::{Bus, Device, Filter};

use crate::protocol::{FRAME_PACKET_SIZE, PRODUCT_ID, VENDOR_ID};

/// The largest report the panel takes, plus the report-ID byte hidraw expects.
const MAX_REPORT_SIZE: usize = FRAME_PACKET_SIZE + 1;

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("Thermaltake LCD {VENDOR_ID:04x}:{PRODUCT_ID:04x} not found (interface {0} missing)")]
    NotFound(u8),
    // The source is left out of the message: callers print the whole chain.
    #[error("cannot open {path} (is the udev rule installed?)")]
    Open { path: PathBuf, source: io::Error },
    #[error("the LCD did not answer within {0:?}")]
    Timeout(Duration),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl DeviceError {
    /// True when the panel is gone for good - unplugged, or its node taken
    /// away - rather than slow to answer or busy, which is worth another try.
    #[must_use]
    pub fn is_gone(&self) -> bool {
        match self {
            Self::Open { source, .. } | Self::Io(source) => hidraw::is_gone(source),
            Self::NotFound(_) | Self::Timeout(_) => false,
        }
    }
}

/// One hidraw node belonging to the panel.
///
/// The panel is a USB device, so every node of its has an interface number;
/// [`hidraw::Node`] carries it as an option, and this is the same node with
/// that settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidrawNode {
    pub path: PathBuf,
    pub interface: u8,
}

/// Lists the panel's hidraw nodes without opening them, in interface order.
///
/// The panel is looked for on USB only: the same vendor and product IDs on
/// any other bus would be something else wearing them.
///
/// # Errors
///
/// When `/sys/class/hidraw` cannot be listed.
pub fn discover() -> io::Result<Vec<HidrawNode>> {
    let filter = Filter::new()
        .bus(Bus::Usb)
        .vendor(VENDOR_ID)
        .product(PRODUCT_ID);
    let mut nodes: Vec<HidrawNode> = hidraw::discover(&filter)?
        .into_iter()
        .filter_map(|node| {
            Some(HidrawNode {
                interface: node.interface?,
                path: node.path,
            })
        })
        .collect();
    nodes.sort_by_key(|node| node.interface);
    Ok(nodes)
}

/// An open hidraw interface.
#[derive(Debug)]
pub struct Channel {
    device: Device,
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
        let device = Device::open(&node.path).map_err(|source| DeviceError::Open {
            path: node.path.clone(),
            source,
        })?;
        Ok(Self { device })
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
        Ok(self.device.write(report)?)
    }

    /// Waits for the panel to acknowledge the last report, and throws the
    /// acknowledgement away: its content is not used for anything.
    ///
    /// # Errors
    ///
    /// When nothing arrives within `timeout`, or the read fails.
    pub fn wait_reply(&self, timeout: Duration) -> Result<(), DeviceError> {
        let mut buffer = [0u8; MAX_REPORT_SIZE];
        match self.device.read_timeout(&mut buffer, timeout)? {
            Some(_) => Ok(()),
            None => Err(DeviceError::Timeout(timeout)),
        }
    }

    /// Discards pending input reports so the next `wait_reply` sees a fresh one.
    ///
    /// # Errors
    ///
    /// When a read fails.
    pub fn drain(&self) -> Result<(), DeviceError> {
        self.device.drain()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixDatagram;
    use std::path::Path;

    use super::*;

    fn nodes() -> Vec<HidrawNode> {
        vec![
            HidrawNode {
                path: PathBuf::from("/nonexistent/hidraw5"),
                interface: 1,
            },
            HidrawNode {
                path: PathBuf::from("/nonexistent/hidraw4"),
                interface: 0,
            },
        ]
    }

    #[test]
    fn a_channel_is_picked_by_interface() {
        // The right node is chosen before it is opened: the error names the
        // path of the interface asked for, not the first in the list.
        match Channel::open(&nodes(), 0) {
            Err(DeviceError::Open { path, source }) => {
                assert_eq!(path, Path::new("/nonexistent/hidraw4"));
                assert_eq!(source.kind(), io::ErrorKind::NotFound);
            }
            other => panic!("expected an open error, got {other:?}"),
        }
        assert!(matches!(
            Channel::open(&nodes(), 2),
            Err(DeviceError::NotFound(2))
        ));
        assert!(matches!(
            Channel::open(&[], 0),
            Err(DeviceError::NotFound(0))
        ));
    }

    /// A datagram socket pair stands in for the node: like hidraw, it
    /// delivers one message per read.
    fn fake() -> (Channel, UnixDatagram) {
        let (node, peer) = UnixDatagram::pair().unwrap();
        let channel = Channel {
            device: Device::from_fd(node, "/dev/hidraw-fake"),
        };
        (channel, peer)
    }

    #[test]
    fn a_report_goes_out_behind_a_zero_report_id() {
        let (channel, peer) = fake();
        channel.send(&[0x85, 0x01, 0x00, 0x80]).unwrap();
        let mut received = [0u8; 8];
        assert_eq!(peer.recv(&mut received).unwrap(), 5);
        assert_eq!(received[..5], [0, 0x85, 0x01, 0x00, 0x80]);

        // One report at most; the panel's largest one still fits.
        channel.send(&[0; FRAME_PACKET_SIZE]).unwrap();
        let too_large = channel.send(&[0; FRAME_PACKET_SIZE + 1]).unwrap_err();
        assert!(
            matches!(too_large, DeviceError::Io(ref error) if error.kind() == io::ErrorKind::InvalidInput)
        );
    }

    #[test]
    fn a_reply_is_waited_for_and_thrown_away() {
        let (channel, peer) = fake();
        let timeout = Duration::from_millis(5);
        assert!(matches!(
            channel.wait_reply(timeout),
            Err(DeviceError::Timeout(elapsed)) if elapsed == timeout
        ));

        peer.send(&[0x85, 0x01]).unwrap();
        channel.wait_reply(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            channel.wait_reply(Duration::ZERO),
            Err(DeviceError::Timeout(_))
        ));

        // Whatever is queued is not the reply to what comes next.
        for _ in 0..3 {
            peer.send(&[0x82]).unwrap();
        }
        channel.drain().unwrap();
        assert!(matches!(
            channel.wait_reply(Duration::ZERO),
            Err(DeviceError::Timeout(_))
        ));
    }

    #[test]
    fn only_an_unplugged_panel_is_gone() {
        let enodev = rustix::io::Errno::NODEV.raw_os_error();
        assert!(DeviceError::Io(io::Error::from_raw_os_error(enodev)).is_gone());
        assert!(
            DeviceError::Open {
                path: PathBuf::from("/dev/hidraw4"),
                source: io::Error::from_raw_os_error(enodev),
            }
            .is_gone()
        );
        // Slow, busy or absent at startup: worth another try.
        assert!(!DeviceError::Timeout(Duration::from_secs(2)).is_gone());
        assert!(!DeviceError::NotFound(1).is_gone());
        assert!(!DeviceError::Io(io::Error::from(io::ErrorKind::PermissionDenied)).is_gone());
    }
}
