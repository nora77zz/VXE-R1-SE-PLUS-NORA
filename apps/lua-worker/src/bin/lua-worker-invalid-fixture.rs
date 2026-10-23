use lua_process::protocol::HostMessage;
use std::io::{self, BufRead, BufReader, Write};

fn main() {
    let stdin = io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let mut start = String::new();
    if input.read_line(&mut start).is_err() || start.is_empty() {
        std::process::exit(2);
    }
    let Ok(HostMessage::Start { .. }) = serde_json::from_str(&start) else {
        std::process::exit(2);
    };

    let stdout = io::stdout();
    let mut output = stdout.lock();
    output.write_all(b"{invalid-json}\n").unwrap();
    output.flush().unwrap();
}
