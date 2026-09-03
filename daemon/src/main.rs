mod charging;

use charging::{
    ChargeSource, ChargingEstimator, DEFAULT_CHARGE_FULL_SECONDS,
    MAX_TIME_TO_FULL_SECONDS, MIN_CHARGE_FULL_SECONDS,
};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process;
use std::thread;
use std::time::Duration;

const DEFAULT_HOST: &str = "127.0.0.1:3493";
const DEFAULT_UPS_NAME: &str = "ups";
const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 2;
const DEFAULT_TIMEOUT_SECONDS: u64 = 5;
const DEFAULT_SYSFS_PATH: &str = "/sys/kernel/nut_battery/update";
const DEFAULT_CHARGE_STATE_PATH: &str = "/var/lib/nut-power-bridge/charge-rate";
const MAX_RESPONSE_LENGTH: usize = 4096;

struct Config {
    host: String,
    ups_name: String,
    poll_interval: Duration,
    timeout: Duration,
    sysfs_path: PathBuf,
    charge_full_seconds: u64,
    charge_state_path: PathBuf,
    time_to_full_var: Option<String>,
}

impl Config {
    fn from_env_and_args() -> Result<Self, String> {
        let mut host = env_string("NUT_HOST", DEFAULT_HOST)?;
        let mut ups_name = env_string("NUT_UPS_NAME", DEFAULT_UPS_NAME)?;
        let mut poll_interval_seconds = env_u64(
            "NUT_POLL_INTERVAL_SECONDS",
            DEFAULT_POLL_INTERVAL_SECONDS,
        )?;
        let mut timeout_seconds = env_u64("NUT_TIMEOUT_SECONDS", DEFAULT_TIMEOUT_SECONDS)?;
        let mut sysfs_path = env::var_os("NUT_SYSFS_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_SYSFS_PATH));
        let charge_full_seconds = env_u64(
            "NUT_CHARGE_FULL_SECONDS",
            DEFAULT_CHARGE_FULL_SECONDS,
        )?;
        let charge_state_path = env::var_os("NUT_CHARGE_STATE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CHARGE_STATE_PATH));
        let time_to_full_var = env_string("NUT_TIME_TO_FULL_VAR", "")?;

        let mut args = env::args().skip(1);
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--host" => host = next_argument(&mut args, "--host")?,
                "--ups" => ups_name = next_argument(&mut args, "--ups")?,
                "--interval" => {
                    poll_interval_seconds = parse_positive_u64(
                        &next_argument(&mut args, "--interval")?,
                        "--interval",
                    )?
                }
                "--timeout" => {
                    timeout_seconds = parse_positive_u64(
                        &next_argument(&mut args, "--timeout")?,
                        "--timeout",
                    )?
                }
                "--sysfs-path" => {
                    sysfs_path = PathBuf::from(next_argument(&mut args, "--sysfs-path")?)
                }
                "-h" | "--help" => {
                    println!(
                        "Usage: nut-power-bridge [--host HOST:PORT] [--ups NAME] \
                         [--interval SECONDS] [--timeout SECONDS] [--sysfs-path PATH]\n\
                         Environment: NUT_HOST, NUT_UPS_NAME, \
                         NUT_POLL_INTERVAL_SECONDS, NUT_TIMEOUT_SECONDS, NUT_SYSFS_PATH, \
                         NUT_CHARGE_FULL_SECONDS, NUT_CHARGE_STATE_PATH, NUT_TIME_TO_FULL_VAR"
                    );
                    process::exit(0);
                }
                _ => return Err(format!("unknown argument: {argument}")),
            }
        }

        if host.is_empty() || host.chars().any(char::is_control) {
            return Err("NUT host must be a non-empty single-line value".to_owned());
        }
        validate_protocol_token(&ups_name, "UPS name")?;
        if sysfs_path.as_os_str().is_empty() {
            return Err("sysfs path must not be empty".to_owned());
        }
        if !(MIN_CHARGE_FULL_SECONDS..=MAX_TIME_TO_FULL_SECONDS)
            .contains(&charge_full_seconds)
        {
            return Err(format!(
                "NUT_CHARGE_FULL_SECONDS must be between {MIN_CHARGE_FULL_SECONDS} \
                 and {MAX_TIME_TO_FULL_SECONDS} seconds"
            ));
        }
        if charge_state_path.as_os_str().is_empty() || charge_state_path.file_name().is_none() {
            return Err("NUT_CHARGE_STATE_PATH must name a state file".to_owned());
        }
        let time_to_full_var = if time_to_full_var.is_empty() {
            None
        } else {
            validate_protocol_token(&time_to_full_var, "NUT_TIME_TO_FULL_VAR")?;
            if matches!(
                time_to_full_var.as_str(),
                "battery.runtime" | "battery.runtime.low" | "battery.runtime.restart"
            ) {
                return Err("NUT_TIME_TO_FULL_VAR must describe charging time, not discharge runtime".to_owned());
            }
            Some(time_to_full_var)
        };
        Ok(Self {
            host,
            ups_name,
            poll_interval: Duration::from_secs(poll_interval_seconds),
            timeout: Duration::from_secs(timeout_seconds),
            sysfs_path,
            charge_full_seconds,
            charge_state_path,
            time_to_full_var,
        })
    }
}

