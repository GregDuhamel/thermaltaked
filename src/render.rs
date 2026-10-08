//! Draws the 480x128 dashboard.

// Everything cast here is a pixel coordinate or a text width on a 480x128
// panel, which every integer and float type used holds exactly.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::fs;
use std::path::Path;
use std::time::Duration;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use anyhow::Context;
use chrono::{DateTime, Datelike, Local};
use image::{Rgb, RgbImage};
use imageproc::drawing::{draw_filled_rect_mut, draw_text_mut, text_size};
use imageproc::rect::Rect;

use crate::player::NowPlaying;
use crate::protocol::{HEIGHT, WIDTH};
use crate::sensors::Snapshot;
use crate::weather::Weather;

const BACKGROUND: Rgb<u8> = Rgb([6, 8, 12]);
const HEADER: Rgb<u8> = Rgb([18, 22, 30]);
const RULE: Rgb<u8> = Rgb([40, 46, 58]);
const TRACK: Rgb<u8> = Rgb([28, 32, 42]);
const TEXT: Rgb<u8> = Rgb([232, 236, 242]);
const MUTED: Rgb<u8> = Rgb([175, 183, 196]);
/// Gauge titles, fan names and header labels: readable on the panel, yet
/// apart from the white values.
const LABEL: Rgb<u8> = Rgb([110, 180, 255]);
const DATE: Rgb<u8> = Rgb([90, 225, 120]);
const ACCENT: Rgb<u8> = Rgb([80, 160, 255]);
const COOL: Rgb<u8> = Rgb([70, 230, 140]);
const WARM: Rgb<u8> = Rgb([255, 176, 32]);
const HOT: Rgb<u8> = Rgb([255, 72, 72]);

const MARGIN: i32 = 8;
const HEADER_HEIGHT: u32 = 22;
const HEADER_TEXT_SIZE: f32 = 13.0;
const TITLE_SIZE: f32 = 11.0;
const VALUE_TOP: i32 = 33;
const BAR_TOP: i32 = 68;
const BAR_HEIGHT: u32 = 5;
const COLUMN_GAP: i32 = 14;
/// Side of the cover art: the panel's full height, flush with its left edge.
pub const ART_SIZE: u32 = HEIGHT;
/// Room between the cover and the text beside it.
const ART_GAP: i32 = 16;
const FAN_BAND_TOP: i32 = 88;
/// Narrowest fan cell in which a four-digit speed still reads; the band
/// shows as many fans as cells of that width fit, and the rest go unshown.
const FAN_CELL_MIN: i32 = 44;
const FAN_CELL_MAX: i32 = 96;
/// The band holds this many fans at most.
pub const MAX_FANS: usize = ((LEFT_PANEL_RIGHT - MARGIN) / FAN_CELL_MIN) as usize;
/// The clock and weather column starts here.
const RIGHT_COLUMN_LEFT: i32 = 318;
/// Axis of the date, clock, weather and load average.
const RIGHT_COLUMN_CENTER: i32 = (RIGHT_COLUMN_LEFT + WIDTH as i32 - MARGIN) / 2;
/// Gauges, fans and the kernel version end here, one gutter before the separator.
const LEFT_PANEL_RIGHT: i32 = RIGHT_COLUMN_LEFT - 18;

const DAYS: [&str; 7] = [
    "Lundi", "Mardi", "Mercredi", "Jeudi", "Vendredi", "Samedi", "Dimanche",
];

