//! ประเมินเวลาชาร์จจาก UPS หรืออัตราที่เรียนรู้ โดยไม่ใช้ข้อมูลคายประจุ

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const DEFAULT_CHARGE_FULL_SECONDS: u64 = 14400;
pub const MIN_CHARGE_FULL_SECONDS: u64 = 1200;
pub const MAX_TIME_TO_FULL_SECONDS: u64 = 72000;

const MIN_SECONDS_PER_PERCENT: f64 = MIN_CHARGE_FULL_SECONDS as f64 / 100.0;
const CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const PERSIST_INTERVAL: Duration = Duration::from_secs(60);
const MAX_STATE_BYTES: u64 = 4096;
const STATE_VERSION: &str = "nut-power-bridge-charge-rate-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChargeSource {
    UpsTime,
    UpsCurrent,
    Learned,
    Fallback,
}

pub struct ChargeEstimate {
    pub seconds: i32,
    pub source: Option<ChargeSource>,
}

pub struct ChargingEstimator {
    state_path: PathBuf,
    identity: String,
    fallback_full_seconds: u64,
    max_sample_gap: Duration,
    learned_seconds_per_percent: Option<f64>,
    learned_at: Option<SystemTime>,
    session: Option<ChargingSession>,
    last_persist_attempt: Option<Instant>,
}

struct ChargingSession {
    capacity: i32,
    last_sample: Instant,
    last_wall: SystemTime,
    anchor: Option<(i32, Instant)>,
}

impl ChargingEstimator {
    pub fn new(
        state_path: PathBuf,
        identity: String,
        fallback_full_seconds: u64,
        max_sample_gap: Duration,
    ) -> Self {
        let learned = load_rate(&state_path, &identity);
        Self {
            state_path,
            identity,
            fallback_full_seconds: fallback_full_seconds
                .clamp(MIN_CHARGE_FULL_SECONDS, MAX_TIME_TO_FULL_SECONDS),
            max_sample_gap,
            learned_seconds_per_percent: learned.map(|(rate, _)| rate),
            learned_at: learned.map(|(_, timestamp)| timestamp),
            session: None,
            last_persist_attempt: None,
        }
    }

    pub fn update(
        &mut self,
        capacity: i32,
        status: i32,
        ac_online: i32,
        ups_estimate: Option<(f64, ChargeSource)>,
    ) -> ChargeEstimate {
        if status != 1 || ac_online != 1 || !(0..100).contains(&capacity) {
            self.reset_session();
            return ChargeEstimate {
                seconds: 0,
                source: None,
            };
        }

        if self.learned_at.map_or(false, |timestamp| {
            SystemTime::now()
                .duration_since(timestamp)
                .map_or(true, |age| age > CACHE_MAX_AGE)
        }) {
            self.learned_seconds_per_percent = None;
            self.learned_at = None;
        }
        self.observe(capacity);
        let remaining = f64::from(100 - capacity);
        let (seconds, source) = match ups_estimate {
            Some((seconds, source))
                if seconds.is_finite()
                    && seconds > 0.0
                    && matches!(source, ChargeSource::UpsTime | ChargeSource::UpsCurrent) =>
            {
                (seconds, source)
            }
            _ => match self.learned_seconds_per_percent {
                Some(rate) => (remaining * rate, ChargeSource::Learned),
                None => (
                    remaining * self.fallback_full_seconds as f64 / 100.0,
                    ChargeSource::Fallback,
                ),
            },
        };

        // ปัดขึ้นเพื่อไม่ให้อัตราบนสเกลพลังงาน 100 Wh เกิน 300 W
        ChargeEstimate {
            seconds: seconds
                .clamp(
                    remaining * MIN_SECONDS_PER_PERCENT,
                    MAX_TIME_TO_FULL_SECONDS as f64,
                )
                .ceil() as i32,
            source: Some(source),
        }
    }

    /// ล้างเฉพาะช่วงสังเกตเมื่อข้อมูลขาดตอน โดยเก็บอัตราที่เรียนรู้ไว้
    pub fn reset_session(&mut self) {
        self.session = None;
    }