#[derive(Clone, Copy)]
struct Snapshot {
    capacity: i32,
    voltage_now_uv: i32,
    time_to_empty_seconds: i32,
    status: i32,
    ac_online: i32,
    time_to_full_seconds: i32,
    charge_source: Option<ChargeSource>,
}

fn main() {
    let config = match Config::from_env_and_args() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("configuration error: {error}");
            process::exit(2);
        }
    };

    let mut last_published_snapshot = None;
    let mut degraded_published = false;
    let mut charging = ChargingEstimator::new(
        config.charge_state_path.clone(),
        format!("{} {}", config.host, config.ups_name),
        config.charge_full_seconds,
        config.poll_interval.saturating_mul(3).max(Duration::from_secs(15)),
    );

    if !kernel_supports_charge_time(&config.sysfs_path) {
        eprintln!(
            "kernel module uses the legacy snapshot format; battery updates continue, \
             but charging-time support requires reloading nut_power or rebooting after installation"
        );
    }

    loop {
        match connect(&config) {
            Ok(stream) => {
                eprintln!("connected to NUT at {}", config.host);
                poll_connection(
                    stream,
                    &config,
                    &mut last_published_snapshot,
                    &mut degraded_published,
                    &mut charging,
                );
            }
            Err(error) => {
                eprintln!("cannot connect to NUT at {}: {error}", config.host);
                charging.reset_session();
                publish_degraded_once(
                    &config,
                    last_published_snapshot,
                    &mut degraded_published,
                );
            }
        }

        thread::sleep(config.poll_interval);
    }
}

fn poll_connection(
    stream: TcpStream,
    config: &Config,
    last_published_snapshot: &mut Option<Snapshot>,
    degraded_published: &mut bool,
    charging: &mut ChargingEstimator,
) {
    let mut reader = BufReader::new(stream);

    loop {
        match fetch_snapshot(&mut reader, config, *last_published_snapshot, charging) {
            Ok(snapshot) => {
                if let Err(error) = write_snapshot(config, &snapshot) {
                    eprintln!(
                        "cannot update {}: {error}",
                        config.sysfs_path.display()
                    );
                } else {
                    if snapshot.charge_source
                        != last_published_snapshot.and_then(|previous| previous.charge_source)
                    {
                        if let Some(source) = snapshot.charge_source {
                            let description = match source {
                                ChargeSource::UpsTime => "UPS time-to-full variable",
                                ChargeSource::UpsCurrent => "UPS battery current and capacity",
                                ChargeSource::Learned => "learned charging rate",
                                ChargeSource::Fallback => "configured fallback charging rate",
                            };
                            eprintln!(
                                "charging time source: {description}; time to full: {} seconds",
                                snapshot.time_to_full_seconds
                            );
                        }
                    }
                    *last_published_snapshot = Some(snapshot);
                    *degraded_published = false;
                }
            }
            Err(error) => {
                eprintln!("NUT connection lost or returned invalid data: {error}");
                charging.reset_session();
                publish_degraded_once(config, *last_published_snapshot, degraded_published);
                return;
            }
        }

        thread::sleep(config.poll_interval);
    }
}