/// The longest start of `text` that `fits` once an ellipsis ends it, or the
/// whole of it when it fits as it is. Width grows with every character kept,
/// so a binary search finds the cut in a handful of measurements rather than
/// one per character.
fn shorten(text: &str, fits: impl Fn(&str) -> bool) -> String {
    if fits(text) {
        return text.to_owned();
    }
    let starts: Vec<usize> = text.char_indices().map(|(index, _)| index).collect();
    let candidate = |kept: usize| format!("{}…", text[..starts[kept]].trim_end());
    if !fits(&candidate(0)) {
        return String::new();
    }
    // The whole text does not fit, so at most all but one character stays.
    let (mut low, mut high) = (0, starts.len() - 1);
    while low < high {
        let middle = (low + high).div_ceil(2);
        if fits(&candidate(middle)) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    candidate(low)
}

/// Minutes and seconds, as a player writes them.
fn clock_face(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// The date as the panel spells it: "Samedi 19/09/26".
fn format_date(now: DateTime<Local>) -> String {
    let day = DAYS[now.weekday().num_days_from_monday() as usize];
    format!("{day} {}", now.format("%d/%m/%y"))
}

/// How a piece of text is drawn: its size in pixels, weight and color.
#[derive(Debug, Clone, Copy)]
struct TextStyle {
    size: f32,
    bold: bool,
    color: Rgb<u8>,
}

/// A style, spelled short enough to sit in a call.
const fn style(size: f32, bold: bool, color: Rgb<u8>) -> TextStyle {
    TextStyle { size, bold, color }
}

/// One dashboard column: a title, a big value and a usage bar.
struct Gauge<'a> {
    title: &'a str,
    value: Option<f32>,
    unit: &'a str,
    color: Rgb<u8>,
    usage: Option<f32>,
}

pub struct Dashboard {
    regular: FontVec,
    bold: FontVec,
}

fn load_font(path: &Path) -> anyhow::Result<FontVec> {
    let data = fs::read(path).with_context(|| format!("reading font {}", path.display()))?;
    FontVec::try_from_vec(data).with_context(|| format!("parsing font {}", path.display()))
}

fn temp_color(temp: f32, warn: f32, critical: f32) -> Rgb<u8> {
    if temp >= critical {
        HOT
    } else if temp >= warn {
        WARM
    } else {
        COOL
    }
}

fn rect(image: &mut RgbImage, x: i32, y: i32, width: u32, height: u32, color: Rgb<u8>) {
    if width > 0 && height > 0 {
        draw_filled_rect_mut(image, Rect::at(x, y).of_size(width, height), color);
    }
}

impl Dashboard {
    /// # Errors
    ///
    /// When either font file cannot be read or parsed.
    pub fn new(regular: &Path, bold: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            regular: load_font(regular)?,
            bold: load_font(bold)?,
        })
    }

    const fn font(&self, bold: bool) -> &FontVec {
        if bold { &self.bold } else { &self.regular }
    }

    /// The width of `text` drawn in `style`.
    fn width(&self, style: TextStyle, text: &str) -> i32 {
        text_size(PxScale::from(style.size), self.font(style.bold), text).0 as i32
    }

    /// Draws `text` with its left edge at `x`, and returns where it ends.
    fn text(&self, image: &mut RgbImage, x: i32, y: i32, style: TextStyle, text: &str) -> i32 {
        let scale = PxScale::from(style.size);
        draw_text_mut(image, style.color, x, y, scale, self.font(style.bold), text);
        x + self.width(style, text)
    }

    /// Draws `text` centered on `center`.
    fn text_centered(
        &self,
        image: &mut RgbImage,
        center: i32,
        y: i32,
        style: TextStyle,
        text: &str,
    ) {
        let x = center - self.width(style, text) / 2;
        self.text(image, x, y, style, text);
    }

    /// A big number and its small unit, centered together on `center`.
    fn value(&self, image: &mut RgbImage, center: i32, color: Rgb<u8>, number: &str, unit: &str) {
        const UNIT_GAP: i32 = 2;
        let (number_style, unit_style) = (style(32.0, true, color), style(15.0, false, MUTED));
        let width = self.width(number_style, number) + UNIT_GAP + self.width(unit_style, unit);
        let x = center - width / 2;
        let end = self.text(image, x, VALUE_TOP, number_style, number);
        self.text(image, end + UNIT_GAP, VALUE_TOP + 4, unit_style, unit);
    }

    /// Gauge title. A product name too long for its column loses its leading
    /// words: "Radeon RX 9070 XT" becomes "RX 9070 XT".
    fn title(&self, image: &mut RgbImage, x: i32, width: u32, name: &str) {
        let scale = PxScale::from(TITLE_SIZE);
        let mut fitted = name;
        while text_size(scale, &self.bold, fitted).0 > width
            && let Some((_, rest)) = fitted.split_once(' ')
        {
            fitted = rest;
            // "Ryzen 9 9900X3D" shrinks to "9900X3D", not "9 9900X3D".
            if let Some((word, rest)) = fitted.split_once(' ')
                && word.bytes().all(|byte| byte.is_ascii_digit())
            {
                fitted = rest;
            }
        }
        let center = x + width as i32 / 2;
        self.text_centered(image, center, 25, style(TITLE_SIZE, true, LABEL), fitted);
    }

    /// Title, value, then a usage bar with its percentage underneath, all
    /// centered on the column.
    fn gauge(&self, image: &mut RgbImage, x: i32, width: u32, gauge: &Gauge) {
        self.title(image, x, width, gauge.title);
        let center = x + width as i32 / 2;
        let number = gauge
            .value
            .map_or_else(|| "--".to_owned(), |value| format!("{value:.0}"));
        self.value(image, center, gauge.color, &number, gauge.unit);
        if let Some(usage) = gauge.usage {
            let filled = (width as f32 * usage.clamp(0.0, 100.0) / 100.0).round() as u32;
            rect(image, x, BAR_TOP, width, BAR_HEIGHT, TRACK);
            rect(image, x, BAR_TOP, filled, BAR_HEIGHT, ACCENT);
            let percent = format!("{usage:.0}%");
            let percent_top = BAR_TOP + BAR_HEIGHT as i32 + 1;
            self.text_centered(
                image,
                center,
                percent_top,
                style(11.0, false, TEXT),
                &percent,
            );
        }
    }

    /// Top coordinate at which capitals of `size` sit vertically centered in a band.
    fn cap_centered_top(&self, size: f32, band_top: i32, band_height: u32) -> i32 {
        let font = self.regular.as_scaled(PxScale::from(size));
        let cap_height = font
            .outline_glyph(font.scaled_glyph('H'))
            .map_or(size * 0.7, |glyph| -glyph.px_bounds().min.y);
        let baseline = band_top as f32 + f32::midpoint(band_height as f32, cap_height);
        (baseline - font.ascent()).round() as i32
    }

    /// Draws "label value" with the label in the label color, at the x that `place` derives
    /// from its width. Without `room` for it, the label goes, then everything.
    fn labelled(
        &self,
        image: &mut RgbImage,
        y: i32,
        label: &str,
        value: &str,
        room: i32,
        place: impl Fn(i32) -> i32,
    ) {
        let (label_style, value_style) = (
            style(HEADER_TEXT_SIZE, false, LABEL),
            style(HEADER_TEXT_SIZE, false, TEXT),
        );
        let prefix = format!("{label} ");
        let Some((prefix, total)) = [prefix.as_str(), ""]
            .into_iter()
            .map(|prefix| {
                let total = self.width(label_style, prefix) + self.width(value_style, value);
                (prefix, total)
            })
            .find(|&(_, total)| total <= room)
        else {
            return;
        };
        let end = self.text(image, place(total), y, label_style, prefix);
        self.text(image, end, y, value_style, value);
    }

    /// Hostname, kernel and load share one baseline. The hostname starts on
    /// the gauges' left edge, the kernel is centered in the space left before
    /// the separator, and the load sits on the clock's axis.
    fn header(&self, image: &mut RgbImage, snapshot: &Snapshot<'_>) {
        rect(image, 0, 0, WIDTH, HEADER_HEIGHT, HEADER);
        let y = self.cap_centered_top(HEADER_TEXT_SIZE, 0, HEADER_HEIGHT);
        let hostname = &snapshot.hostname;
        let hostname_end = self.text(
            image,
            MARGIN,
            y,
            style(HEADER_TEXT_SIZE, true, TEXT),
            hostname,
        );

        let room = LEFT_PANEL_RIGHT - hostname_end - 12;
        let kernel = snapshot.kernel;
        self.labelled(image, y, "Kernel", kernel, room, |width| {
            (hostname_end + LEFT_PANEL_RIGHT - width) / 2
        });

        let [one, five, fifteen] = snapshot.load_average;
        let load = format!("{one:.2} {five:.2} {fifteen:.2}");
        let room = WIDTH as i32 - MARGIN - RIGHT_COLUMN_LEFT;
        self.labelled(image, y, "Load", &load, room, |width| {
            RIGHT_COLUMN_CENTER - width / 2
        });
    }

    /// Cuts a text down until it fits `width`, ending it in an ellipsis.
    fn shortened(&self, text: &str, style: TextStyle, width: i32) -> String {
        shorten(text, |candidate| self.width(style, candidate) <= width)
    }

    /// What is playing: cover on the left, track and progress on the right.
    #[must_use]
    pub fn render_player(
        &self,
        playing: &NowPlaying,
        art: Option<&RgbImage>,
        now: DateTime<Local>,
    ) -> RgbImage {
        let mut canvas = RgbImage::from_pixel(WIDTH, HEIGHT, BACKGROUND);
        let image = &mut canvas;
        match art {
            Some(art) => image::imageops::replace(image, art, 0, 0),
            // A plain square holds the place until the cover arrives.
            None => rect(image, 0, 0, ART_SIZE, ART_SIZE, HEADER),
        }
        let left = ART_SIZE as i32 + ART_GAP;
        let right = WIDTH as i32 - MARGIN;
        let width = right - left;
        let small = style(12.0, false, MUTED);

        let time = now.format("%H:%M").to_string();
        let time_width = self.width(small, &time);
        self.text(image, right - time_width, 10, small, &time);

        let title_style = style(21.0, true, TEXT);
        let room = width - time_width - 10;
        let title = self.shortened(&playing.track.title, title_style, room);
        self.text(image, left, 22, title_style, &title);
        let mut line = playing.track.artist.clone();
        if !playing.track.album.is_empty() {
            line = format!("{line} · {}", playing.track.album);
        }
        let line_style = style(14.0, false, LABEL);
        let line = self.shortened(&line, line_style, width);
        self.text(image, left, 52, line_style, &line);

        let bar_top = 84;
        rect(image, left, bar_top, width as u32, BAR_HEIGHT, TRACK);
        if let Some(length) = playing.length.filter(|length| !length.is_zero()) {
            let share = playing.position.as_secs_f32() / length.as_secs_f32();
            let filled = (width as f32 * share.clamp(0.0, 1.0)).round() as u32;
            rect(image, left, bar_top, filled, BAR_HEIGHT, ACCENT);
            let total = clock_face(length);
            let total_width = self.width(small, &total);
            self.text(image, right - total_width, 98, small, &total);
        }
        self.text(image, left, 98, small, &clock_face(playing.position));
        canvas
    }

    /// The whole panel given over to the date and the time, for when the
    /// monitor is asleep or the session is locked.
    #[must_use]
    pub fn render_clock(&self, now: DateTime<Local>) -> RgbImage {
        let mut canvas = RgbImage::from_pixel(WIDTH, HEIGHT, BACKGROUND);
        let image = &mut canvas;
        let center = WIDTH as i32 / 2;
        let date = format_date(now);
        self.text_centered(image, center, 18, style(20.0, false, DATE), &date);
        let time = now.format("%H:%M").to_string();
        self.text_centered(image, center, 42, style(76.0, true, TEXT), &time);
        canvas
    }

    #[must_use]
    pub fn render(
        &self,
        snapshot: &Snapshot<'_>,
        weather: Option<&Weather>,
        now: DateTime<Local>,
    ) -> RgbImage {
        let mut canvas = RgbImage::from_pixel(WIDTH, HEIGHT, BACKGROUND);
        let image = &mut canvas;

        self.header(image, snapshot);

        rect(
            image,
            RIGHT_COLUMN_LEFT - 9,
            HEADER_HEIGHT as i32 + 6,
            1,
            HEIGHT - HEADER_HEIGHT - 12,
            RULE,
        );

        let columns = if snapshot.psu_power.is_some() { 3 } else { 2 };
        let gauge_width =
            ((LEFT_PANEL_RIGHT - MARGIN - COLUMN_GAP * (columns - 1)) / columns) as u32;
        let column = |index: i32| MARGIN + (gauge_width as i32 + COLUMN_GAP) * index;
        let temp_gauge = |title, temp: Option<f32>, usage, warn, critical| Gauge {
            title,
            value: temp,
            unit: "°C",
            color: temp.map_or(MUTED, |temp| temp_color(temp, warn, critical)),
            usage,
        };
        let cpu = temp_gauge(
            &snapshot.names.cpu,
            snapshot.cpu_temp,
            snapshot.cpu_usage,
            75.0,
            90.0,
        );
        // Junction temperature: it runs hotter than the edge, and AMD cards
        // only reach their critical point at 110 °C.
        let gpu = temp_gauge(
            &snapshot.names.gpu,
            snapshot.gpu_temp,
            snapshot.gpu_usage,
            95.0,
            105.0,
        );
        self.gauge(image, column(0), gauge_width, &cpu);
        self.gauge(image, column(1), gauge_width, &gpu);
        if snapshot.psu_power.is_some() {
            let psu = Gauge {
                title: &snapshot.names.psu,
                value: snapshot.psu_power,
                unit: "W",
                color: TEXT,
                usage: snapshot.psu_usage,
            };
            self.gauge(image, column(2), gauge_width, &psu);
        }

        if !snapshot.fans.is_empty() {
            // The fans sit in their own band, apart from the gauges above.
            let band_width = (RIGHT_COLUMN_LEFT - 9) as u32;
            rect(
                image,
                0,
                FAN_BAND_TOP,
                band_width,
                HEIGHT - FAN_BAND_TOP as u32,
                HEADER,
            );
            rect(image, 0, FAN_BAND_TOP, band_width, 1, RULE);
            let fans = &snapshot.fans[..snapshot.fans.len().min(MAX_FANS)];
            let cell = ((LEFT_PANEL_RIGHT - MARGIN) / fans.len() as i32).min(FAN_CELL_MAX);
            for (index, fan) in fans.iter().enumerate() {
                let center = MARGIN + cell * index as i32 + cell / 2;
                let label_style = style(10.0, false, LABEL);
                let label = self.shortened(&fan.label.to_uppercase(), label_style, cell - 4);
                self.text_centered(image, center, FAN_BAND_TOP + 6, label_style, &label);
                let rpm = fan.rpm.to_string();
                let rpm_style = style(15.0, true, TEXT);
                self.text_centered(image, center, FAN_BAND_TOP + 18, rpm_style, &rpm);
            }
        }

        // Date, clock and weather share the right column's axis.
        let center = RIGHT_COLUMN_CENTER;
        let date = format_date(now);
        self.text_centered(image, center, 26, style(15.0, false, DATE), &date);
        let time = now.format("%H:%M").to_string();
        self.text_centered(image, center, 38, style(44.0, true, TEXT), &time);

        if let Some(weather) = weather {
            const WEATHER_GAP: i32 = 6;
            let (temp_style, description_style) =
                (style(20.0, true, ACCENT), style(13.0, false, TEXT));
            let temp = format!("{:.0}°", weather.temp);
            let width =
                self.width(temp_style, &temp) + self.width(description_style, weather.description);
            let x = center - width / 2 - WEATHER_GAP / 2;
            let end = self.text(image, x, 86, temp_style, &temp);
            self.text(
                image,
                end + WEATHER_GAP,
                91,
                description_style,
                weather.description,
            );
            self.text_centered(image, center, 110, style(11.0, false, MUTED), &weather.city);
        }

        canvas
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten pixels a character, so the widths are easy to reason about.
    fn fits(width: usize) -> impl Fn(&str) -> bool {
        move |text: &str| text.chars().count() * 10 <= width
    }

    #[test]
    fn short_texts_are_left_alone() {
        assert_eq!(shorten("Adagio", fits(100)), "Adagio");
    }

    #[test]
    fn long_texts_keep_as_much_as_fits() {
        assert_eq!(shorten("Adagio for Strings", fits(100)), "Adagio fo…");
        // A cut on a space does not leave it dangling before the ellipsis.
        assert_eq!(shorten("Adagio for Strings", fits(80)), "Adagio…");
        assert_eq!(shorten("Beyoncé Knowles", fits(80)), "Beyoncé…");
    }

    #[test]
    fn nothing_when_not_even_the_ellipsis_fits() {
        assert_eq!(shorten("Adagio", fits(5)), "");
    }

    #[test]
    fn the_result_never_overflows() {
        let text = "Exploration Of Space (Cosmic Gate Remix) — Live at Ushuaïa";
        for width in 0..700 {
            let short = shorten(text, fits(width));
            assert!(fits(width)(&short) || short.is_empty(), "{width}: {short}");
        }
    }
}
