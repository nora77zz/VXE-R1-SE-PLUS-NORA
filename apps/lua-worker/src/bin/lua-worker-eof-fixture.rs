use lua_process::protocol::{BackendOperation, HostMessage, WorkerMessage};
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
    emit(&mut output, &WorkerMessage::Ready);
    emit(
        &mut output,
        &WorkerMessage::InputCall {
            id: 1,
            operation: BackendOperation::PressMouseButton { button: 2 },
        },
    );
    let mut response = String::new();
    let _ = input.read_line(&mut response);
    // Deliberately terminate without WorkerFinished to exercise parent EOF cleanup.
}

fn emit(output: &mut impl Write, message: &WorkerMessage) {
    serde_json::to_writer(&mut *output, message).unwrap();
    output.write_all(b"\n").unwrap();
    output.flush().unwrap();
}
