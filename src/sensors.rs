//! System readings taken straight from /sys/class/hwmon and /proc.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;

/// (chip name, temperature label) pairs tried in order.
const CPU_TEMP_SOURCES: [(&str, &str); 3] = [
    ("k10temp", "Tctl"),
    ("coretemp", "Package id 0"),
    ("zenpower", "Tdie"),
];
/// libdrm's table of AMD marketing names: "device id, revision, name" rows.
const AMDGPU_IDS: &str = "/usr/share/libdrm/amdgpu.ids";
const GPU_TEMP_SOURCES: [(&str, &str); 2] = [("amdgpu", "edge"), ("nouveau", "")];

#[derive(Debug)]
pub struct Fan<'a> {
    pub label: &'a str,
    pub rpm: u32,
}

/// Product names shown above each gauge.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Names {
    pub cpu: String,
    pub gpu: String,
    pub psu: String,
}

/// One reading of every sensor. What cannot change while running (names,
/// kernel, fan labels) is borrowed from `Sensors`, which works it out once.
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

struct FanInput {
    path: PathBuf,
    label: String,
    always_shown: bool,
    /// Position in the config, so the display follows the order written there.
    order: usize,
}

pub struct Sensors {
    names: Names,
    kernel: String,
    // sysfs files read on every snapshot.
    cpu_temp_input: Option<PathBuf>,
    gpu_temp_input: Option<PathBuf>,
    gpu_usage_input: Option<PathBuf>,
    psu_power_input: Option<PathBuf>,
    psu_rating: Option<f32>,
    fans: Vec<FanInput>,
    /// (busy, total) jiffies of the previous /proc/stat sample.
    previous_cpu_times: Option<(u64, u64)>,
}

fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    Some(fs::read_to_string(path).ok()?.trim().to_owned())
}

fn read_number(path: impl AsRef<Path>) -> Option<f32> {
    read_trimmed(path)?.parse().ok()
}

fn find_temp(chips: &[(String, PathBuf)], sources: &[(&str, &str)]) -> Option<PathBuf> {
    sources.iter().find_map(|&(chip, label)| {
        let (_, dir) = chips.iter().find(|(name, _)| name == chip)?;
        (1..=16).find_map(|index| {
            let input = dir.join(format!("temp{index}_input"));
            let label_path = dir.join(format!("temp{index}_label"));
            (input.exists() && read_trimmed(label_path).unwrap_or_default() == label)
                .then_some(input)
        })
    })
}

/// "AMD Ryzen 9 9900X3D 12-Core Processor" becomes "Ryzen 9 9900X3D".
fn cpu_name() -> Option<String> {
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").ok()?;
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
    Some(words.join(" "))
}

