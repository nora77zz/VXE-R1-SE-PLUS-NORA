use ghub_lua::{BackendError, InputBackend, ScriptEvent};
use lua_process::{LuaProcessHandle, LuaProcessRuntime, ProcessConfig, ProcessExit};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Default)]
struct AuditBackend {
    logs: Mutex<Vec<String>>,
    changed: Condvar,
}

impl AuditBackend {
    fn wait_for_log(&self, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut logs = self.lock(&self.logs);
        while !logs.iter().any(|line| line == expected) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for log {expected:?}");
            let (next, timeout) = self
                .changed
                .wait_timeout(logs, remaining)
                .unwrap_or_else(|poison| poison.into_inner());
            logs = next;
            assert!(!timeout.timed_out() || logs.iter().any(|line| line == expected));
        }
    }

    fn snapshot(&self) -> Vec<String> {
        self.lock(&self.logs).clone()
    }

    fn lock<'a, T>(&self, mutex: &'a Mutex<T>) -> MutexGuard<'a, T> {
        mutex.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

impl InputBackend for AuditBackend {
    fn press_mouse_button(&self, _button: u8) -> Result<(), BackendError> {
        Ok(())
    }

    fn release_mouse_button(&self, _button: u8) -> Result<(), BackendError> {
        Ok(())
    }

    fn is_mouse_button_pressed(&self, _button: u8) -> Result<bool, BackendError> {
        Ok(false)
    }

    fn move_mouse_relative(&self, _x: i32, _y: i32) -> Result<(), BackendError> {
        Ok(())
    }

    fn output_log_message(&self, message: &str) -> Result<(), BackendError> {
        self.lock(&self.logs).push(message.to_owned());
        self.changed.notify_all();
        Ok(())
    }

    fn clear_log(&self) -> Result<(), BackendError> {
        self.lock(&self.logs).clear();
        self.changed.notify_all();
        Ok(())
    }
}

fn runtime(backend: &Arc<AuditBackend>) -> LuaProcessRuntime {
    LuaProcessRuntime::with_config(
        Arc::clone(backend) as Arc<dyn InputBackend>,
        ProcessConfig {
            startup_timeout: Duration::from_secs(3),
            stop_grace: Duration::from_millis(250),
            stop_timeout: Duration::from_secs(2),
            backend_call_timeout: Duration::from_millis(400),
            ..ProcessConfig::default()
        },
    )
}

fn start(source: &str, backend: &Arc<AuditBackend>) -> LuaProcessHandle {
    runtime(backend)
        .start(PathBuf::from(env!("CARGO_BIN_EXE_lua-worker")), source)
        .expect("Lua worker starts")
}

fn wait_for_report(handle: &mut LuaProcessHandle) -> lua_process::ProcessReport {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(report) = handle.try_report().expect("report channel remains available") {
            return report;
        }
        assert!(Instant::now() < deadline, "worker did not publish its terminal report");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn event_error_is_reported_without_another_input_and_releases_the_handle() {
    let backend = Arc::new(AuditBackend::default());
    let mut failed = start(
        r#"
function OnEvent(event)
    if event == "FAIL" then
        OutputLogMessage("before-error")
        error("lifecycle audit failure")
    end
end
"#,
        &backend,
    );
    failed
        .dispatch(ScriptEvent::new("FAIL", None, None::<String>))
        .expect("single event is accepted");
    backend.wait_for_log("before-error");

    let report = wait_for_report(&mut failed);
    assert!(matches!(
        report.exit,
        ProcessExit::Failed(message) if message.contains("lifecycle audit failure")
    ));
    assert_eq!(backend.snapshot(), vec!["before-error"]);
    assert!(matches!(
        failed.try_report(),
        Err(lua_process::ProcessError::AlreadyJoined)
    ));

    let mut restarted = start("function OnEvent(event) end", &backend);
    let report = restarted.stop().expect("replacement worker stops cleanly");
    assert_eq!(report.exit, ProcessExit::Cancelled);
}

#[test]
fn try_report_returns_promptly_while_the_worker_is_running() {
    let backend = Arc::new(AuditBackend::default());
    let mut handle = start("function OnEvent(event) end", &backend);

    let started = Instant::now();
    assert!(handle.try_report().expect("report channel is open").is_none());
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "nonblocking report poll took {:?}",
        started.elapsed()
    );

    let report = handle.stop().expect("worker stops after the poll");
    assert_eq!(report.exit, ProcessExit::Cancelled);
}
