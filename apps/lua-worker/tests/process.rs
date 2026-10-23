use ghub_lua::{BackendError, InputBackend, ScriptEvent};
use lua_process::{
    LuaProcessRuntime, ProcessConfig, ProcessExit, ProcessReport,
};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Eq, PartialEq)]
enum Action {
    Press(u8),
    Release(u8),
    Move(i32, i32),
    ClearLog,
}

#[derive(Default)]
struct MockBackend {
    actions: Mutex<Vec<Action>>,
    pressed: Mutex<BTreeSet<u8>>,
    logs: Mutex<Vec<String>>,
    changed: Condvar,
}

impl MockBackend {
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

    fn wait_for_actions(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut actions = self.lock(&self.actions);
        while actions.len() < count {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for backend actions");
            let (next, timeout) = self
                .changed
                .wait_timeout(actions, remaining)
                .unwrap_or_else(|poison| poison.into_inner());
            actions = next;
            assert!(!timeout.timed_out() || actions.len() >= count);
        }
    }

    fn action_snapshot(&self) -> Vec<Action> {
        self.lock(&self.actions).clone()
    }

    fn pressed_snapshot(&self) -> BTreeSet<u8> {
        self.lock(&self.pressed).clone()
    }

    fn lock<'a, T>(&self, mutex: &'a Mutex<T>) -> MutexGuard<'a, T> {
        mutex.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

impl InputBackend for MockBackend {
    fn press_mouse_button(&self, button: u8) -> Result<(), BackendError> {
        self.lock(&self.pressed).insert(button);
        self.lock(&self.actions).push(Action::Press(button));
        self.changed.notify_all();
        Ok(())
    }

    fn release_mouse_button(&self, button: u8) -> Result<(), BackendError> {
        self.lock(&self.pressed).remove(&button);
        self.lock(&self.actions).push(Action::Release(button));
        self.changed.notify_all();
        Ok(())
    }

    fn is_mouse_button_pressed(&self, button: u8) -> Result<bool, BackendError> {
        Ok(self.lock(&self.pressed).contains(&button))
    }

    fn move_mouse_relative(&self, x: i32, y: i32) -> Result<(), BackendError> {
        self.lock(&self.actions).push(Action::Move(x, y));
        self.changed.notify_all();
        Ok(())
    }

    fn output_log_message(&self, message: &str) -> Result<(), BackendError> {
        self.lock(&self.logs).push(message.to_owned());
        self.changed.notify_all();
        Ok(())
    }

    fn clear_log(&self) -> Result<(), BackendError> {
        self.lock(&self.logs).clear();
        self.lock(&self.actions).push(Action::ClearLog);
        self.changed.notify_all();
        Ok(())
    }
}

fn config() -> ProcessConfig {
    ProcessConfig {
        startup_timeout: Duration::from_secs(3),
        stop_grace: Duration::from_millis(250),
        backend_call_timeout: Duration::from_millis(400),
        ..ProcessConfig::default()
    }
}

fn start(source: &str, backend: &Arc<MockBackend>) -> lua_process::LuaProcessHandle {
    let worker = PathBuf::from(env!("CARGO_BIN_EXE_lua-worker"));
    LuaProcessRuntime::with_config(Arc::clone(backend) as Arc<dyn InputBackend>, config())
        .start(worker, source)
        .expect("worker process should become ready")
}

fn stop(mut handle: lua_process::LuaProcessHandle) -> (ProcessReport, Duration) {
    let started = Instant::now();
    let report = handle.stop().expect("worker should report shutdown");
    (report, started.elapsed())
}

#[test]
fn parent_kills_non_yieldable_gsub_callback_and_releases_pressed_button() {
    let backend = Arc::new(MockBackend::default());
    let handle = start(
        r#"
function OnEvent(event)
    if event == "STUCK" then
        PressMouseButton(2)
        OutputLogMessage("inside-gsub")
        string.gsub("x", ".", function() while true do end end)
    end
end
"#,
        &backend,
    );
    handle
        .dispatch(ScriptEvent::new("STUCK", None, None::<String>))
        .expect("event queues");
    backend.wait_for_log("inside-gsub");
    backend.wait_for_actions(1);
    assert_eq!(backend.pressed_snapshot(), BTreeSet::from([2]));

    let (report, elapsed) = stop(handle);
    assert_eq!(report.exit, ProcessExit::KilledAfterGrace);
    assert!(report.forced_kill);
    assert!(elapsed < Duration::from_secs(2), "stop took {elapsed:?}");
    assert!(backend.pressed_snapshot().is_empty());
    assert_eq!(
        backend.action_snapshot(),
        vec![Action::Press(2), Action::Release(2)]
    );
}

#[test]
fn parent_stop_interrupts_protected_lua_loop() {
    let backend = Arc::new(MockBackend::default());
    let handle = start(
        r#"
function OnEvent(event)
    if event == "LOOP" then
        OutputLogMessage("inside-pcall")
        pcall(function() while true do end end)
    end
end
"#,
        &backend,
    );
    handle
        .dispatch(ScriptEvent::new("LOOP", None, None::<String>))
        .expect("event queues");
    backend.wait_for_log("inside-pcall");

    let (report, elapsed) = stop(handle);
    assert_eq!(report.exit, ProcessExit::Cancelled);
    assert!(elapsed < Duration::from_secs(2), "stop took {elapsed:?}");
}

#[test]
fn script_error_releases_parent_tracked_button() {
    let backend = Arc::new(MockBackend::default());
    let handle = start(
        r#"
function OnEvent(event)
    if event == "FAIL" then
        PressMouseButton(4)
        error("synthetic script error")
    end
end
"#,
        &backend,
    );
    handle
        .dispatch(ScriptEvent::new("FAIL", None, None::<String>))
        .expect("event queues");
    backend.wait_for_actions(2);
    assert_eq!(
        backend.action_snapshot(),
        vec![Action::Press(4), Action::Release(4)]
    );
    assert!(backend.pressed_snapshot().is_empty());
    let (report, _) = stop(handle);
    assert!(matches!(report.exit, ProcessExit::Failed(message) if message.contains("synthetic script error")));
}

#[test]
fn child_eof_releases_parent_tracked_button() {
    let backend = Arc::new(MockBackend::default());
    let worker = PathBuf::from(env!("CARGO_BIN_EXE_lua-worker-eof-fixture"));
    let runtime = LuaProcessRuntime::with_config(
        Arc::clone(&backend) as Arc<dyn InputBackend>,
        config(),
    );
    let mut handle = runtime.start(worker, "return").expect("fixture starts");
    backend.wait_for_actions(2);
    assert!(backend.pressed_snapshot().is_empty());
    let report = handle.stop().expect("bounded EOF report");
    assert_eq!(report.exit, ProcessExit::UnexpectedEof);
    assert_eq!(
        backend.action_snapshot(),
        vec![Action::Press(2), Action::Release(2)]
    );
}

#[test]
fn invalid_child_frame_cannot_reach_input_backend() {
    let backend = Arc::new(MockBackend::default());
    let worker = PathBuf::from(env!("CARGO_BIN_EXE_lua-worker-invalid-fixture"));
    let runtime = LuaProcessRuntime::with_config(
        Arc::clone(&backend) as Arc<dyn InputBackend>,
        config(),
    );
    let error = match runtime.start(worker, "return") {
        Ok(_) => panic!("invalid fixture must fail before startup"),
        Err(error) => error,
    };
    assert!(matches!(error, lua_process::ProcessError::StartupFailed(_)));
    assert!(backend.action_snapshot().is_empty());
    assert!(backend.pressed_snapshot().is_empty());
}
