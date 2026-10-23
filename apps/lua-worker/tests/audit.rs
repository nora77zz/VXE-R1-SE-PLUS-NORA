use ghub_lua::{BackendError, InputBackend, RuntimeConfig, ScriptEvent};
use lua_process::{
    LuaProcessRuntime, ProcessConfig, ProcessError, ProcessExit, ProcessReport,
};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

const WORKER: &str = env!("CARGO_BIN_EXE_lua-worker");
const WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Eq, PartialEq)]
enum Action {
    Query(u8, bool),
    Press(u8),
    Release(u8),
    Move(i32, i32),
    Log(String),
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
        self.wait_for_log_matching(|line| line == expected, expected);
    }

    fn wait_for_log_prefix(&self, prefix: &str) {
        self.wait_for_log_matching(|line| line.starts_with(prefix), prefix);
    }

    fn wait_for_log_matching(&self, predicate: impl Fn(&str) -> bool, label: &str) {
        let deadline = Instant::now() + WAIT;
        let mut logs = self.lock(&self.logs);
        while !logs.iter().any(|line| predicate(line)) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for Lua log {label:?}");
            let (next, timeout) = self
                .changed
                .wait_timeout(logs, remaining)
                .unwrap_or_else(|poison| poison.into_inner());
            logs = next;
            assert!(
                !timeout.timed_out() || logs.iter().any(|line| predicate(line)),
                "timed out waiting for Lua log {label:?}"
            );
        }
    }

    fn wait_for_actions(&self, count: usize) {
        let deadline = Instant::now() + WAIT;
        let mut actions = self.lock(&self.actions);
        while actions.len() < count {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for backend actions");
            let (next, timeout) = self
                .changed
                .wait_timeout(actions, remaining)
                .unwrap_or_else(|poison| poison.into_inner());
            actions = next;
            assert!(
                !timeout.timed_out() || actions.len() >= count,
                "timed out waiting for backend actions"
            );
        }
    }

    fn action_snapshot(&self) -> Vec<Action> {
        self.lock(&self.actions).clone()
    }

    fn log_snapshot(&self) -> Vec<String> {
        self.lock(&self.logs).clone()
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
        let pressed = self.lock(&self.pressed).contains(&button);
        self.lock(&self.actions).push(Action::Query(button, pressed));
        self.changed.notify_all();
        Ok(pressed)
    }

    fn move_mouse_relative(&self, x: i32, y: i32) -> Result<(), BackendError> {
        self.lock(&self.actions).push(Action::Move(x, y));
        self.changed.notify_all();
        Ok(())
    }

    fn output_log_message(&self, message: &str) -> Result<(), BackendError> {
        self.lock(&self.logs).push(message.to_owned());
        self.lock(&self.actions).push(Action::Log(message.to_owned()));
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
        runtime: RuntimeConfig::default(),
        startup_timeout: Duration::from_secs(3),
        stop_grace: Duration::from_millis(250),
        stop_timeout: Duration::from_secs(3),
        backend_call_timeout: Duration::from_millis(500),
        event_queue_capacity: 128,
    }
}

fn start(source: &str, backend: &Arc<MockBackend>) -> lua_process::LuaProcessHandle {
    start_with_config(source, backend, config())
}

fn start_with_config(
    source: &str,
    backend: &Arc<MockBackend>,
    config: ProcessConfig,
) -> lua_process::LuaProcessHandle {
    let started = Instant::now();
    let runtime = LuaProcessRuntime::with_config(
        Arc::clone(backend) as Arc<dyn InputBackend>,
        config,
    );
    let result = runtime.start(PathBuf::from(WORKER), source);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "worker startup exceeded the external case deadline; exact Lua source: {source:?}"
    );
    result.unwrap_or_else(|error| {
        panic!("worker startup failed: {error}; exact Lua source: {source:?}")
    })
}

fn stop(mut handle: lua_process::LuaProcessHandle) -> ProcessReport {
    let deadline = Instant::now() + Duration::from_secs(5);
    let report = handle.stop().expect("worker must stop within its configured deadline");
    assert!(Instant::now() <= deadline, "process stop exceeded external deadline");
    report
}

