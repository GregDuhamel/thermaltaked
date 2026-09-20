//! Cover art, fetched off the drawing path so a slow network never holds a
//! frame back.

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use image::RgbImage;
use image::imageops::FilterType;

const TIMEOUT: Duration = Duration::from_secs(10);
/// Covers are small; anything larger is not one.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

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

fn fetch(url: &str, size: u32) -> anyhow::Result<RgbImage> {
    let mut response = if let Some(path) = url.strip_prefix("file://") {
        image::open(percent_decoded(path))?
    } else {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .build()
            .into();
        let mut body = agent.get(url).call()?.into_body();
        let bytes = body.with_config().limit(MAX_BYTES).read_to_vec()?;
        image::load_from_memory(&bytes)?
    };
    response = response.resize_to_fill(size, size, FilterType::Lanczos3);
    Ok(response.to_rgb8())
}

/// Players hand out paths with the usual URL escaping.
fn percent_decoded(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%'
            && let (Some(high), Some(low)) = (bytes.next(), bytes.next())
            && let Ok(text) = std::str::from_utf8(&[high, low])
            && let Ok(decoded) = u8::from_str_radix(text, 16)
        {
            out.push(decoded as char);
        } else {
            out.push(byte as char);
        }
    }
    out
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
            while let Ok((url, size)) = wanted.recv() {
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
            slot.url = url.to_owned();
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
        assert_eq!(percent_decoded("/tmp/a%20b.jpg"), "/tmp/a b.jpg");
        assert_eq!(percent_decoded("/plain/path.png"), "/plain/path.png");
    }
}