/// Looks the PCI device and revision of an amdgpu hwmon chip up in libdrm's table.
fn amd_gpu_name(hwmon: &Path) -> Option<String> {
    let id = |file: &str| {
        let text = read_trimmed(hwmon.join("device").join(file))?;
        u32::from_str_radix(text.trim_start_matches("0x"), 16).ok()
    };
    let (device, revision) = (id("device")?, id("revision")?);
    let table = fs::read_to_string(AMDGPU_IDS).ok()?;
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

impl Sensors {
    pub fn new(config: &Config) -> Self {
        let (fan_labels, names) = (&config.fans, &config.names);
        let mut chips: Vec<(String, PathBuf)> = fs::read_dir("/sys/class/hwmon")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| Some((read_trimmed(entry.path().join("name"))?, entry.path())))
            .collect();
        chips.sort();

        let mut fans = Vec::new();
        for (chip, dir) in &chips {
            let inputs: Vec<(u32, PathBuf)> = (1..=16)
                .map(|index| (index, dir.join(format!("fan{index}_input"))))
                .filter(|(_, path)| path.exists())
                .collect();
            // A lone fan (GPU, PSU) is better named after its chip than "fan1".
            let lone = inputs.len() == 1;
            for (index, path) in inputs {
                let configured = fan_labels.get_full(&format!("{chip}/fan{index}"));
                // Once fans are named in the config, that list is the selection.
                if configured.is_none() && !fan_labels.is_empty() {
                    continue;
                }
                let label = configured
                    .map(|(_, _, label)| label.clone())
                    .or_else(|| read_trimmed(dir.join(format!("fan{index}_label"))))
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
                    always_shown: configured.is_some(),
                    order: configured.map_or(usize::MAX, |(position, _, _)| position),
                });
            }
        }

        fans.sort_by_key(|fan| fan.order);

        let gpu_temp_input = find_temp(&chips, &GPU_TEMP_SOURCES);
        let gpu_usage_input = gpu_temp_input
            .as_ref()
            .and_then(|temp| Some(temp.parent()?.join("device/gpu_busy_percent")))
            .filter(|path| path.exists());

        let psu = chips.iter().find(|(name, _)| name == "corsairpsu");
        let or_detected = |configured: &String, detected: Option<String>, fallback: &str| {
            if configured.is_empty() {
                detected.unwrap_or_else(|| fallback.to_owned())
            } else {
                configured.clone()
            }
        };
        let gpu_chip = gpu_temp_input.as_ref().and_then(|temp| temp.parent());
        let psu_model = psu.and_then(|(_, dir)| psu_name(dir));
        let psu_rating = config
            .psu_rating
            .or_else(|| psu_model.as_deref().and_then(rating_from_model));
        let names = Names {
            cpu: or_detected(&names.cpu, cpu_name(), "CPU"),
            gpu: or_detected(&names.gpu, gpu_chip.and_then(amd_gpu_name), "GPU"),
            psu: or_detected(&names.psu, psu_model, "PSU"),
        };
        let psu_power_input = psu
            .map(|(_, dir)| dir.join("power1_input"))
            .filter(|path| path.exists());

        // The same release string `uname -r` prints.
        let kernel = read_trimmed("/proc/sys/kernel/osrelease")
            .and_then(|release| Some(release.split('-').next()?.to_owned()))
            .unwrap_or_default();

        Self {
            names,
            kernel,
            psu_power_input,
            psu_rating,
            cpu_temp_input: find_temp(&chips, &CPU_TEMP_SOURCES),
            gpu_temp_input,
            gpu_usage_input,
            fans,
            previous_cpu_times: None,
        }
    }

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

    pub fn snapshot(&mut self) -> Snapshot<'_> {
        let cpu_usage = self.cpu_usage();
        let millidegrees = |path: &Option<PathBuf>| Some(read_number(path.as_ref()?)? / 1000.0);
        let mut load_average = [0.0; 3];
        if let Some(loadavg) = read_trimmed("/proc/loadavg") {
            for (slot, field) in load_average.iter_mut().zip(loadavg.split_whitespace()) {
                *slot = field.parse().unwrap_or(0.0);
            }
        }

        // hwmon reports power in microwatts.
        let psu_power = self
            .psu_power_input
            .as_ref()
            .and_then(read_number)
            .map(|microwatts| microwatts / 1e6);

        Snapshot {
            names: &self.names,
            hostname: read_trimmed("/proc/sys/kernel/hostname").unwrap_or_default(),
            kernel: &self.kernel,
            load_average,
            cpu_temp: millidegrees(&self.cpu_temp_input),
            cpu_usage,
            gpu_temp: millidegrees(&self.gpu_temp_input),
            gpu_usage: self.gpu_usage_input.as_ref().and_then(read_number),
            psu_power,
            psu_usage: psu_power
                .zip(self.psu_rating)
                .map(|(watts, rating)| watts * 100.0 / rating),
            fans: self
                .fans
                .iter()
                .filter_map(|fan| {
                    let rpm = read_number(&fan.path)? as u32;
                    (rpm > 0 || fan.always_shown).then_some(Fan {
                        label: &fan.label,
                        rpm,
                    })
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rating_from_model_name() {
        assert_eq!(rating_from_model("CORSAIR HX1200i"), Some(1200.0));
        assert_eq!(rating_from_model("Corsair RM850i ATX 3.1"), Some(850.0));
        assert_eq!(rating_from_model("CORSAIR"), None);
    }
}
