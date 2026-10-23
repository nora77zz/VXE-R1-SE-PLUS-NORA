use ghub_lua::InputBackend;
use lua_process::LuaProcessRuntime;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

struct DummyBackend;
impl InputBackend for DummyBackend {
    fn press_mouse_button(&self, _button: u8) -> Result<(), ghub_lua::BackendError> { Ok(()) }
    fn release_mouse_button(&self, _button: u8) -> Result<(), ghub_lua::BackendError> { Ok(()) }
    fn is_mouse_button_pressed(&self, _button: u8) -> Result<bool, ghub_lua::BackendError> { Ok(false) }
    fn move_mouse_relative(&self, _x: i32, _y: i32) -> Result<(), ghub_lua::BackendError> { Ok(()) }
    fn output_log_message(&self, msg: &str) -> Result<(), ghub_lua::BackendError> {
        println!("[LOG] {}", msg);
        Ok(())
    }
    fn clear_log(&self) -> Result<(), ghub_lua::BackendError> { Ok(()) }
}

#[test]
fn test_runtime_start_user_script() {
    let raw = PathBuf::from("../../apps/lua-worker/target/release/lua-worker.exe");
    let worker_path = fs::canonicalize(&raw).expect("canonicalize");

    let runtime = LuaProcessRuntime::new(Arc::new(DummyBackend));
    let source = r#"-- Script Lua Host-side para VXE R1 SE+
-- 100% Compatível com a API Logitech G-HUB (OnEvent, OutputLogMessage, MoveMouseRelative, Sleep)

function OnEvent(event, arg)
    OutputLogMessage("Evento recebido: %s, Arg: %s\n", tostring(event), tostring(arg))
    
    if event == "PROFILE_ACTIVATED" then
        OutputLogMessage(">> [VXE R1 SE+] Perfil ativado com sucesso!\n")
    end

    -- Exemplo: clique com o botão esquerdo (1)
    if event == "MOUSE_BUTTON_PRESSED" and arg == 1 then
        OutputLogMessage(">> Botão 1 (Esquerdo) pressionado\n")
    end
end
"#;

    println!("Starting worker with user script...");
    let start_res = runtime.start(&worker_path, source);
    match start_res {
        Ok(mut handle) => {
            println!("Worker started successfully!");
            let report = handle.stop().expect("stop should succeed");
            println!("Stop report: {:?}", report);
        }
        Err(err) => {
            panic!("runtime.start failed: {:?}", err);
        }
    }
}
