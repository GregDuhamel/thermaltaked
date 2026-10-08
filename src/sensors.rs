//! System readings taken straight from /sys/class/hwmon and /proc.
//!
//! The hwmon tree is walked once at start, and again when it changes under
//! the daemon: a GPU reset or a resume from sleep makes amdgpu register its
//! chip anew under another number, a power supply plugged back in does the
//! same, and a driver loaded after the daemon brings a chip it never saw. A
//! reading whose file is gone, or a sensor not found yet, has the tree walked
//! again, no more than once every few seconds.

use std::cell::Cell;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use log::{debug, info, warn};

use crate::config::Config;

const HWMON: &str = "/sys/class/hwmon";
/// (chip name, temperature label) pairs tried in order.
const CPU_TEMP_SOURCES: [(&str, &str); 3] = [
    ("k10temp", "Tctl"),
    ("coretemp", "Package id 0"),
    ("zenpower", "Tdie"),
];
/// libdrm's table of AMD marketing names: "device id, revision, name" rows.
const AMDGPU_IDS: &str = "/usr/share/libdrm/amdgpu.ids";
/// The junction is the GPU's hottest point and the one it throttles on; older
/// AMD cards only report their edge.
const GPU_TEMP_SOURCES: [(&str, &str); 3] =
    [("amdgpu", "junction"), ("amdgpu", "edge"), ("nouveau", "")];
/// A reading whose file vanished has the tree walked again, but no sooner
/// than this after the last walk: a chip that re-registers takes a moment.
const REDISCOVER_LOST_AFTER: Duration = Duration::from_secs(5);
/// A sensor not found is looked for again this often, in case its driver
/// was loaded, or its device plugged in, after the daemon started.
const REDISCOVER_MISSING_EVERY: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct Fan<'a> {
    pub label: &'a str,
    pub rpm: u32,
}

/// Product names shown above each gauge.
#[derive(Debug, Default, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Names {
    pub cpu: String,
    pub gpu: String,
    pub psu: String,
}

/// One reading of every sensor. What never changes (the kernel) or changes
/// only when the tree is walked again (names, fan labels) is borrowed from
/// `Sensors`, which works it out then.
#[derive(Debug)]
pub struct Snapshot<'a> {
    pub names: &'a Names,
    pub hostname: String,
    /// Release without the distribution suffix: "7.2.5" for "7.2.5-200.fc44.x86_64".
    pub kernel: &'a str,
    pub load_average: [f32; 3],
    pub cpu_temp: Option<f32>,
    pub cpu_usage: Option<f32>,
    pub gpu_temp: Option<f32>,
    pub gpu_usage: Option<f32>,
    /// Watts drawn from the power supply, when it reports them.
    pub psu_power: Option<f32>,
    /// That draw as a share of the supply's rated wattage, in percent.
    pub psu_usage: Option<f32>,
    pub fans: Vec<Fan<'a>>,
}

#[derive(Debug)]
struct FanInput {
    path: PathBuf,
    label: String,
    always_shown: bool,
    /// Position in the config, so the display follows the order written there.
    order: usize,
}

/// What the configuration says about which sensors to show and what to
/// call them; kept so the tree can be walked again on the same terms.
struct Selection {
    fans: IndexMap<String, String>,
    names: Names,
    psu_rating: Option<f32>,
}

/// The sysfs files each reading comes from, as the last walk found them.
#[derive(Debug, Default)]
struct Inputs {
    cpu_temp: Option<PathBuf>,
    gpu_temp: Option<PathBuf>,
    gpu_usage: Option<PathBuf>,
    psu_power: Option<PathBuf>,
    psu_rating: Option<f32>,
    fans: Vec<FanInput>,
}

impl Inputs {
    /// Every reading by name, with the file it comes from when it has one.
    fn listed(&self) -> Vec<(String, Option<&Path>)> {
        let mut listed: Vec<(String, Option<&Path>)> = [
            ("CPU temperature", &self.cpu_temp),
            ("GPU temperature", &self.gpu_temp),
            ("GPU usage", &self.gpu_usage),
            ("PSU power", &self.psu_power),
        ]
        .into_iter()
        .map(|(label, path)| (label.to_owned(), path.as_deref()))
        .collect();
        listed.extend(
            self.fans
                .iter()
                .map(|fan| (format!("fan {}", fan.label), Some(fan.path.as_path()))),
        );
        listed
    }