fn publish_degraded_once(
    config: &Config,
    last_published_snapshot: Option<Snapshot>,
    degraded_published: &mut bool,
) {
    if *degraded_published {
        return;
    }

    let mut snapshot = match last_published_snapshot {
        Some(snapshot) => snapshot,
        None => return,
    };
    snapshot.status = 0;
    snapshot.time_to_full_seconds = 0;
    snapshot.charge_source = None;

    match write_snapshot(config, &snapshot) {
        Ok(()) => *degraded_published = true,
        Err(error) => eprintln!(
            "cannot mark {} as unavailable: {error}",
            config.sysfs_path.display()
        ),
    }
}

fn connect(config: &Config) -> io::Result<TcpStream> {
    let addresses = config.host.to_socket_addrs()?;
    let mut last_error = None;

    for address in addresses {
        match TcpStream::connect_timeout(&address, config.timeout) {
            Ok(stream) => {
                stream.set_read_timeout(Some(config.timeout))?;
                stream.set_write_timeout(Some(config.timeout))?;
                return Ok(stream);
            }
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::AddrNotAvailable, "host resolved to no addresses")
    }))
}

fn fetch_snapshot(
    reader: &mut BufReader<TcpStream>,
    config: &Config,
    last_snapshot: Option<Snapshot>,
    charging: &mut ChargingEstimator,
) -> io::Result<Snapshot> {
    let ups_name = &config.ups_name;
    let capacity = parse_scaled_integer(
        &get_required_var(reader, ups_name, "battery.charge")?,
        1.0,
        0,
        100,
        "battery.charge",
    )?;
    let time_to_empty_seconds = get_optional_scaled_integer(
        reader,
        ups_name,
        "battery.runtime",
        1.0,
        0,
        i32::MAX,
        last_snapshot
            .map(|snapshot| snapshot.time_to_empty_seconds)
            .unwrap_or(0),
    )?;
    let voltage_now_uv = get_optional_scaled_integer(
        reader,
        ups_name,
        "battery.voltage",
        1_000_000.0,
        0,
        i32::MAX,
        last_snapshot
            .map(|snapshot| snapshot.voltage_now_uv)
            .unwrap_or(0),
    )?;
    let ups_status = get_required_var(reader, ups_name, "ups.status")?;
    let (status, ac_online) = map_status(&ups_status, capacity)?;
    let ups_charge_time = if status == 1 && ac_online == 1 && capacity < 100 {
        get_ups_charge_time(reader, config, capacity)?
    } else {
        None
    };
    let charge_estimate = charging.update(capacity, status, ac_online, ups_charge_time);

    Ok(Snapshot {
        capacity,
        voltage_now_uv,
        time_to_empty_seconds,
        status,
        ac_online,
        time_to_full_seconds: charge_estimate.seconds,
        charge_source: charge_estimate.source,
    })
}

fn get_ups_charge_time(
    reader: &mut BufReader<TcpStream>,
    config: &Config,
    capacity: i32,
) -> io::Result<Option<(f64, ChargeSource)>> {
    if let Some(variable) = &config.time_to_full_var {
        if let Some(seconds) = get_optional_number(reader, &config.ups_name, variable)? {
            if seconds > 0.0 {
                return Ok(Some((seconds, ChargeSource::UpsTime)));
            }
        }
    }

    let amp_hours = get_optional_number(reader, &config.ups_name, "battery.capacity")?;
    if let Some(amp_hours) = amp_hours.filter(|value| *value > 0.0) {
        let current = get_optional_number(reader, &config.ups_name, "battery.current")?;
        if let Some(current) = current.filter(|value| value.abs() > 0.0) {
            // ใช้ขนาด battery.current เฉพาะเมื่อ UPS ยืนยันสถานะ Charging เท่านั้น
            let seconds = 3600.0 * amp_hours * (100 - capacity) as f64
                / (100.0 * current.abs());
            if seconds.is_finite() && seconds > 0.0 {
                return Ok(Some((seconds, ChargeSource::UpsCurrent)));
            }
        }
    }

    Ok(None)
}