    fn observe(&mut self, capacity: i32) {
        let now = Instant::now();
        let wall = SystemTime::now();
        let fresh = ChargingSession {
            capacity,
            last_sample: now,
            last_wall: wall,
            anchor: None,
        };
        let mut session = match self.session.take() {
            Some(session) => session,
            None => {
                self.session = Some(fresh);
                return;
            }
        };

        let gap = now.duration_since(session.last_sample);
        let wall_gap = wall.duration_since(session.last_wall);
        let clock_slack = Duration::from_secs(2);
        let continuous = match wall_gap {
            Ok(wall_gap) => {
                gap <= self.max_sample_gap
                    && wall_gap <= self.max_sample_gap
                    && wall_gap <= gap.saturating_add(clock_slack)
                    && gap <= wall_gap.saturating_add(clock_slack)
            }
            Err(_) => false,
        };
        let increase = capacity - session.capacity;
        if !continuous || !(0..=5).contains(&increase) {
            // ตรวจ wall clock ด้วย เพราะ monotonic clock อาจไม่นับเวลาที่ suspend
            self.session = Some(fresh);
            return;
        }

        let mut sample = None;
        if increase > 0 {
            if let Some((start_capacity, started)) = session.anchor {
                let points = capacity - start_capacity;
                let elapsed = now.duration_since(started).as_secs_f64();
                if points >= 2 && elapsed >= 60.0 {
                    let rate = elapsed / f64::from(points);
                    if valid_rate(rate) {
                        sample = Some(rate);
                    }
                    session.anchor = Some((capacity, now));
                }
            } else {
                // ข้ามช่วงเปอร์เซ็นต์แรก เพราะอาจเริ่มสังเกตกลางช่วงนั้น
                session.anchor = Some((capacity, now));
            }
        }
        session.capacity = capacity;
        session.last_sample = now;
        session.last_wall = wall;
        self.session = Some(session);

        if let Some(rate) = sample {
            let smoothed = match self.learned_seconds_per_percent {
                Some(previous) => previous * 0.75 + rate * 0.25,
                None => rate,
            };
            self.learned_seconds_per_percent = Some(smoothed);
            self.learned_at = Some(wall);
            self.persist_rate(smoothed, now, wall);
        }
    }

    fn persist_rate(&mut self, rate: f64, now: Instant, wall: SystemTime) {
        if self
            .last_persist_attempt
            .map_or(false, |last| now.duration_since(last) < PERSIST_INTERVAL)
        {
            return;
        }
        self.last_persist_attempt = Some(now);
        if let Err(error) = save_rate(&self.state_path, &self.identity, rate, wall) {
            eprintln!("Could not persist learned charging rate: {error}");
        }
    }
}

fn valid_rate(rate: f64) -> bool {
    rate.is_finite()
        && (MIN_SECONDS_PER_PERCENT..=MAX_TIME_TO_FULL_SECONDS as f64).contains(&rate)
}

fn identity_key(identity: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut key = String::with_capacity(identity.len() * 2);
    for byte in identity.bytes() {
        key.push(HEX[(byte >> 4) as usize] as char);
        key.push(HEX[(byte & 15) as usize] as char);
    }
    key
}

fn load_rate(path: &Path, identity: &str) -> Option<(f64, SystemTime)> {
    let mut text = String::new();
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            eprintln!("Could not read charging state {}: {error}", path.display());
            return None;
        }
    };
    if let Err(error) = file
        .take(MAX_STATE_BYTES + 1)
        .read_to_string(&mut text)
    {
        eprintln!("Could not read charging state {}: {error}", path.display());
        return None;
    }
    if text.len() as u64 > MAX_STATE_BYTES {
        return None;
    }
    let mut lines = text.lines();
    if lines.next()? != STATE_VERSION
        || lines.next()?.strip_prefix("identity=")? != identity_key(identity)
    {
        return None;
    }
    let timestamp = lines
        .next()?
        .strip_prefix("timestamp=")?
        .parse::<u64>()
        .ok()?;
    let rate = lines
        .next()?
        .strip_prefix("seconds_per_percent=")?
        .parse::<f64>()
        .ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if lines.next().is_some()
        || !valid_rate(rate)
        || timestamp > now
        || now - timestamp > CACHE_MAX_AGE.as_secs()
    {
        return None;
    }
    Some((rate, UNIX_EPOCH.checked_add(Duration::from_secs(timestamp))?))
}

fn save_rate(path: &Path, identity: &str, rate: f64, wall: SystemTime) -> io::Result<()> {
    let timestamp = wall
        .duration_since(UNIX_EPOCH)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let text = format!(
        "{STATE_VERSION}\nidentity={}\ntimestamp={}\nseconds_per_percent={rate:.17}\n",
        identity_key(identity),
        timestamp.as_secs(),
    );
    if text.len() as u64 > MAX_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Charging state exceeds the size limit",
        ));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    let filename = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "Charging state needs a filename")
    })?;

    for attempt in 0..8 {
        let mut temporary_name = filename.to_os_string();
        temporary_name.push(format!(
            ".{}.{}.{attempt}.tmp",
            std::process::id(),
            timestamp.as_nanos(),
        ));
        let temporary_path = parent.join(temporary_name);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let result = (|| {
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary_path, path)?;
            File::open(parent)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        return result;
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "Could not create a unique charging state temporary file",
    ))
}