    /// Whether a reading has no file to come from, or a configured fan was
    /// not found: worth another look at the tree now and then.
    fn incomplete(&self, selection: &Selection) -> bool {
        self.cpu_temp.is_none()
            || self.gpu_temp.is_none()
            || self.psu_power.is_none()
            || self.fans.len() < selection.fans.len()
    }
}

pub struct Sensors {
    hwmon: PathBuf,
    selection: Selection,
    /// From /proc/cpuinfo, which does not change while running.
    cpu_model: Option<String>,
    kernel: String,
    names: Names,
    inputs: Inputs,
    /// When the tree was last walked.
    discovered: Instant,
    /// Whether a reading since then found its file gone.
    lost: bool,
    /// (busy, total) jiffies of the previous /proc/stat sample.
    previous_cpu_times: Option<(u64, u64)>,
}

fn read_trimmed(path: impl AsRef<Path>) -> io::Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_owned())
}

fn read_text(path: impl AsRef<Path>) -> Option<String> {
    read_trimmed(path).ok()
}

/// Every chip under `hwmon` as (name, directory), sorted by name so that the
/// order, and with it the fans', is the same from one walk to the next
/// whatever numbers the kernel hands out.
fn chips(hwmon: &Path) -> Vec<(String, PathBuf)> {
    let mut chips: Vec<(String, PathBuf)> = fs::read_dir(hwmon)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| Some((read_text(entry.path().join("name"))?, entry.path())))
        .collect();
    chips.sort();
    chips
}

fn find_temp(chips: &[(String, PathBuf)], sources: &[(&str, &str)]) -> Option<PathBuf> {
    sources.iter().find_map(|&(chip, label)| {
        let (_, dir) = chips.iter().find(|(name, _)| name == chip)?;
        (1..=16).find_map(|index| {
            let input = dir.join(format!("temp{index}_input"));
            let label_path = dir.join(format!("temp{index}_label"));
            (input.exists() && read_text(label_path).unwrap_or_default() == label).then_some(input)
        })
    })
}

/// The fans of every chip, those the configuration names when it names any,
/// in its order; otherwise all of them, in chip order.
fn find_fans(chips: &[(String, PathBuf)], configured: &IndexMap<String, String>) -> Vec<FanInput> {
    let mut fans = Vec::new();
    for (chip, dir) in chips {
        let inputs: Vec<(u32, PathBuf)> = (1..=16)
            .map(|index| (index, dir.join(format!("fan{index}_input"))))
            .filter(|(_, path)| path.exists())
            .collect();
        // A lone fan (GPU, PSU) is better named after its chip than "fan1".
        let lone = inputs.len() == 1;
        for (index, path) in inputs {
            let named = configured.get_full(&format!("{chip}/fan{index}"));
            // Once fans are named in the config, that list is the selection.
            if named.is_none() && !configured.is_empty() {
                continue;
            }
            let label = named
                .map(|(_, _, label)| label.clone())
                .or_else(|| read_text(dir.join(format!("fan{index}_label"))))
                .unwrap_or_else(|| {
                    if lone {
                        chip.clone()
                    } else {
                        format!("fan{index}")
                    }
                });
            fans.push(FanInput {
                path,
                label,
                always_shown: named.is_some(),
                order: named.map_or(usize::MAX, |(position, _, _)| position),
            });
        }
    }
    fans.sort_by_key(|fan| fan.order);
    fans
}