fn get_optional_number(
    reader: &mut BufReader<TcpStream>,
    ups_name: &str,
    variable: &str,
) -> io::Result<Option<f64>> {
    // ข้อมูลชาร์จที่ไม่มีหรือใช้ไม่ได้ต้องไม่ทำให้ค่าหลักของแบตเตอรี่หยุดอัปเดต
    Ok(get_var(reader, ups_name, variable)?
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite()))
}

fn get_var(
    reader: &mut BufReader<TcpStream>,
    ups_name: &str,
    variable: &str,
) -> io::Result<Option<String>> {
    let command = format!("GET VAR {ups_name} {variable}\n");
    reader.get_mut().write_all(command.as_bytes())?;
    reader.get_mut().flush()?;

    let mut response = String::new();
    let bytes_read = reader
        .take((MAX_RESPONSE_LENGTH + 1) as u64)
        .read_line(&mut response)?;
    if bytes_read == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "NUT closed the connection",
        ));
    }
    if response.len() > MAX_RESPONSE_LENGTH {
        return Err(invalid_data("NUT response is too long"));
    }
    if !response.ends_with('\n') {
        return Err(invalid_data("NUT response is not newline-terminated"));
    }

    let response = response.trim_end_matches(|character| character == '\r' || character == '\n');
    let tokens = parse_protocol_tokens(response)?;
    if tokens.first().map(String::as_str) == Some("ERR") {
        if tokens.len() == 2
            && matches!(
                tokens[1].as_str(),
                "VAR-NOT-SUPPORTED" | "VAR-NOT-AVAILABLE"
            )
        {
            return Ok(None);
        }
        return Err(invalid_data(format!("NUT returned: {response}")));
    }
    if tokens.len() != 4
        || tokens[0] != "VAR"
        || tokens[1] != ups_name
        || tokens[2] != variable
    {
        return Err(invalid_data(format!(
            "unexpected response for {variable}: {response}"
        )));
    }

    Ok(Some(tokens[3].clone()))
}

fn get_required_var(
    reader: &mut BufReader<TcpStream>,
    ups_name: &str,
    variable: &str,
) -> io::Result<String> {
    get_var(reader, ups_name, variable)?.ok_or_else(|| {
        invalid_data(format!(
            "required NUT variable is not available: {variable}"
        ))
    })
}

fn get_optional_scaled_integer(
    reader: &mut BufReader<TcpStream>,
    ups_name: &str,
    variable: &str,
    scale: f64,
    minimum: i32,
    maximum: i32,
    fallback: i32,
) -> io::Result<i32> {
    match get_var(reader, ups_name, variable)? {
        Some(value) => parse_scaled_integer(&value, scale, minimum, maximum, variable),
        None => Ok(fallback),
    }
}

fn parse_protocol_tokens(line: &str) -> io::Result<Vec<String>> {
    let bytes = line.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index == bytes.len() {
            break;
        }

        if bytes[index] == b'"' {
            index += 1;
            let mut token = Vec::new();
            let mut closed = false;
            while index < bytes.len() {
                match bytes[index] {
                    b'"' => {
                        index += 1;
                        closed = true;
                        break;
                    }
                    b'\\' => {
                        index += 1;
                        if index == bytes.len() {
                            return Err(invalid_data("unterminated escape in NUT response"));
                        }
                        if !matches!(bytes[index], b'\\' | b'"') {
                            return Err(invalid_data("invalid escape in NUT response"));
                        }
                        token.push(bytes[index]);
                        index += 1;
                    }
                    byte => {
                        if byte.is_ascii_control() {
                            return Err(invalid_data("control byte in quoted NUT value"));
                        }
                        token.push(byte);
                        index += 1;
                    }
                }
            }
            if !closed {
                return Err(invalid_data("unterminated quote in NUT response"));
            }
            if index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                return Err(invalid_data("invalid text after quoted NUT value"));
            }
            tokens.push(
                String::from_utf8(token)
                    .map_err(|_| invalid_data("NUT response is not valid UTF-8"))?,
            );
        } else {
            let start = index;
            while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                if bytes[index] == b'"' || bytes[index].is_ascii_control() {
                    return Err(invalid_data("invalid unquoted NUT token"));
                }
                index += 1;
            }
            tokens.push(line[start..index].to_owned());
        }
    }

    Ok(tokens)
}