fn wait_until_process_terminal(
    handle: &lua_process::LuaProcessHandle,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut probe = 0u64;
    loop {
        match handle.dispatch(ScriptEvent::new(
            format!("AUDIT_TERMINAL_PROBE_{probe}"),
            None,
            None::<String>,
        )) {
            Ok(()) | Err(ProcessError::EventQueueFull) => probe += 1,
            Err(ProcessError::WorkerStopped) => return Ok(()),
            Err(error) => return Err(format!("unexpected terminal probe error: {error}")),
        }
        if Instant::now() >= deadline {
            return Err(format!("worker did not terminate before {timeout:?}"));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn extract_default_script(bundle: &str) -> String {
    let marker = "const DEFAULT_SCRIPT = `";
    let template = bundle
        .split_once(marker)
        .unwrap_or_else(|| panic!("bundle does not contain {marker:?}"))
        .1;
    let end = template
        .find("\n`;\n")
        .unwrap_or_else(|| panic!("DEFAULT_SCRIPT template terminator was not found"));
    // The JavaScript template literal doubles backslashes. Restore the Lua
    // source's original `\\n` escape before sending the source to lua-worker.
    template[..end].replace("\\\\n", "\\n")
}

#[test]
fn ipc_exercises_all_five_mouse_buttons_and_preserves_backend_call_order() {
    let backend = Arc::new(MockBackend::default());
    let source = r#"
function OnEvent(event)
    if event == "BUTTON_AUDIT" then
        for button = 1, 5 do
            local before = IsMouseButtonPressed(button)
            PressMouseButton(button)
            local pressed = IsMouseButtonPressed(button)
            ReleaseMouseButton(button)
            local released = IsMouseButtonPressed(button)
            PressAndReleaseMouseButton(button)
            OutputLogMessage("button:%d:%s:%s:%s", button,
                tostring(before), tostring(pressed), tostring(released))
        end
    end
end
"#;
    let handle = start(&source, &backend);
    handle
        .dispatch(ScriptEvent::new("BUTTON_AUDIT", None, None::<String>))
        .expect("audit event must enter the process IPC queue");
    backend.wait_for_log("button:5:false:true:false");
    let report = stop(handle);

    assert_eq!(report.exit, ProcessExit::Cancelled);
    assert_eq!(backend.pressed_snapshot(), BTreeSet::new());
    assert_eq!(
        backend.log_snapshot(),
        (1..=5)
            .map(|button| format!("button:{button}:false:true:false"))
            .collect::<Vec<_>>()
    );
    let mut expected = Vec::new();
    for button in 1..=5 {
        expected.extend([
            Action::Query(button, false),
            Action::Press(button),
            Action::Query(button, true),
            Action::Release(button),
            Action::Query(button, false),
            Action::Press(button),
            Action::Release(button),
            Action::Log(format!("button:{button}:false:true:false")),
        ]);
    }
    assert_eq!(backend.action_snapshot(), expected);
}

#[test]
fn ipc_preserves_signed_motion_and_sleep_running_time_order_with_tolerance() {
    let backend = Arc::new(MockBackend::default());
    let source = r#"
function OnEvent(event)
    if event == "MOTION_AUDIT" then
        MoveMouseRelative(11, -5)
        MoveMouseRelative(-3, 7)
        local started = GetRunningTime()
        Sleep(45)
        local finished = GetRunningTime()
        OutputLogMessage("timing:%d:%d:%d", started, finished, finished - started)
    end
end
"#;
    let handle = start(source, &backend);
    handle
        .dispatch(ScriptEvent::new("MOTION_AUDIT", None, None::<String>))
        .expect("motion event must enter IPC");
    backend.wait_for_log_prefix("timing:");
    let report = stop(handle);

    assert_eq!(report.exit, ProcessExit::Cancelled);
    let logs = backend.log_snapshot();
    let values = logs[0]
        .strip_prefix("timing:")
        .expect("timing log prefix")
        .split(':')
        .map(|value| value.parse::<i64>().expect("integer timing value"))
        .collect::<Vec<_>>();
    assert_eq!(values.len(), 3);
    assert!(values[0] >= 0);
    assert!(values[1] >= values[0]);
    assert!((35..=2_000).contains(&values[2]), "unexpected Sleep elapsed time: {values:?}");
    assert_eq!(
        backend.action_snapshot(),
        vec![Action::Move(11, -5), Action::Move(-3, 7), Action::Log(logs[0].clone())]
    );
}

#[test]
fn primary_button_gate_boolean_numeric_nil_args_and_family_cross_ipc() {
    let backend = Arc::new(MockBackend::default());
    let source = r#"
function OnEvent(event, arg, family)
    if event == "ENABLE_BOOLEAN" then
        EnablePrimaryMouseButtonEvents(true)
    elseif event == "DISABLE_NUMERIC" then
        EnablePrimaryMouseButtonEvents(0)
    elseif event == "ENABLE_NUMERIC" then
        EnablePrimaryMouseButtonEvents(1)
    elseif event == "MOUSE_BUTTON_PRESSED" or event == "MOUSE_BUTTON_RELEASED" then
        OutputLogMessage("primary:%s:%s:%s", event, tostring(arg), tostring(family))
    elseif event == "NIL_FIELDS" then
        OutputLogMessage("nil:%s:%s", tostring(arg), tostring(family))
    elseif event == "EXPLICIT_FIELDS" then
        OutputLogMessage("fields:%s:%s", tostring(arg), tostring(family))
    end
end
"#;
    let handle = start(source, &backend);
    for event in [
        ScriptEvent::new("MOUSE_BUTTON_PRESSED", Some(1), Some("mouse")),
        ScriptEvent::new("ENABLE_BOOLEAN", None, None::<String>),
        ScriptEvent::new("MOUSE_BUTTON_PRESSED", Some(1), Some("mouse")),
        ScriptEvent::new("DISABLE_NUMERIC", None, None::<String>),
        ScriptEvent::new("MOUSE_BUTTON_RELEASED", Some(1), Some("mouse")),
        ScriptEvent::new("ENABLE_NUMERIC", None, None::<String>),
        ScriptEvent::new("MOUSE_BUTTON_RELEASED", Some(1), None::<String>),
        ScriptEvent::new("NIL_FIELDS", None, None::<String>),
        ScriptEvent::new("EXPLICIT_FIELDS", Some(17), Some("mouse")),
    ] {
        handle.dispatch(event).expect("event must enter IPC");
    }
    backend.wait_for_log("fields:17:mouse");
    let report = stop(handle);

    assert_eq!(report.exit, ProcessExit::Cancelled);
    assert_eq!(
        backend.log_snapshot(),
        vec![
            "primary:MOUSE_BUTTON_PRESSED:1:mouse",
            "primary:MOUSE_BUTTON_RELEASED:1:nil",
            "nil:nil:nil",
            "fields:17:mouse",
        ]
    );
    assert!(backend.action_snapshot().iter().all(|action| matches!(action, Action::Log(_))));
}

#[test]
fn legacy_default_script_reproduces_the_primary_button_gate_bug() {
    let backend = Arc::new(MockBackend::default());
    let source = include_str!("fixtures/legacy-default.lua");
    assert!(!source.contains("EnablePrimaryMouseButtonEvents(true)"));
    let handle = start(&source, &backend);
    handle
        .dispatch(ScriptEvent::new(
            "MOUSE_BUTTON_PRESSED",
            Some(1),
            Some("mouse"),
        ))
        .expect("primary event queues");
    handle
        .dispatch(ScriptEvent::new("AUDIT_SENTINEL", None, None::<String>))
        .expect("sentinel queues after primary event");
    backend.wait_for_log("Evento recebido: AUDIT_SENTINEL, Arg: nil\n");
    let report = stop(handle);

    assert_eq!(report.exit, ProcessExit::Cancelled);
    assert_eq!(
        backend.log_snapshot(),
        vec![
            "Evento recebido: AUDIT_SENTINEL, Arg: nil\n",
            "Evento recebido: PROFILE_DEACTIVATED, Arg: nil\n",
        ]
    );
    assert!(!backend
        .log_snapshot()
        .iter()
        .any(|line| line == ">> Botão 1 (Esquerdo) pressionado\n"));
    assert!(backend.action_snapshot().iter().all(|action| matches!(action, Action::Log(_))));
}

#[test]
fn active_bundle_default_script_delivers_the_left_button_branch() {
    let backend = Arc::new(MockBackend::default());
    let source = extract_default_script(include_str!(
        "../../../apps/desktop/web/atk_tauri_bridge.js"
    ));
    assert!(source.contains("EnablePrimaryMouseButtonEvents(true)"));
    let handle = start(&source, &backend);
    handle
        .dispatch(ScriptEvent::new(
            "MOUSE_BUTTON_PRESSED",
            Some(1),
            Some("mouse"),
        ))
        .expect("left button queues");
    handle
        .dispatch(ScriptEvent::new("AUDIT_SENTINEL", None, None::<String>))
        .expect("sentinel queues after left button");
    backend.wait_for_log("Evento recebido: AUDIT_SENTINEL, Arg: nil\n");
    let report = stop(handle);

    assert_eq!(report.exit, ProcessExit::Cancelled);
    assert_eq!(
        backend.log_snapshot(),
        vec![
            "Evento recebido: MOUSE_BUTTON_PRESSED, Arg: 1\n",
            ">> Botão 1 (Esquerdo) pressionado\n",
            "Evento recebido: AUDIT_SENTINEL, Arg: nil\n",
        ]
    );
}

#[test]
fn output_log_formatting_and_clear_log_are_ordered_through_ipc() {
    let backend = Arc::new(MockBackend::default());
    let source = r#"
function OnEvent(event)
    if event == "LOG_AUDIT" then
        OutputLogMessage("obsolete:%d", 9)
        ClearLog()
        OutputLogMessage("formatted:%04d:%s", 7, "ok")
    end
end
"#;
    let handle = start(source, &backend);
    handle
        .dispatch(ScriptEvent::new("LOG_AUDIT", None, None::<String>))
        .expect("log event queues");
    backend.wait_for_log("formatted:0007:ok");
    let report = stop(handle);

    assert_eq!(report.exit, ProcessExit::Cancelled);
    assert_eq!(backend.log_snapshot(), vec!["formatted:0007:ok"]);
    assert_eq!(
        backend.action_snapshot(),
        vec![
            Action::Log("obsolete:9".to_owned()),
            Action::ClearLog,
            Action::Log("formatted:0007:ok".to_owned()),
        ]
    );
}

#[test]
fn syntax_error_is_reported_during_process_startup() {
    let backend = Arc::new(MockBackend::default());
    let runtime = LuaProcessRuntime::with_config(
        Arc::clone(&backend) as Arc<dyn InputBackend>,
        config(),
    );
    let source = "function OnEvent(\n    this is not valid Lua";
    let started = Instant::now();
    let result = runtime.start(PathBuf::from(WORKER), source);
    assert!(started.elapsed() < Duration::from_secs(5));
    match result {
        Err(ProcessError::StartupFailed(message)) => {
            assert!(!message.is_empty(), "compile failure should include a diagnostic");
        }
        Ok(_) => panic!("invalid Lua source unexpectedly became ready: {source:?}"),
        Err(error) => panic!("expected Lua syntax failure, got {error}; source: {source:?}"),
    }
    assert!(backend.action_snapshot().is_empty());
    assert!(backend.log_snapshot().is_empty());
}

#[test]
fn missing_worker_path_fails_startup_without_falling_back_to_path_lookup() {
    let backend = Arc::new(MockBackend::default());
    let runtime = LuaProcessRuntime::with_config(
        Arc::clone(&backend) as Arc<dyn InputBackend>,
        config(),
    );
    let missing = std::env::temp_dir().join(format!(
        "lua-worker-audit-missing-{}-{}.exe",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    assert!(missing.is_absolute());
    assert!(!missing.exists());
    let result = runtime.start(&missing, "return");
    assert!(matches!(result, Err(ProcessError::InvalidExecutablePath(_))));
    assert!(backend.action_snapshot().is_empty());
}

#[test]
fn rejected_event_names_and_families_never_reach_the_input_backend() {
    let backend = Arc::new(MockBackend::default());
    let source = r#"
function OnEvent(event)
    MoveMouseRelative(2, -1)
end
"#;
    let handle = start(source, &backend);
    let empty_name = handle.dispatch(ScriptEvent::new("", None, None::<String>));
    let long_name = handle.dispatch(ScriptEvent::new("x".repeat(257), None, None::<String>));
    let long_family = handle.dispatch(ScriptEvent::new("VALID", None, Some("f".repeat(129))));
    assert!(matches!(empty_name, Err(ProcessError::InvalidConfiguration(_))));
    assert!(matches!(long_name, Err(ProcessError::InvalidConfiguration(_))));
    assert!(matches!(long_family, Err(ProcessError::InvalidConfiguration(_))));

    handle
        .dispatch(ScriptEvent::new("VALID", None, None::<String>))
        .expect("valid control event queues");
    backend.wait_for_actions(1);
    let report = stop(handle);
    assert_eq!(report.exit, ProcessExit::Cancelled);
    assert_eq!(backend.action_snapshot(), vec![Action::Move(2, -1)]);
}

#[test]
fn unsupported_apis_are_absent_and_callback_error_is_reported_without_input() {
    let backend = Arc::new(MockBackend::default());
    let source = r#"
function OnEvent(event)
    if event == "UNSUPPORTED_AUDIT" then
        assert(PressKey == nil)
        assert(ReleaseKey == nil)
        assert(MoveMouseWheel == nil)
        assert(SetMouseDPITable == nil)
        assert(SetMouseDPITableIndex == nil)
        assert(io == nil and os == nil and package == nil and require == nil)
        OutputLogMessage("unsupported-apis-absent")
        SetMouseDPITable(1, 800)
    end
end
"#;
    let handle = start(source, &backend);
    handle
        .dispatch(ScriptEvent::new(
            "UNSUPPORTED_AUDIT",
            None,
            None::<String>,
        ))
        .expect("unsupported API event queues");
    backend.wait_for_log("unsupported-apis-absent");
    wait_until_process_terminal(&handle, WAIT)
        .expect("unsupported call must terminate the real worker process");
    let report = stop(handle);

    assert!(matches!(report.exit, ProcessExit::Failed(ref message) if
        message.contains("SetMouseDPITable") || message.contains("nil value")));
    assert_eq!(backend.action_snapshot(), vec![Action::Log("unsupported-apis-absent".to_owned())]);
}

#[test]
fn invalid_api_arguments_are_rejected_without_input_and_callback_remains_responsive() {
    let backend = Arc::new(MockBackend::default());
    let source = r#"
function OnEvent(event)
    if event == "INVALID_ARGUMENT_AUDIT" then
        local cases = {
            {"press-zero", function() PressMouseButton(0) end},
            {"press-six", function() PressMouseButton(6) end},
            {"release-zero", function() ReleaseMouseButton(0) end},
            {"click-six", function() PressAndReleaseMouseButton(6) end},
            {"query-six", function() IsMouseButtonPressed(6) end},
            {"move-fraction", function() MoveMouseRelative(1.5, 0) end},
            {"move-too-large", function() MoveMouseRelative(2147483648, 0) end},
            {"move-too-small", function() MoveMouseRelative(0, -2147483649) end},
            {"sleep-negative", function() Sleep(-1) end},
            {"gate-invalid", function() EnablePrimaryMouseButtonEvents(2) end},
        }
        local rejected = 0
        for _, case in ipairs(cases) do
            local ok = pcall(case[2])
            if not ok then rejected = rejected + 1 end
            OutputLogMessage("invalid:%s:%s", case[1], tostring(not ok))
        end
        OutputLogMessage("invalid-complete:%d", rejected)
    elseif event == "INVALID_ARGUMENT_SENTINEL" then
        OutputLogMessage("invalid-sentinel")
    end
end
"#;
    let handle = start(source, &backend);
    handle
        .dispatch(ScriptEvent::new(
            "INVALID_ARGUMENT_AUDIT",
            None,
            None::<String>,
        ))
        .expect("invalid API arguments are handled inside protected calls");
    backend.wait_for_log("invalid-complete:10");
    handle
        .dispatch(ScriptEvent::new(
            "INVALID_ARGUMENT_SENTINEL",
            None,
            None::<String>,
        ))
        .expect("callback remains usable after rejected arguments");
    backend.wait_for_log("invalid-sentinel");
    let report = stop(handle);

    assert_eq!(report.exit, ProcessExit::Cancelled);
    let logs = backend.log_snapshot();
    assert_eq!(logs.len(), 12);
    assert!(logs[..10].iter().all(|line| line.ends_with(":true")), "an invalid API argument was accepted: {logs:?}");
    assert_eq!(logs[10], "invalid-complete:10");
    assert_eq!(logs[11], "invalid-sentinel");
    assert!(backend.pressed_snapshot().is_empty());
    assert!(backend.action_snapshot().iter().all(|action| matches!(action, Action::Log(_))));
}

#[test]
fn memory_limit_rejects_oversized_lua_allocation_inside_callback() {
    let backend = Arc::new(MockBackend::default());
    let mut limited = config();
    limited.runtime.memory_limit_bytes = 16 * 1024 * 1024;
    let source = r#"
function OnEvent(event)
    if event == "MEMORY_AUDIT" then
        local ok, err = pcall(function()
            local oversized = string.rep("x", 32 * 1024 * 1024)
        end)
        OutputLogMessage("memory:%s:%s", tostring(ok), tostring(err))
    end
end
"#;
    let handle = start_with_config(source, &backend, limited);
    handle
        .dispatch(ScriptEvent::new("MEMORY_AUDIT", None, None::<String>))
        .expect("memory event queues");
    backend.wait_for_log_prefix("memory:");
    let report = stop(handle);

    assert_eq!(report.exit, ProcessExit::Cancelled);
    let logs = backend.log_snapshot();
    assert!(logs[0].starts_with("memory:false:"), "allocator limit was not enforced: {logs:?}");
    assert!(logs[0].to_ascii_lowercase().contains("memory"), "Lua error should identify allocation failure: {logs:?}");
    assert!(backend.action_snapshot().iter().all(|action| matches!(action, Action::Log(_))));
}

#[test]
fn bounded_runtime_event_queue_reports_full_and_stops_cooperatively() {
    let backend = Arc::new(MockBackend::default());
    let mut bounded = config();
    bounded.runtime.event_queue_capacity = 1;
    let source = r#"
function OnEvent(event)
    if event == "BLOCK_AUDIT" then
        OutputLogMessage("block-enter")
        while true do end
    end
end
"#;
    let handle = start_with_config(source, &backend, bounded);
    handle
        .dispatch(ScriptEvent::new("BLOCK_AUDIT", None, None::<String>))
        .expect("blocking callback event queues");
    backend.wait_for_log("block-enter");
    for index in 0..4 {
        handle
            .dispatch(ScriptEvent::new(
                format!("QUEUED_{index}"),
                None,
                None::<String>,
            ))
            .expect("events fit the larger host-side queue");
    }
    // The worker reader and bounded Lua command queue are separate processes
    // and channels; allow both to consume the queued frames before stop.
    thread::sleep(Duration::from_millis(100));
    let report = stop(handle);

    assert_eq!(report.exit, ProcessExit::Cancelled);
    assert!(!report.forced_kill);
    assert!(
        report.messages.iter().any(|message| message.contains("event queue is full")),
        "expected bounded queue rejection in process report: {:?}",
        report.messages
    );
    assert_eq!(backend.log_snapshot(), vec!["block-enter"]);
}

#[test]
fn event_order_and_repeated_process_lifecycle_remain_stable_under_small_stress() {
    let backend = Arc::new(MockBackend::default());
    let source = r#"
function OnEvent(event, arg)
    if event == "ORDER_AUDIT" then
        OutputLogMessage("order:%d", arg)
    end
end
"#;
    let handle = start(source, &backend);
    for value in 1..=64 {
        handle
            .dispatch(ScriptEvent::new("ORDER_AUDIT", Some(value), None::<String>))
            .expect("stress event queues within configured limits");
    }
    backend.wait_for_log("order:64");
    let report = stop(handle);
    assert_eq!(report.exit, ProcessExit::Cancelled);
    assert_eq!(
        backend.log_snapshot(),
        (1..=64).map(|value| format!("order:{value}")).collect::<Vec<_>>()
    );

    for cycle in 0..8 {
        let source = format!(
            "function OnEvent(event) if event == \"CYCLE\" then OutputLogMessage(\"cycle:{cycle}\") end end"
        );
        let handle = start(&source, &backend);
        let before = backend.log_snapshot().len();
        handle
            .dispatch(ScriptEvent::new("CYCLE", None, None::<String>))
            .expect("cycle event queues");
        backend.wait_for_log(&format!("cycle:{cycle}"));
        assert_eq!(backend.log_snapshot().len(), before + 1);
        assert_eq!(stop(handle).exit, ProcessExit::Cancelled);
    }
}
