use std::env;
use std::fs::OpenOptions;
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
const MAX_RESPONSE_LENGTH: usize = 4096;

struct Config {
    host: String,
    ups_name: String,
    poll_interval: Duration,
    timeout: Duration,
    sysfs_path: PathBuf,
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
                         NUT_POLL_INTERVAL_SECONDS, NUT_TIMEOUT_SECONDS, NUT_SYSFS_PATH"
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

        Ok(Self {
            host,
            ups_name,
            poll_interval: Duration::from_secs(poll_interval_seconds),
            timeout: Duration::from_secs(timeout_seconds),
            sysfs_path,
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

    loop {
        match connect(&config) {
            Ok(stream) => {
                eprintln!("connected to NUT at {}", config.host);
                poll_connection(
                    stream,
                    &config,
                    &mut last_published_snapshot,
                    &mut degraded_published,
                );
            }
            Err(error) => {
                eprintln!("cannot connect to NUT at {}: {error}", config.host);
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
) {
    let mut reader = BufReader::new(stream);

    loop {
        match fetch_snapshot(&mut reader, &config.ups_name) {
            Ok(snapshot) => {
                if let Err(error) = write_snapshot(&config.sysfs_path, &snapshot) {
                    eprintln!(
                        "cannot update {}: {error}",
                        config.sysfs_path.display()
                    );
                } else {
                    *last_published_snapshot = Some(snapshot);
                    *degraded_published = false;
                }
            }
            Err(error) => {
                eprintln!("NUT connection lost or returned invalid data: {error}");
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

    let Some(mut snapshot) = last_published_snapshot else {
        return;
    };
    snapshot.status = 0;

    match write_snapshot(&config.sysfs_path, &snapshot) {
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

fn fetch_snapshot(reader: &mut BufReader<TcpStream>, ups_name: &str) -> io::Result<Snapshot> {
    let capacity = parse_scaled_integer(
        &get_var(reader, ups_name, "battery.charge")?,
        1.0,
        0,
        100,
        "battery.charge",
    )?;
    let time_to_empty_seconds = parse_scaled_integer(
        &get_var(reader, ups_name, "battery.runtime")?,
        1.0,
        0,
        i32::MAX,
        "battery.runtime",
    )?;
    let voltage_now_uv = parse_scaled_integer(
        &get_var(reader, ups_name, "battery.voltage")?,
        1_000_000.0,
        0,
        i32::MAX,
        "battery.voltage",
    )?;
    let ups_status = get_var(reader, ups_name, "ups.status")?;
    let (status, ac_online) = map_status(&ups_status, capacity)?;

    Ok(Snapshot {
        capacity,
        voltage_now_uv,
        time_to_empty_seconds,
        status,
        ac_online,
    })
}

fn get_var(
    reader: &mut BufReader<TcpStream>,
    ups_name: &str,
    variable: &str,
) -> io::Result<String> {
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

    let response = response.trim_end_matches(|character| character == '\r' || character == '\n');
    let tokens = parse_protocol_tokens(response)?;
    if tokens.first().map(String::as_str) == Some("ERR") {
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

    Ok(tokens[3].clone())
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
                        token.push(bytes[index]);
                        index += 1;
                    }
                    byte => {
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
    let on_line = statuses.contains(&"OL");
    let on_battery = statuses.contains(&"OB");
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
    let status = if charging {
        1
    } else if discharging || on_battery {
        2
    } else if capacity == 100 {
        4
    } else {
        3
    };

    Ok((status, ac_online))
}

fn write_snapshot(path: &Path, snapshot: &Snapshot) -> io::Result<()> {
    let line = format!(
        "{} {} {} {} {}\n",
        snapshot.capacity,
        snapshot.voltage_now_uv,
        snapshot.time_to_empty_seconds,
        snapshot.status,
        snapshot.ac_online
    );
    let mut file = OpenOptions::new().write(true).open(path)?;
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
