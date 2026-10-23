use std::process::ExitCode;

use atk_device::{list_target_interfaces, read_target_info, DEFAULT_QUERY_TIMEOUT};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Help,
    List,
    ReadInfo,
    ReadWiredInfo,
}

fn parse_mode(args: &[String]) -> Result<Mode, &'static str> {
    match args {
        [] => Ok(Mode::Help),
        [flag] if flag == "--help" || flag == "-h" => Ok(Mode::Help),
        [flag] if flag == "--list" => Ok(Mode::List),
        [flag] if flag == "--read-info" => Ok(Mode::ReadInfo),
        [flag] if flag == "--read-wired-info" => Ok(Mode::ReadWiredInfo),
        _ => Err("unknown or conflicting arguments"),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = match parse_mode(&args) {
        Ok(mode) => mode,
        Err(message) => {
            eprintln!("atk-read-info: {message}");
            print_usage();
            return ExitCode::from(2);
        }
    };

    match mode {
        Mode::Help => {
            print_usage();
            ExitCode::SUCCESS
        }
        Mode::List => match list_target_interfaces() {
            Ok(interfaces) => match serde_json::to_string_pretty(&interfaces) {
                Ok(json) => {
                    println!("{json}");
                    ExitCode::SUCCESS
                }
                Err(_) => {
                    eprintln!("atk-read-info: could not serialize collection metadata");
                    ExitCode::from(1)
                }
            },
            Err(error) => {
                eprintln!("atk-read-info: {error}");
                ExitCode::from(1)
            }
        },
        Mode::ReadInfo | Mode::ReadWiredInfo => match if mode == Mode::ReadWiredInfo {
            read_wired_info()
        } else {
            read_target_info(DEFAULT_QUERY_TIMEOUT)
        } {
            Ok(info) => match serde_json::to_string_pretty(&info) {
                Ok(json) => {
                    println!("{json}");
                    ExitCode::SUCCESS
                }
                Err(_) => {
                    eprintln!("atk-read-info: could not serialize read-only information");
                    ExitCode::from(1)
                }
            },
            Err(error) => {
                eprintln!("atk-read-info: {error}");
                ExitCode::from(1)
            }
        },
    }
}

fn read_wired_info() -> Result<atk_device::DeviceInfo, atk_device::DeviceError> {
    let candidates = atk_device::list_target_devices()?;
    let wired: Vec<_> = candidates.iter().filter(|device|
        device.interface.vendor_id == atk_device::WIRED_VENDOR_ID
        && device.interface.product_id == atk_device::WIRED_PRODUCT_ID).collect();
    let selected = match wired.as_slice() {
        [device] => *device,
        [] => return Err(atk_device::DeviceError::NoMatchingCollection),
        many => return Err(atk_device::DeviceError::MultipleMatchingCollections { count: many.len() }),
    };
    let mut session = atk_device::open_report_session_for_key(&selected.device_key, DEFAULT_QUERY_TIMEOUT)?;
    atk_device::read_info_with_report_session(&mut session, |_| {})
}

fn print_usage() {
    println!(
        "atk-read-info\n\
         Usage:\n\
          atk-read-info [--help]\n\
          atk-read-info --list\n\
          atk-read-info --read-info\n\
          atk-read-info --read-wired-info\n\
         Default behavior prints this help and performs no HID I/O.\n\
         --list enumerates matching collection metadata only.\n\
         --read-info explicitly sends the four read-only GET requests."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_help_do_not_request_device_reads() {
        assert_eq!(parse_mode(&[]), Ok(Mode::Help));
        assert_eq!(parse_mode(&["--help".into()]), Ok(Mode::Help));
    }

    #[test]
    fn hardware_reads_require_the_explicit_flag() {
        assert_eq!(parse_mode(&["--list".into()]), Ok(Mode::List));
        assert_eq!(parse_mode(&["--read-info".into()]), Ok(Mode::ReadInfo));
        assert!(parse_mode(&["--list".into(), "--read-info".into()]).is_err());
    }
}