/// "AMD Ryzen 9 9900X3D 12-Core Processor" becomes "Ryzen 9 9900X3D", and
/// "Intel(R) Core(TM) i7-8700K CPU @ 3.70GHz" becomes "i7-8700K".
fn cpu_name_from(cpuinfo: &str) -> Option<String> {
    let model = cpuinfo
        .lines()
        .find(|line| line.starts_with("model name"))?
        .split_once(':')?
        .1;
    let words: Vec<&str> = model
        .split_whitespace()
        .filter(|word| !matches!(*word, "AMD" | "Intel(R)" | "Core(TM)" | "CPU"))
        .take_while(|word| !word.ends_with("-Core") && *word != "@")
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

fn cpu_name() -> Option<String> {
    cpu_name_from(&fs::read_to_string("/proc/cpuinfo").ok()?)
}

/// Looks the PCI device and revision of an amdgpu hwmon chip up in libdrm's
/// `table`.
fn amd_gpu_name(hwmon: &Path, table: &Path) -> Option<String> {
    let id = |file: &str| {
        let text = read_text(hwmon.join("device").join(file))?;
        u32::from_str_radix(text.trim_start_matches("0x"), 16).ok()
    };
    let (device, revision) = (id("device")?, id("revision")?);
    let table = fs::read_to_string(table).ok()?;
    table.lines().find_map(|line| {
        let mut fields = line.split(',').map(str::trim);
        let matches = u32::from_str_radix(fields.next()?, 16).ok()? == device
            && u32::from_str_radix(fields.next()?, 16).ok()? == revision;
        matches.then(|| {
            fields
                .next()
                .map(|name| name.trim_start_matches("AMD ").to_owned())
        })?
    })
}

/// The `HID_NAME` of the power supply, e.g. `CORSAIR HX1200i Power Supply`.
fn psu_name(hwmon: &Path) -> Option<String> {
    let uevent = fs::read_to_string(hwmon.join("device/uevent")).ok()?;
    let name = uevent
        .lines()
        .find_map(|line| line.strip_prefix("HID_NAME="))?;
    Some(name.trim_end_matches(" Power Supply").to_owned())
}

/// Corsair model names carry the rating: an `HX1200i` is a 1200 W unit.
fn rating_from_model(model: &str) -> Option<f32> {
    model.split_whitespace().find_map(|word| {
        let digits: String = word
            .trim_start_matches(|c: char| c.is_ascii_alphabetic())
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        let watts: f32 = digits.parse().ok()?;
        (300.0..=3000.0).contains(&watts).then_some(watts)
    })
}

fn cpu_times() -> Option<(u64, u64)> {
    let stat = fs::read_to_string("/proc/stat").ok()?;
    let fields: Vec<u64> = stat
        .lines()
        .next()?
        .split_whitespace()
        .skip(1)
        .filter_map(|field| field.parse().ok())
        .collect();
    let total: u64 = fields.iter().sum();
    // Fields 3 and 4 are idle and iowait.
    let idle = fields.get(3)? + fields.get(4)?;
    Some((total - idle, total))
}

/// Walks the tree under `hwmon` for every reading's file, and works the
/// gauge names out from what it finds and the configuration.
fn discover(hwmon: &Path, selection: &Selection, cpu_model: Option<&str>) -> (Inputs, Names) {
    let chips = chips(hwmon);
    let gpu_temp = find_temp(&chips, &GPU_TEMP_SOURCES);
    let gpu_usage = gpu_temp
        .as_ref()
        .and_then(|temp| Some(temp.parent()?.join("device/gpu_busy_percent")))
        .filter(|path| path.exists());
    let gpu_chip = gpu_temp.as_ref().and_then(|temp| temp.parent());
    let psu = chips.iter().find(|(name, _)| name == "corsairpsu");
    let psu_model = psu.and_then(|(_, dir)| psu_name(dir));
    let or_detected = |configured: &String, detected: Option<String>, fallback: &str| {
        if configured.is_empty() {
            detected.unwrap_or_else(|| fallback.to_owned())
        } else {
            configured.clone()
        }
    };
    let names = Names {
        cpu: or_detected(&selection.names.cpu, cpu_model.map(str::to_owned), "CPU"),
        gpu: or_detected(
            &selection.names.gpu,
            gpu_chip.and_then(|chip| amd_gpu_name(chip, Path::new(AMDGPU_IDS))),
            "GPU",
        ),
        psu: or_detected(&selection.names.psu, psu_model.clone(), "PSU"),
    };
    let inputs = Inputs {
        cpu_temp: find_temp(&chips, &CPU_TEMP_SOURCES),
        gpu_temp,
        gpu_usage,
        psu_power: psu
            .map(|(_, dir)| dir.join("power1_input"))
            .filter(|path| path.exists()),
        psu_rating: selection
            .psu_rating
            .or_else(|| psu_model.as_deref().and_then(rating_from_model)),
        fans: find_fans(&chips, &selection.fans),
    };
    (inputs, names)
}

/// Says in the log what a walk found, or what changed since the last one.
fn log_changes(before: Option<&Inputs>, after: &Inputs) {
    let Some(before) = before else {
        let (found, missing): (Vec<_>, Vec<_>) = after
            .listed()
            .into_iter()
            .partition(|(_, path)| path.is_some());
        let found: Vec<String> = found.into_iter().map(|(label, _)| label).collect();
        let missing: Vec<String> = missing.into_iter().map(|(label, _)| label).collect();
        let found = if found.is_empty() {
            "nothing".to_owned()
        } else {
            found.join(", ")
        };
        if missing.is_empty() {
            info!("sensors: {found}");
        } else {
            info!("sensors: {found} (no {})", missing.join(", no "));
        }
        for (label, path) in after.listed() {
            if let Some(path) = path {
                debug!("{label}: {}", path.display());
            }
        }
        return;
    };
    let was = before.listed();
    let mut changed = false;
    for (label, path) in after.listed() {
        let earlier = was
            .iter()
            .find(|(known, _)| *known == label)
            .and_then(|(_, path)| *path);
        match (earlier, path) {
            (None, Some(path)) => info!("{label} found at {}", path.display()),
            (Some(from), Some(to)) if from != to => info!("{label} moved to {}", to.display()),
            _ => continue,
        }
        changed = true;
    }
    for (label, path) in was {
        let present = after
            .listed()
            .iter()
            .any(|(known, path)| *known == label && path.is_some());
        if path.is_some() && !present {
            warn!("{label} gone");
            changed = true;
        }
    }
    if !changed {
        debug!("hwmon walked again, nothing changed");
    }
}

impl Sensors {
    /// Finds every sensor under `/sys/class/hwmon`.
    #[must_use]
    pub fn new(config: &Config) -> Self {
        Self::with_root(config, HWMON)
    }

    /// Finds every sensor under `hwmon`, laid out as `/sys/class/hwmon` is:
    /// one directory per chip, holding its `name` and its inputs.
    #[must_use]
    pub fn with_root(config: &Config, hwmon: impl Into<PathBuf>) -> Self {
        let selection = Selection {
            fans: config.fans.clone(),
            names: config.names.clone(),
            psu_rating: config.psu_rating,
        };
        let hwmon = hwmon.into();
        let cpu_model = cpu_name();
        let (inputs, names) = discover(&hwmon, &selection, cpu_model.as_deref());
        log_changes(None, &inputs);
        // The same release string `uname -r` prints.
        let kernel = read_text("/proc/sys/kernel/osrelease")
            .and_then(|release| Some(release.split('-').next()?.to_owned()))
            .unwrap_or_default();
        Self {
            hwmon,
            selection,
            cpu_model,
            kernel,
            names,
            inputs,
            discovered: Instant::now(),
            lost: false,
            previous_cpu_times: None,
        }
    }

    /// Walks the tree again when a reading lost its file, or a sensor is
    /// still missing, and the last walk is long enough ago.
    fn rediscover_if_due(&mut self) {
        let since = self.discovered.elapsed();
        let due = (self.lost && since >= REDISCOVER_LOST_AFTER)
            || (self.inputs.incomplete(&self.selection) && since >= REDISCOVER_MISSING_EVERY);
        if !due {
            return;
        }
        let (inputs, names) = discover(&self.hwmon, &self.selection, self.cpu_model.as_deref());
        log_changes(Some(&self.inputs), &inputs);
        self.inputs = inputs;
        self.names = names;
        self.discovered = Instant::now();
        self.lost = false;
    }

    // Jiffies since the last sample: thousands, far below what f32 holds exactly.
    #[allow(clippy::cast_precision_loss)]
    fn cpu_usage(&mut self) -> Option<f32> {
        let current = cpu_times()?;
        let previous = self.previous_cpu_times.replace(current)?;
        let total = current
            .1
            .checked_sub(previous.1)
            .filter(|&delta| delta > 0)?;
        let busy = current.0.saturating_sub(previous.0);
        Some(busy as f32 * 100.0 / total as f32)
    }

    /// Takes a /proc/stat sample and throws it away, so that the next usage
    /// is measured from now rather than from whenever the dashboard last ran.
    pub fn sample_cpu(&mut self) {
        let _ = self.cpu_usage();
    }

    pub fn snapshot(&mut self) -> Snapshot<'_> {
        self.rediscover_if_due();
        let cpu_usage = self.cpu_usage();
        let mut load_average = [0.0; 3];
        if let Some(loadavg) = read_text("/proc/loadavg") {
            for (slot, field) in load_average.iter_mut().zip(loadavg.split_whitespace()) {
                *slot = field.parse().unwrap_or(0.0);
            }
        }

        // A file that is no longer there means the chip re-registered or
        // the device left: the tree is walked again, after a moment.
        let lost = Cell::new(false);
        let read = |path: &Path| match read_trimmed(path) {
            Ok(text) => Some(text),
            Err(error) => {
                if error.kind() == ErrorKind::NotFound {
                    lost.set(true);
                }
                None
            }
        };
        let number = |path: &Option<PathBuf>| read(path.as_ref()?)?.parse::<f32>().ok();
        let millidegrees = |path: &Option<PathBuf>| Some(number(path)? / 1000.0);
        let inputs = &self.inputs;
        let cpu_temp = millidegrees(&inputs.cpu_temp);
        let gpu_temp = millidegrees(&inputs.gpu_temp);
        let gpu_usage = number(&inputs.gpu_usage);
        // hwmon reports power in microwatts.
        let psu_power = number(&inputs.psu_power).map(|microwatts| microwatts / 1e6);
        let fans = inputs
            .fans
            .iter()
            .filter_map(|fan| {
                let rpm: u32 = read(&fan.path)?.parse().ok()?;
                (rpm > 0 || fan.always_shown).then_some(Fan {
                    label: &fan.label,
                    rpm,
                })
            })
            .collect();
        self.lost |= lost.get();

        Snapshot {
            names: &self.names,
            hostname: read_text("/proc/sys/kernel/hostname").unwrap_or_default(),
            kernel: &self.kernel,
            load_average,
            cpu_temp,
            cpu_usage,
            gpu_temp,
            gpu_usage,
            psu_power,
            psu_usage: psu_power
                .zip(inputs.psu_rating)
                .map(|(watts, rating)| watts * 100.0 / rating),
            fans,
        }
    }
}

