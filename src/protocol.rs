//! Wire format of the Thermaltake 3.9" bar LCD (264a:233d).
//!
//! The panel exposes two HID interfaces: interface 0 takes 440-byte command
//! reports, interface 1 takes 1024-byte frame reports carrying a chunked JPEG.

pub const VENDOR_ID: u16 = 0x264a;
pub const PRODUCT_ID: u16 = 0x233d;

pub const WIDTH: u32 = 480;
pub const HEIGHT: u32 = 128;

pub const COMMAND_INTERFACE: u8 = 0;
pub const FRAME_INTERFACE: u8 = 1;

pub const COMMAND_PACKET_SIZE: usize = 440;
pub const FRAME_PACKET_SIZE: usize = 1024;
pub const FRAME_HEADER_SIZE: usize = 4;
pub const FRAME_DATA_SIZE: usize = FRAME_PACKET_SIZE - FRAME_HEADER_SIZE;
/// The packet count travels in a single byte.
pub const MAX_FRAME_PACKETS: usize = 255;
pub const MAX_JPEG_SIZE: usize = MAX_FRAME_PACKETS * FRAME_DATA_SIZE;

/// Opcodes sent, in order, to wake the panel up. Each one is acknowledged.
pub const HANDSHAKE_OPCODES: [u8; 6] = [0x85, 0x87, 0x85, 0x87, 0x84, 0x81];
const HEARTBEAT_OPCODE: u8 = 0x82;
const BRIGHTNESS_OPCODE: u8 = 0x12;
const FRAME_OPCODE: u8 = 0x08;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("frame is not a complete JPEG stream")]
    NotJpeg,
    #[error("JPEG is {0} bytes, the panel accepts at most {MAX_JPEG_SIZE}")]
    TooLarge(usize),
}

fn command(prefix: &[u8]) -> [u8; COMMAND_PACKET_SIZE] {
    let mut packet = [0u8; COMMAND_PACKET_SIZE];
    packet[..prefix.len()].copy_from_slice(prefix);
    packet
}

#[must_use]
pub fn handshake(opcode: u8) -> [u8; COMMAND_PACKET_SIZE] {
    command(&[opcode, 0x01, 0x00, 0x80])
}

#[must_use]
pub fn heartbeat() -> [u8; COMMAND_PACKET_SIZE] {
    command(&[HEARTBEAT_OPCODE, 0x01, 0x00, 0x80])
}

/// `percent` is clamped to 0..=100.
#[must_use]
pub fn brightness(percent: u8) -> [u8; COMMAND_PACKET_SIZE] {
    command(&[BRIGHTNESS_OPCODE, 0x01, 0x00, 0x80, percent.min(100)])
}

/// Splits a JPEG into frame reports, built one at a time as they are sent.
/// The first header carries the total packet count and the 0x80 start flag,
/// the following ones their index.
///
/// # Errors
///
/// When `jpeg` is not a complete JPEG stream, or is too large to address with
/// a one-byte packet count.
pub fn frame_packets(
    jpeg: &[u8],
) -> Result<impl Iterator<Item = [u8; FRAME_PACKET_SIZE]> + '_, FrameError> {
    if !jpeg.starts_with(&[0xff, 0xd8]) || !jpeg.ends_with(&[0xff, 0xd9]) {
        return Err(FrameError::NotJpeg);
    }
    if jpeg.len() > MAX_JPEG_SIZE {
        return Err(FrameError::TooLarge(jpeg.len()));
    }

    // At most MAX_FRAME_PACKETS, so the count and the indexes fit in a byte.
    let count = jpeg.len().div_ceil(FRAME_DATA_SIZE) as u8;
    Ok(jpeg
        .chunks(FRAME_DATA_SIZE)
        .enumerate()
        .map(move |(index, chunk)| {
            let mut packet = [0u8; FRAME_PACKET_SIZE];
            packet[0] = FRAME_OPCODE;
            if index == 0 {
                packet[1] = count;
                packet[3] = 0x80;
            } else {
                packet[1] = index as u8;
            }
            packet[FRAME_HEADER_SIZE..FRAME_HEADER_SIZE + chunk.len()].copy_from_slice(chunk);
            packet
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_jpeg(len: usize) -> Vec<u8> {
        let mut data = vec![0x11; len];
        data[..2].copy_from_slice(&[0xff, 0xd8]);
        data[len - 2..].copy_from_slice(&[0xff, 0xd9]);
        data
    }

    #[test]
    fn commands_are_zero_padded() {
        let packet = handshake(0x85);
        assert_eq!(packet[..4], [0x85, 0x01, 0x00, 0x80]);
        assert!(packet[4..].iter().all(|&b| b == 0));
        assert_eq!(heartbeat()[..4], [0x82, 0x01, 0x00, 0x80]);
    }

    #[test]
    fn brightness_is_clamped() {
        assert_eq!(brightness(40)[..5], [0x12, 0x01, 0x00, 0x80, 40]);
        assert_eq!(brightness(250)[4], 100);
    }

    fn packets(jpeg: &[u8]) -> Vec<[u8; FRAME_PACKET_SIZE]> {
        frame_packets(jpeg).unwrap().collect()
    }

    #[test]
    fn frame_headers() {
        let jpeg = fake_jpeg(FRAME_DATA_SIZE * 2 + 10);
        let packets = packets(&jpeg);
        assert_eq!(packets.len(), 3);
        assert_eq!(packets[0][..4], [0x08, 3, 0x00, 0x80]);
        assert_eq!(packets[1][..4], [0x08, 1, 0x00, 0x00]);
        assert_eq!(packets[2][..4], [0x08, 2, 0x00, 0x00]);
    }

    #[test]
    fn frame_payload_roundtrip() {
        let jpeg = fake_jpeg(FRAME_DATA_SIZE + 7);
        let packets = packets(&jpeg);
        let mut payload: Vec<u8> = packets
            .iter()
            .flat_map(|p| p[FRAME_HEADER_SIZE..].iter().copied())
            .collect();
        payload.truncate(jpeg.len());
        assert_eq!(payload, jpeg);
        assert!(packets[1][FRAME_HEADER_SIZE + 7..].iter().all(|&b| b == 0));
    }

    #[test]
    fn exact_multiple_has_no_empty_packet() {
        assert_eq!(packets(&fake_jpeg(FRAME_DATA_SIZE * 2)).len(), 2);
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(frame_packets(&[0u8; 64]).err(), Some(FrameError::NotJpeg));
        let huge = fake_jpeg(MAX_JPEG_SIZE + 1);
        let too_large = FrameError::TooLarge(huge.len());
        assert_eq!(frame_packets(&huge).err(), Some(too_large));
        assert_eq!(packets(&fake_jpeg(MAX_JPEG_SIZE)).len(), MAX_FRAME_PACKETS);
    }
}
