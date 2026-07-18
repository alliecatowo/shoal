use std::time::{Duration, Instant};

use shoal_prompt::{BatterySnapshot, PromptConfig};

const MAX_SAMPLE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Once-per-refresh battery producer. The cache keeps platform discovery and
/// sampling completely outside Reedline's per-keystroke renderer.
pub(crate) struct BatterySampler {
    enabled: bool,
    interval: Duration,
    next_refresh: Option<Instant>,
    cached: Option<BatterySnapshot>,
    #[cfg(target_os = "linux")]
    power_supply_root: std::path::PathBuf,
}

impl BatterySampler {
    pub(crate) fn new(config: &PromptConfig, warnings: &mut Vec<String>) -> Self {
        let requested = Duration::from_secs(config.module.battery.sample_interval_s);
        let interval = requested.min(MAX_SAMPLE_INTERVAL);
        if requested > MAX_SAMPLE_INTERVAL {
            warnings.push(format!(
                "prompt.module.battery.sample_interval_s exceeds {}; clamped to {}",
                MAX_SAMPLE_INTERVAL.as_secs(),
                MAX_SAMPLE_INTERVAL.as_secs()
            ));
        }
        Self {
            enabled: config.module.battery.enabled,
            interval,
            next_refresh: None,
            cached: None,
            #[cfg(target_os = "linux")]
            power_supply_root: "/sys/class/power_supply".into(),
        }
    }

    pub(crate) fn sample(&mut self) -> Option<BatterySnapshot> {
        if !self.enabled {
            return None;
        }
        let now = Instant::now();
        if self.next_refresh.is_some_and(|deadline| now < deadline) {
            return self.cached.clone();
        }
        self.cached = sample_platform(self);
        self.next_refresh = now.checked_add(self.interval);
        self.cached.clone()
    }
}

#[cfg(target_os = "linux")]
fn sample_platform(sampler: &BatterySampler) -> Option<BatterySnapshot> {
    use std::fs::{self, File};
    use std::io::Read;

    const MAX_SUPPLIES: usize = 64;
    const MAX_ATTRIBUTE_BYTES: u64 = 64;

    fn attribute(path: &std::path::Path) -> Option<String> {
        let file = File::open(path).ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        let mut bytes = Vec::with_capacity(MAX_ATTRIBUTE_BYTES as usize + 1);
        file.take(MAX_ATTRIBUTE_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > MAX_ATTRIBUTE_BYTES {
            return None;
        }
        String::from_utf8(bytes)
            .ok()
            .map(|value| value.trim().into())
    }

    let mut total = 0u16;
    let mut count = 0u16;
    let mut charging = false;
    for entry in fs::read_dir(&sampler.power_supply_root)
        .ok()?
        .take(MAX_SUPPLIES)
        .flatten()
    {
        let root = entry.path();
        if attribute(&root.join("type")).as_deref() != Some("Battery") {
            continue;
        }
        let Some(pct) =
            attribute(&root.join("capacity")).and_then(|value| value.parse::<u8>().ok())
        else {
            continue;
        };
        if pct > 100 {
            continue;
        }
        let status = attribute(&root.join("status")).unwrap_or_default();
        charging |= matches!(status.as_str(), "Charging" | "Full");
        total += u16::from(pct);
        count += 1;
    }
    (count > 0).then(|| BatterySnapshot {
        pct: ((total + count / 2) / count) as u8,
        charging,
    })
}

#[cfg(target_os = "macos")]
fn sample_platform(_sampler: &BatterySampler) -> Option<BatterySnapshot> {
    let mut command = std::process::Command::new("/usr/bin/pmset");
    command.args(["-g", "batt"]);
    let output =
        shoal_exec::run_bounded_command(&mut command, Duration::from_millis(250), 16 * 1024)
            .ok()?;
    if output.timed_out || output.truncated || !output.status.success() {
        return None;
    }
    parse_pmset(&String::from_utf8(output.stdout).ok()?)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sample_platform(_sampler: &BatterySampler) -> Option<BatterySnapshot> {
    None
}

#[cfg(any(target_os = "macos", test))]
fn parse_pmset(output: &str) -> Option<BatterySnapshot> {
    let mut total = 0u16;
    let mut count = 0u16;
    let mut charging = false;
    for line in output.lines() {
        let Some((prefix, _)) = line.split_once('%') else {
            continue;
        };
        let digits = prefix
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        let pct: u8 = digits.parse().ok()?;
        if pct > 100 {
            continue;
        }
        charging |= line
            .split(';')
            .map(|part| part.trim().to_ascii_lowercase())
            .any(|part| part == "charging" || part == "charged");
        total += u16::from(pct);
        count += 1;
    }
    (count > 0).then(|| BatterySnapshot {
        pct: ((total + count / 2) / count) as u8,
        charging,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pmset_parser_distinguishes_charging_from_discharging() {
        let battery = parse_pmset(" -InternalBattery-0\t76%; discharging; 4:15 remaining").unwrap();
        assert_eq!(battery.pct, 76);
        assert!(!battery.charging);
        let battery = parse_pmset(" -InternalBattery-0\t81%; charging; 0:40 remaining").unwrap();
        assert!(battery.charging);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sampler_is_bounded_cached_and_averages_batteries() {
        let root = tempfile::tempdir().unwrap();
        for (name, pct, status) in [("BAT0", "40", "Discharging"), ("BAT1", "80", "Charging")] {
            let battery = root.path().join(name);
            std::fs::create_dir(&battery).unwrap();
            std::fs::write(battery.join("type"), "Battery\n").unwrap();
            std::fs::write(battery.join("capacity"), pct).unwrap();
            std::fs::write(battery.join("status"), status).unwrap();
        }
        let mut config = PromptConfig::default();
        config.module.battery.enabled = true;
        config.module.battery.sample_interval_s = 60;
        let mut sampler = BatterySampler::new(&config, &mut Vec::new());
        sampler.power_supply_root = root.path().into();
        let first = sampler.sample().unwrap();
        assert_eq!(first.pct, 60);
        assert!(first.charging);

        std::fs::write(root.path().join("BAT0/capacity"), "0").unwrap();
        assert_eq!(sampler.sample().unwrap().pct, 60, "cached until TTL");
    }
}
