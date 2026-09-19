//! High-level panel API: handshake, brightness, heartbeat and frame upload.

use std::io::Cursor;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, RgbImage};

use crate::device::{self, Channel, DeviceError};
use crate::protocol::{self, FrameError, HEIGHT, WIDTH};

const REPLY_TIMEOUT: Duration = Duration::from_secs(2);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const JPEG_QUALITY: u8 = 90;

#[derive(Debug, thiserror::Error)]
pub enum LcdError {
    #[error(transparent)]
    Device(#[from] DeviceError),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("JPEG encoding failed: {0}")]
    Encode(#[from] image::ImageError),
}

struct Heartbeat {
    stop: Sender<()>,
    thread: JoinHandle<()>,
}

pub struct Lcd {
    command: Arc<Mutex<Channel>>,
    frame: Channel,
    heartbeat: Option<Heartbeat>,
}

impl Lcd {
    /// Opens both interfaces and runs the wake-up handshake.
    ///
    /// # Errors
    ///
    /// When the panel is absent, its hidraw nodes cannot be opened, or it does
    /// not acknowledge the handshake.
    pub fn open() -> Result<Self, LcdError> {
        let nodes = device::discover().map_err(DeviceError::from)?;
        let lcd = Self {
            command: Arc::new(Mutex::new(Channel::open(
                &nodes,
                protocol::COMMAND_INTERFACE,
            )?)),
            frame: Channel::open(&nodes, protocol::FRAME_INTERFACE)?,
            heartbeat: None,
        };
        lcd.handshake()?;
        Ok(lcd)
    }

    fn handshake(&self) -> Result<(), LcdError> {
        self.frame.drain()?;
        let command = self.command.lock().unwrap();
        command.drain()?;
        for opcode in protocol::HANDSHAKE_OPCODES {
            command.send(&protocol::handshake(opcode))?;
            command.wait_reply(REPLY_TIMEOUT)?;
        }
        command.send(&protocol::brightness(100))?;
        Ok(())
    }

    /// # Errors
    ///
    /// When the command cannot be sent to the panel.
    ///
    /// # Panics
    ///
    /// When another thread panicked while holding the command interface.
    pub fn set_brightness(&self, percent: u8) -> Result<(), LcdError> {
        let command = self.command.lock().unwrap();
        Ok(command.send(&protocol::brightness(percent))?)
    }

    /// Keeps the panel awake from a background thread until `self` is dropped.
    ///
    /// # Panics
    ///
    /// When another thread panicked while holding the command interface.
    pub fn start_heartbeat(&mut self) {
        if self.heartbeat.is_some() {
            return;
        }
        let (stop, stopped) = mpsc::channel();
        let command = Arc::clone(&self.command);
        let thread = thread::spawn(move || {
            while stopped.recv_timeout(HEARTBEAT_INTERVAL) == Err(RecvTimeoutError::Timeout) {
                let command = command.lock().unwrap();
                // Errors surface on the next frame upload, which the caller sees.
                if command.send(&protocol::heartbeat()).is_err() {
                    return;
                }
                let _ = command.wait_reply(REPLY_TIMEOUT);
            }
        });
        self.heartbeat = Some(Heartbeat { stop, thread });
    }

    /// Uploads an already encoded baseline JPEG and waits for the panel's ack.
    ///
    /// # Errors
    ///
    /// When `jpeg` is not a frame the panel accepts, an upload write fails, or
    /// no acknowledgement arrives.
    pub fn send_jpeg(&self, jpeg: &[u8]) -> Result<(), LcdError> {
        let packets = protocol::frame_packets(jpeg)?;
        self.frame.drain()?;
        for packet in packets {
            self.frame.send(&packet)?;
        }
        self.frame.wait_reply(REPLY_TIMEOUT)?;
        Ok(())
    }

    /// Uploads a frame that is already 480x128.
    ///
    /// # Errors
    ///
    /// When the frame cannot be encoded as JPEG, or the upload fails.
    pub fn send_frame(&self, frame: &RgbImage) -> Result<(), LcdError> {
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(Cursor::new(&mut jpeg), JPEG_QUALITY).encode_image(frame)?;
        self.send_jpeg(&jpeg)
    }

    /// Scales any image to cover the panel, cropping what overflows.
    ///
    /// # Errors
    ///
    /// When the scaled frame cannot be encoded as JPEG, or the upload fails.
    pub fn send_image(&self, image: &DynamicImage) -> Result<(), LcdError> {
        let fitted = image.resize_to_fill(WIDTH, HEIGHT, FilterType::Lanczos3);
        self.send_frame(&fitted.to_rgb8())
    }
}

impl Drop for Lcd {
    fn drop(&mut self) {
        if let Some(heartbeat) = self.heartbeat.take() {
            drop(heartbeat.stop);
            let _ = heartbeat.thread.join();
        }
    }
}
