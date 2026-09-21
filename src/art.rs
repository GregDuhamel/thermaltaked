//! Cover art, fetched off the drawing path so a slow network never holds a
//! frame back.

use std::ffi::OsString;
use std::fs::File;
use std::io::{Cursor, Read};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use image::imageops::FilterType;
use image::{ImageReader, Limits, RgbImage};

const TIMEOUT: Duration = Duration::from_secs(10);
/// Covers are small; anything larger is not one.
const MAX_BYTES: u64 = 4 * 1024 * 1024;
/// A cover needs a few hundred kilobytes once decoded. The decoder's own
/// default would let a malformed one claim half a gigabyte first.
const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SIDE: u32 = 4096;

#[derive(Default)]
struct Slot {
    url: String,
    image: Option<RgbImage>,
}

/// Holds the cover of one track at a time, the one last asked for.
pub struct CoverArt {
    slot: Arc<Mutex<Slot>>,
    requests: Sender<(String, u32)>,
}

fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits
}

fn fetch(url: &str, size: u32) -> anyhow::Result<RgbImage> {
    let bytes = if let Some(path) = url.strip_prefix("file://") {
        let mut bytes = Vec::new();
        File::open(percent_decoded(path))?
            .take(MAX_BYTES)
            .read_to_end(&mut bytes)?;
        bytes
    } else {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .build()
            .into();
        let mut body = agent.get(url).call()?.into_body();
        body.with_config().limit(MAX_BYTES).read_to_vec()?
    };
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    reader.limits(limits());
    let cover = reader
        .decode()?
        .resize_to_fill(size, size, FilterType::Lanczos3);
    Ok(cover.to_rgb8())
}

/// Players hand out paths with the usual URL escaping. The escapes decode to
/// bytes, which only make sense together: "é" is two of them.
fn percent_decoded(path: &str) -> PathBuf {
    let mut out = Vec::with_capacity(path.len());
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%'
            && let (Some(high), Some(low)) = (bytes.next(), bytes.next())
            && let Ok(text) = std::str::from_utf8(&[high, low])
            && let Ok(decoded) = u8::from_str_radix(text, 16)
        {
            out.push(decoded);
        } else {
            out.push(byte);
        }
    }
    PathBuf::from(OsString::from_vec(out))
}

impl CoverArt {
    /// Starts the thread that downloads and scales covers.
    ///
    /// # Panics
    ///
    /// When another thread panicked while holding the cover.
    #[must_use]
    pub fn new() -> Self {
        let slot = Arc::new(Mutex::new(Slot::default()));
        let (requests, wanted) = mpsc::channel::<(String, u32)>();
        let shared = Arc::clone(&slot);
        thread::spawn(move || {
            while let Ok(mut request) = wanted.recv() {
                // Tracks skipped in a row only want the last cover.
                while let Ok(newer) = wanted.try_recv() {
                    request = newer;
                }
                let (url, size) = request;
                let image = fetch(&url, size)
                    .inspect_err(|error| eprintln!("cover art: {error:#}"))
                    .ok();
                let mut slot = shared.lock().unwrap();
                // A later track may have been asked for in the meantime.
                if slot.url == url {
                    slot.image = image;
                }
            }
        });
        Self { slot, requests }
    }

    /// The cover for `url`, once it has arrived. Asking for a new one starts
    /// its download and returns nothing until then.
    ///
    /// # Panics
    ///
    /// When another thread panicked while holding the cover.
    #[must_use]
    pub fn get(&self, url: &str, size: u32) -> Option<RgbImage> {
        let mut slot = self.slot.lock().unwrap();
        if slot.url != url {
            url.clone_into(&mut slot.url);
            slot.image = None;
            let _ = self.requests.send((url.to_owned(), size));
        }
        slot.image.clone()
    }
}

impl Default for CoverArt {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_escapes_come_back() {
        let decoded = |path| {
            percent_decoded(path)
                .into_os_string()
                .into_string()
                .unwrap()
        };
        assert_eq!(decoded("/tmp/a%20b.jpg"), "/tmp/a b.jpg");
        assert_eq!(decoded("/plain/path.png"), "/plain/path.png");
    }

    #[test]
    fn accented_paths_survive() {
        let decoded = |path| {
            percent_decoded(path)
                .into_os_string()
                .into_string()
                .unwrap()
        };
        assert_eq!(decoded("/music/Beyonc%C3%A9.jpg"), "/music/Beyoncé.jpg");
        assert_eq!(decoded("/music/Beyoncé.jpg"), "/music/Beyoncé.jpg");
    }
}
