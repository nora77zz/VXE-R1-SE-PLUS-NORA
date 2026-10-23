use std::process::ExitCode;

use atk_device::{list_target_interfaces, read_target_info, DEFAULT_QUERY_TIMEOUT};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Help,
    List,
    ReadInfo,
}

fn parse_mode(args: &[String]) -> Result<Mode, &'static str> {
    match args {
        [] => Ok(Mode::Help),
        [flag] if flag == "--help" || flag == "-h" => Ok(Mode::Help),
        [flag] if flag == "--list" => Ok(Mode::List),
        [flag] if flag == "--read-info" => Ok(Mode::ReadInfo),
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
        Mode::ReadInfo => match read_target_info(DEFAULT_QUERY_TIMEOUT) {
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

fn print_usage() {
    println!(
        "atk-read-info\n\
         Usage:\n\
          atk-read-info [--help]\n\
          atk-read-info --list\n\
          atk-read-info --read-info\n\
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