// The values compared are the very literals the fake tree was written with.
#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    /// A `/sys/class/hwmon` lookalike: one directory per chip, holding its
    /// `name` and whatever files the case needs. Each case gets its own,
    /// since the tests run side by side.
    struct Tree(tempfile::TempDir);

    impl Tree {
        fn new() -> Self {
            Self(tempfile::tempdir().unwrap())
        }

        fn path(&self) -> &Path {
            self.0.path()
        }

        /// Creates `dir` for a chip called `name`, with `files` in it; a
        /// slash in a file name makes the directories on the way.
        fn chip(&self, dir: &str, name: &str, files: &[(&str, &str)]) -> PathBuf {
            let chip = self.path().join(dir);
            fs::create_dir_all(&chip).unwrap();
            fs::write(chip.join("name"), format!("{name}\n")).unwrap();
            for (file, content) in files {
                let path = chip.join(file);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, format!("{content}\n")).unwrap();
            }
            chip
        }
    }

    fn config(fans: &[(&str, &str)]) -> Config {
        Config {
            fans: fans
                .iter()
                .map(|(key, label)| ((*key).to_owned(), (*label).to_owned()))
                .collect(),
            ..Config::default()
        }
    }

    fn fan_labels<'a>(snapshot: &'a Snapshot<'a>) -> Vec<(&'a str, u32)> {
        snapshot
            .fans
            .iter()
            .map(|fan| (fan.label, fan.rpm))
            .collect()
    }

    /// Makes the last walk look `ago` old, so the next snapshot is due one.
    fn walked_ago(sensors: &mut Sensors, ago: Duration) {
        sensors.discovered = Instant::now().checked_sub(ago).unwrap();
    }

    #[test]
    fn every_spinning_fan_is_shown_when_none_is_configured() {
        let tree = Tree::new();
        tree.chip(
            "hwmon3",
            "nct6799",
            &[
                ("fan1_input", "900"),
                ("fan1_label", "CPU Fan"),
                ("fan2_input", "0"),
            ],
        );
        tree.chip("hwmon1", "amdgpu", &[("fan1_input", "1200")]);
        let mut sensors = Sensors::with_root(&config(&[]), tree.path());
        // Chips come in name order, not hwmon number order; a lone fan is
        // named after its chip, a labelled one after its label.
        let labels: Vec<&str> = sensors
            .inputs
            .fans
            .iter()
            .map(|fan| fan.label.as_str())
            .collect();
        assert_eq!(labels, ["amdgpu", "CPU Fan", "fan2"]);
        // A fan that is not spinning is left out of the snapshot.
        let snapshot = sensors.snapshot();
        assert_eq!(fan_labels(&snapshot), [("amdgpu", 1200), ("CPU Fan", 900)]);
    }

    #[test]
    fn configured_fans_are_the_selection_in_their_order() {
        let tree = Tree::new();
        tree.chip(
            "hwmon3",
            "nct6799",
            &[("fan1_input", "900"), ("fan2_input", "0")],
        );
        tree.chip("hwmon1", "amdgpu", &[("fan1_input", "1200")]);
        let config = config(&[("nct6799/fan2", "rear"), ("amdgpu/fan1", "gpu")]);
        let mut sensors = Sensors::with_root(&config, tree.path());
        // Only the named fans, as written, and a named fan shows even still.
        let snapshot = sensors.snapshot();
        assert_eq!(fan_labels(&snapshot), [("rear", 0), ("gpu", 1200)]);
    }

    #[test]
    fn cpu_names_lose_their_marketing() {
        let amd = "processor\t: 0\nmodel name\t: AMD Ryzen 9 9900X3D 12-Core Processor\n";
        assert_eq!(cpu_name_from(amd).as_deref(), Some("Ryzen 9 9900X3D"));
        let intel = "model name\t: Intel(R) Core(TM) i7-8700K CPU @ 3.70GHz\n";
        assert_eq!(cpu_name_from(intel).as_deref(), Some("i7-8700K"));
        assert_eq!(cpu_name_from("processor\t: 0\n"), None);
    }

    #[test]
    fn cpu_temperature_sources_are_tried_in_order() {
        let tree = Tree::new();
        tree.chip(
            "hwmon2",
            "coretemp",
            &[("temp1_input", "41000"), ("temp1_label", "Package id 0")],
        );
        let k10temp = tree.chip(
            "hwmon4",
            "k10temp",
            &[
                ("temp1_input", "55000"),
                ("temp1_label", "Tctl"),
                ("temp3_input", "52000"),
                ("temp3_label", "Tccd1"),
            ],
        );
        let chips = chips(tree.path());
        assert_eq!(
            find_temp(&chips, &CPU_TEMP_SOURCES),
            Some(k10temp.join("temp1_input"))
        );
        let mut sensors = Sensors::with_root(&config(&[]), tree.path());
        assert_eq!(sensors.snapshot().cpu_temp, Some(55.0));
    }

    #[test]
    fn gpu_temperature_prefers_the_junction() {
        let tree = Tree::new();
        let amdgpu = tree.chip(
            "hwmon5",
            "amdgpu",
            &[
                ("temp1_input", "60000"),
                ("temp1_label", "edge"),
                ("temp2_input", "72000"),
                ("temp2_label", "junction"),
                ("device/gpu_busy_percent", "68"),
            ],
        );
        let chips = chips(tree.path());
        assert_eq!(
            find_temp(&chips, &GPU_TEMP_SOURCES),
            Some(amdgpu.join("temp2_input"))
        );
        let mut sensors = Sensors::with_root(&config(&[]), tree.path());
        let snapshot = sensors.snapshot();
        assert_eq!(snapshot.gpu_temp, Some(72.0));
        assert_eq!(snapshot.gpu_usage, Some(68.0));
    }

    #[test]
    fn amd_gpu_names_come_from_the_libdrm_table() {
        let tree = Tree::new();
        let amdgpu = tree.chip(
            "hwmon5",
            "amdgpu",
            &[("device/device", "0x7550"), ("device/revision", "0xc0")],
        );
        let table = tree.path().join("amdgpu.ids");
        fs::write(
            &table,
            "# AMD GPU IDs\n7550,\tC0,\tAMD Radeon RX 9070 XT\n7550,\tC3,\tAMD Radeon RX 9070\n",
        )
        .unwrap();
        assert_eq!(
            amd_gpu_name(&amdgpu, &table).as_deref(),
            Some("Radeon RX 9070 XT")
        );
        assert_eq!(amd_gpu_name(&amdgpu, Path::new("/nonexistent")), None);
    }

    #[test]
    fn the_power_supply_is_named_and_rated_from_its_uevent() {
        let tree = Tree::new();
        tree.chip(
            "hwmon6",
            "corsairpsu",
            &[
                (
                    "device/uevent",
                    "HID_NAME=CORSAIR HX1200i Power Supply\nHID_PHYS=usb-0000:0e:00.3-4/input0",
                ),
                ("power1_input", "317000000"),
            ],
        );
        let mut sensors = Sensors::with_root(&config(&[]), tree.path());
        let snapshot = sensors.snapshot();
        assert_eq!(snapshot.names.psu, "CORSAIR HX1200i");
        assert_eq!(snapshot.psu_power, Some(317.0));
        assert_eq!(snapshot.psu_usage, Some(317.0 * 100.0 / 1200.0));
    }

    #[test]
    fn configured_names_and_rating_win_over_detected_ones() {
        let tree = Tree::new();
        tree.chip(
            "hwmon6",
            "corsairpsu",
            &[
                ("device/uevent", "HID_NAME=CORSAIR HX1200i Power Supply"),
                ("power1_input", "600000000"),
            ],
        );
        let mut config = config(&[]);
        config.names = Names {
            cpu: "Desk".to_owned(),
            gpu: String::new(),
            psu: "Brick".to_owned(),
        };
        config.psu_rating = Some(1000.0);
        let mut sensors = Sensors::with_root(&config, tree.path());
        let snapshot = sensors.snapshot();
        assert_eq!(snapshot.names.cpu, "Desk");
        assert_eq!(snapshot.names.psu, "Brick");
        // Nothing detects a GPU here, so the fallback stands.
        assert_eq!(snapshot.names.gpu, "GPU");
        assert_eq!(snapshot.psu_usage, Some(60.0));
    }

    #[test]
    fn a_chip_that_re_registers_is_found_again() {
        let tree = Tree::new();
        let old = tree.chip(
            "hwmon1",
            "k10temp",
            &[("temp1_input", "55000"), ("temp1_label", "Tctl")],
        );
        let mut sensors = Sensors::with_root(&config(&[]), tree.path());
        assert_eq!(sensors.snapshot().cpu_temp, Some(55.0));

        // The chip goes, and comes back under another number.
        fs::remove_dir_all(&old).unwrap();
        let new = tree.chip(
            "hwmon7",
            "k10temp",
            &[("temp1_input", "57000"), ("temp1_label", "Tctl")],
        );
        assert_eq!(sensors.snapshot().cpu_temp, None);
        assert!(sensors.lost);
        // Not walked again straight away: the walk is rate limited.
        let walked = sensors.discovered;
        assert_eq!(sensors.snapshot().cpu_temp, None);
        assert_eq!(sensors.discovered, walked);

        walked_ago(&mut sensors, REDISCOVER_LOST_AFTER);
        assert_eq!(sensors.snapshot().cpu_temp, Some(57.0));
        assert_eq!(sensors.inputs.cpu_temp, Some(new.join("temp1_input")));
        assert!(!sensors.lost);
        assert!(sensors.discovered > walked);
    }

    #[test]
    fn a_sensor_missing_at_start_is_looked_for_again() {
        let tree = Tree::new();
        let mut sensors = Sensors::with_root(&config(&[]), tree.path());
        assert_eq!(sensors.snapshot().cpu_temp, None);

        // The driver is loaded after the daemon started.
        tree.chip(
            "hwmon0",
            "k10temp",
            &[("temp1_input", "48000"), ("temp1_label", "Tctl")],
        );
        // A missing sensor is looked for again less often than a lost one.
        walked_ago(&mut sensors, REDISCOVER_LOST_AFTER);
        assert_eq!(sensors.snapshot().cpu_temp, None);
        walked_ago(&mut sensors, REDISCOVER_MISSING_EVERY);
        assert_eq!(sensors.snapshot().cpu_temp, Some(48.0));
    }

    #[test]
    fn a_configured_fan_that_is_missing_keeps_the_tree_watched() {
        let tree = Tree::new();
        tree.chip("hwmon1", "nct6799", &[("fan1_input", "900")]);
        let config = config(&[("nct6799/fan1", "front"), ("corsairpsu/fan1", "psu")]);
        let sensors = Sensors::with_root(&config, tree.path());
        assert!(sensors.inputs.incomplete(&sensors.selection));
        assert_eq!(sensors.inputs.fans.len(), 1);
    }

    #[test]
    fn rating_from_model_name() {
        assert_eq!(rating_from_model("CORSAIR HX1200i"), Some(1200.0));
        assert_eq!(rating_from_model("Corsair RM850i ATX 3.1"), Some(850.0));
        assert_eq!(rating_from_model("CORSAIR"), None);
    }
}