fn parse_scaled_integer(
    value: &str,
    scale: f64,
    minimum: i32,
    maximum: i32,
    variable: &str,
) -> io::Result<i32> {
    let number = value
        .parse::<f64>()
        .map_err(|_| invalid_data(format!("{variable} is not numeric: {value}")))?;
    let scaled = number * scale;
    if !scaled.is_finite() || scaled < minimum as f64 || scaled > maximum as f64 {
        return Err(invalid_data(format!(
            "{variable} is outside the supported range: {value}"
        )));
    }

    Ok(scaled.round() as i32)
}

fn map_status(value: &str, capacity: i32) -> io::Result<(i32, i32)> {
    let statuses: Vec<&str> = value.split_ascii_whitespace().collect();
    if statuses.is_empty()
        || statuses.iter().any(|status| {
            !status
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'-')
        })
    {
        return Err(invalid_data(format!("invalid ups.status value: {value}")));
    }

    let on_line = statuses.contains(&"OL") || statuses.contains(&"ONLINE");
    let on_battery = statuses.contains(&"OB")
        || statuses.contains(&"ONBAT")
        || statuses.contains(&"ONBATT");
    let charging = statuses.contains(&"CHRG");
    let discharging = statuses.contains(&"DISCHRG");

    if on_line == on_battery {
        return Err(invalid_data(format!(
            "ups.status must contain exactly one of OL or OB: {value}"
        )));
    }
    if charging && (discharging || on_battery) {
        return Err(invalid_data(format!(
            "ups.status contains conflicting charge states: {value}"
        )));
    }

    let ac_online = if on_line { 1 } else { 0 };
    let status = if on_battery || discharging {
        2
    } else if charging {
        1
    } else if on_line && capacity == 100 {
        4
    } else {
        3
    };

    Ok((status, ac_online))
}

fn kernel_supports_charge_time(update_path: &Path) -> bool {
    let version_path = update_path.with_file_name("protocol_version");
    match fs::read_to_string(&version_path) {
        Ok(version) => version.trim() == "2",
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            eprintln!("cannot read {}: {error}", version_path.display());
            false
        }
    }
}

fn write_snapshot(config: &Config, snapshot: &Snapshot) -> io::Result<()> {
    let mut line = format!(
        "{} {} {} {} {}",
        snapshot.capacity,
        snapshot.voltage_now_uv,
        snapshot.time_to_empty_seconds,
        snapshot.status,
        snapshot.ac_online
    );
    if kernel_supports_charge_time(&config.sysfs_path) {
        line.push_str(&format!(" {}", snapshot.time_to_full_seconds));
    }
    line.push('\n');
    let mut file = OpenOptions::new().write(true).open(&config.sysfs_path)?;
    file.write_all(line.as_bytes())
}

fn next_argument(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("missing value for {option}"))
}

fn env_u64(name: &str, default: u64) -> Result<u64, String> {
    match env::var(name) {
        Ok(value) => parse_positive_u64(&value, name),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(env::VarError::NotUnicode(_)) => Err(format!("{name} is not valid UTF-8")),
    }
}

fn env_string(name: &str, default: &str) -> Result<String, String> {
    match env::var(name) {
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => Ok(default.to_owned()),
        Err(env::VarError::NotUnicode(_)) => Err(format!("{name} is not valid UTF-8")),
    }
}

fn parse_positive_u64(value: &str, name: &str) -> Result<u64, String> {
    match value.parse::<u64>() {
        Ok(number) if number > 0 => Ok(number),
        _ => Err(format!("{name} must be a positive whole number of seconds")),
    }
}

fn validate_protocol_token(value: &str, name: &str) -> Result<(), String> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(format!(
            "{name} may contain only ASCII letters, digits, '.', '_' and '-'"
        ));
    }
    Ok(())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
