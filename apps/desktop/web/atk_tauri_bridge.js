// ATK HUB Tauri Bridge & Lua Script Native Integration
(function() {
  console.log('[ATK HUB] Tauri Bridge initialized.');

  // Global error logger
  window.addEventListener('error', function(e) {
    console.error('[ATK HUB Window Error]', e.message, e.filename, e.lineno, e.error);
  });
  window.addEventListener('unhandledrejection', function(e) {
    console.error('[ATK HUB Unhandled Rejection]', e.reason);
  });

  // Dedicated Electron IPC shim for Tauri window management and desktop features
  function invokeNative(command, payload) {
    const core = window.__TAURI__ && window.__TAURI__.core;
    if (!core || typeof core.invoke !== 'function') {
      throw new Error('Bridge Tauri indisponível; não foi possível chamar ' + command + '.');
    }
    return payload === undefined ? core.invoke(command) : core.invoke(command, payload);
  }

  const alertedNativeActionErrors = new WeakSet();

  function showNativeActionError(error) {
    if (error && (typeof error === 'object' || typeof error === 'function')) {
      if (alertedNativeActionErrors.has(error)) return;
      alertedNativeActionErrors.add(error);
    }
    const message = error && error.message !== undefined ? String(error.message) : String(error);
    if (typeof window.alert === 'function') {
      try { window.alert(message); }
      catch (alertError) { console.error('[ATK HUB] Não foi possível exibir o erro da ação nativa.', alertError); }
    }
  }

  function nativeAction(action) {
    return (async function() {
      try {
        return await action();
      } catch (error) {
        showNativeActionError(error);
        throw error;
      }
    })();
  }

  function isPlainObject(value) {
    return value !== null && typeof value === 'object' && !Array.isArray(value);
  }

  function readLegacySettings() {
    try {
      const raw = localStorage.getItem('atk_system_settings');
      if (!raw) return null;
      const value = JSON.parse(raw);
      return isPlainObject(value) ? value : null;
    } catch (error) {
      console.warn('[ATK HUB] Preferências locais antigas inválidas; serão ignoradas.', error);
      return null;
    }
  }

  const ipcShim = {
    invoke: async function(cmd, ...args) {
      console.log('[IPC Invoke]', cmd, args);
      if (cmd === 'close-win') {
        return invokeNative('window_close');
      }
      if (cmd === 'min-win' || cmd === 'hide-win') {
        return invokeNative('window_minimize');
      }
      if (cmd === 'max-win') {
        return invokeNative('window_toggle_maximize');
      }
      if (cmd === 'app_quit') return invokeNative('app_quit');
      if (cmd === 'appVersion') return '3.2.17';
      if (cmd === 'getPointerSpeed') {
        return invokeNative('system_get_pointer_speed');
      }
      if (cmd === 'setPointerSpeed') {
        return nativeAction(() => {
          const value = args[0];
          if (!Number.isInteger(value) || value < 1 || value > 20) {
            throw new RangeError('A velocidade do ponteiro deve ser um número inteiro de 1 a 20.');
          }
          return invokeNative('system_set_pointer_speed', { value });
        });
      }
      if (cmd === 'getRollerSpeed') {
        return invokeNative('system_get_roller_speed');
      }
      if (cmd === 'setRollerSpeed') {
        return nativeAction(() => {
          const value = args[0];
          if (!Number.isInteger(value) || (value !== -1 && (value < 1 || value > 100))) {
            throw new RangeError('A rolagem deve ser -1 (página inteira) ou um número inteiro de linhas entre 1 e 100.');
          }
          return invokeNative('system_set_roller_speed', { value });
        });
      }
      if (cmd === 'getSetting') {
        const legacy = readLegacySettings();
        if (legacy && !legacySettingsMigrationComplete) {
          if (!legacySettingsMigrationPromise) {
            legacySettingsMigrationPromise = invokeNative('system_migrate_legacy_settings', { settings: legacy })
              .then(() => { legacySettingsMigrationComplete = true; })
              .finally(() => { legacySettingsMigrationPromise = null; });
          }
          await legacySettingsMigrationPromise;
        }
        return invokeNative('system_get_settings');
      }
      if (cmd === 'setSetting') {
        return nativeAction(() => {
          const patch = args[0];
          if (!isPlainObject(patch) || Object.keys(patch).length === 0) {
            throw new TypeError('As configurações devem ser enviadas como um objeto parcial não vazio.');
          }
          return invokeNative('system_set_settings', { patch });
        });
      }
      if (cmd === 'isDriverInSystem') return true;
      if (cmd === 'saveConfiguration') {
        return nativeAction(async () => {
          const configuration = args[0];
          if (!isPlainObject(configuration)) {
            throw new TypeError('A configuração para exportar precisa ser um objeto.');
          }
          const result = await invokeNative('configuration_export', { configuration });
          if (!result || typeof result.cancelled !== 'boolean' || (!result.cancelled && typeof result.path !== 'string')) {
            throw new Error('O backend retornou um resultado de exportação inválido.');
          }
          return result;
        });
      }
      if (cmd === 'updateTheme') return true;
      return null;
    },
    on: function() {},
    addListener: function() {},
    send: function() {},
    removeAllListeners: function() {}
  };
  window.__electronIpc = ipcShim;

  // Shim Node.js / Electron require for ATK HUB
  window.require = function(moduleName) {
    if (moduleName === 'electron') {
      return {
        ipcRenderer: ipcShim,
        shell: {
          openExternal: function(url) {
            console.log('[Shell Open]', url);
          }
        }
      };
    }
    if (moduleName === 'loudness') {
      return {
        getVolume: async () => 50,
        setVolume: async () => {},
        getMuted: async () => false,
        setMuted: async () => {}
      };
    }
    return {};
  };

  // Enable window dragging on top header
  window.addEventListener('DOMContentLoaded', () => {
    document.addEventListener('mousedown', (e) => {
      const header = e.target.closest('#layout-header, .drag');
      const interactive = e.target.closest('button, a, input, select, textarea, .no-drag, .icon, .icon-group, .hub-tab-bar__item');
      if (header && !interactive) {
        window.__TAURI__?.window?.getCurrentWindow?.()?.startDragging?.();
      }
    });
  });

  // State management for Lua Script Engine
  let luaStatus = { running: false, state: 'stopped', message: null };
  let logPollInterval = null;
  let activeLuaUi = null;
  let legacySettingsMigrationComplete = false;
  let legacySettingsMigrationPromise = null;
  const STORAGE_KEY = 'atk_vxe_lua_script';
  const MAX_CONSOLE_ENTRIES = 1000;

  const DEFAULT_SCRIPT = `-- Exemplo seguro: somente registra eventos e o encerramento.
-- Para ações de mouse/teclado, selecione um preset ou adicione chamadas da API ATK.
local capabilities = atk.capabilities
atk.log.write("ATK Lua API v" .. tostring(capabilities.api_version))
atk.log.write("Teclado: " .. tostring(capabilities.keyboard) ..
    "; rolagem: " .. tostring(capabilities.scroll) ..
    "; texto Unicode: " .. tostring(capabilities.unicode_text))

atk.on_event(function(event)
    local message = tostring(event.name)
    if event.arg ~= nil then
        message = message .. " arg=" .. tostring(event.arg)
    end
    if event.family ~= nil then
        message = message .. " família=" .. tostring(event.family)
    end
    atk.log.write(message)
end)

atk.on_stop(function()
    atk.log.write("Script encerrado.")
end)
`;

  const PRESETS = {
    template: DEFAULT_SCRIPT,
    autoclicker: `-- Exemplo de ação: mantenha o botão lateral 5 pressionado para clicar.
atk.on_event(function(event)
    if event.name == "MOUSE_BUTTON_PRESSED" and event.arg == 5 then
        while atk.mouse.is_pressed(5) do
            atk.mouse.click(1, 25)
            atk.time.sleep(25)
        end
    end
end)
`,
    recoil: `-- Exemplo relativo: botão esquerdo (1) + direito (3) mantidos.
atk.on_event(function(event)
    if event.name == "MOUSE_BUTTON_PRESSED" and event.arg == 1 and
       atk.mouse.is_pressed(1) and atk.mouse.is_pressed(3) then
        while atk.mouse.is_pressed(1) and atk.mouse.is_pressed(3) do
            atk.mouse.move(0, 2)
            atk.time.sleep(15)
        end
    end
end)
`,
    keyboard: `-- Exemplo explícito de captura de teclado enquanto este script roda.
-- A captura fica desativada por padrão e é desligada ao encerrar o script.
atk.keyboard.capture_events(true)
atk.on_event(function(event)
    if event.family == "keyboard" then
        atk.log.write(tostring(event.name) .. " scan_code=" .. tostring(event.arg))
    end
end)
`,
    sequence: `-- Exemplo: pressione o botão lateral 4 para executar uma sequência curta.
atk.on_event(function(event)
    if event.name == "MOUSE_BUTTON_PRESSED" and event.arg == 4 then
        atk.sequence({
            { kind = "move", x = 0, y = 20 },
            { kind = "sleep", ms = 80 },
            { kind = "move", x = 0, y = -20 },
            { kind = "scroll", vertical = 120 }
        })
    end
end)
`
  };

  // Mount function called when LuaSetting component mounts in Vue
  window.__mountLuaUI = function(container) {
    if (!container) return;
    console.log('[ATK HUB] Mounting Lua Script UI in main pane');

    let scriptLibrary = null;
    let activeScript = null;
    let libraryInitializationError = '';
    try {
      if (!window.__atkLuaLibrary || typeof window.__atkLuaLibrary.create !== 'function') {
        throw new Error('Biblioteca de scripts Lua indisponível.');
      }
      scriptLibrary = window.__atkLuaLibrary.create(localStorage, DEFAULT_SCRIPT);
      activeScript = scriptLibrary.getActive();
      if (!activeScript || typeof activeScript.source !== 'string' || typeof activeScript.id !== 'string') {
        throw new Error('A biblioteca retornou um script ativo inválido.');
      }
    } catch (error) {
      libraryInitializationError = error && error.message ? error.message : String(error);
      scriptLibrary = null;
      try {
        const legacySource = localStorage.getItem(STORAGE_KEY);
        activeScript = { id: '', name: 'Script local', source: legacySource === null ? DEFAULT_SCRIPT : legacySource };
      } catch (storageError) {
        activeScript = { id: '', name: 'Script local', source: DEFAULT_SCRIPT };
        libraryInitializationError += ' Não foi possível ler o salvamento local: ' +
          (storageError && storageError.message ? storageError.message : String(storageError));
      }
    }
    const savedCode = activeScript.source;

    container.innerHTML = `
      <div class="atk-lua-page" style="
        width: 100%;
        height: 100%;
        display: flex;
        flex-direction: column;
        padding: 16px;
        box-sizing: border-box;
        gap: 14px;
        overflow-y: auto;
        font-family: HarmonyOS_Sans_SC_Medium, 'Segoe UI', -apple-system, sans-serif;
      ">
        <!-- Card 1: Lua Script Editor & Controls (Authentic ATK HUB Card Style) -->
        <div class="atk-card" style="
          background: #ffffff;
          border-radius: 8px;
          padding: 20px 24px;
          box-shadow: 0 1px 3px rgba(0, 0, 0, 0.02);
          display: flex;
          flex-direction: column;
          gap: 14px;
        ">
          <!-- Card Header & Actions -->
          <div style="display: flex; justify-content: space-between; align-items: flex-start; gap: 16px; flex-wrap: wrap;">
            <div>
              <div style="display: flex; align-items: center; gap: 8px;">
                <svg width="18" height="18" viewBox="0 0 20 20" fill="none" xmlns="http://www.w3.org/2000/svg" style="color: #1d2129;">
                  <g stroke="currentColor" stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5">
                    <path d="m5.333 4.333-4 4.144 4 3.856m5.334-8 4 4.144-4 3.856"></path>
                    <path d="M9.333 1.333 7 14.666"></path>
                  </g>
                </svg>
                <span style="font-size: 15px; font-weight: 600; color: #1d2129;">Script Lua (API ATK v1 + compatibilidade G HUB)</span>
                <span id="atk-lua-badge" style="
                  font-size: 11px;
                  padding: 2px 10px;
                  border-radius: 12px;
                  font-weight: 600;
                  background: #f2f3f5;
                  color: #86909c;
                ">Consultando</span>
              </div>
              <div style="font-size: 12px; color: #86909c; margin-top: 4px; line-height: 18px;">
                Scripts Lua executados no PC. A API ATK oferece funções próprias de mouse, teclado, rolagem, eventos, sequências e encerramento; parte dos nomes G HUB também é compatível.
              </div>
              <details style="font-size: 12px; color: #4e5969; margin-top: 8px; max-width: 820px;">
                <summary style="cursor: pointer; color: #165dff;">Ajuda da API ATK v1 e capacidades</summary>
                <div style="white-space: pre-wrap; line-height: 19px; margin-top: 6px;">Mouse: atk.mouse.press/release/click/is_pressed, move, move_to, position e scroll. Teclado: atk.keyboard.press/release/tap/is_pressed/is_modifier_pressed/is_lock_on, type_text, chord e capture_events. Captura física do teclado começa desligada; ative-a explicitamente no script.
Tempo e sequência: atk.time.sleep/sleep_us/now_us e atk.sequence. Eventos: atk.on_event recebe name, arg, family e time_us; ao defini-lo, ele substitui OnEvent sem duplicar a entrega. Encerramento: atk.on_stop. Consulte os recursos disponíveis em atk.capabilities (api_version, keyboard, unicode_text, absolute_mouse, mouse_position, scroll e keyboard_events).
Coordenadas absolutas usam pixels da área de trabalho virtual e podem ser negativas. Rolagem usa unidades de 120. Teclas numéricas são códigos de varredura Set-1. O template só registra eventos; os presets de ação só executam depois de selecionados e iniciados.</div>
              </details>
              <div id="atk-lua-status-message" role="status" aria-live="polite" style="display: none; font-size: 12px; color: #f53f3f; margin-top: 4px; line-height: 18px;"></div>
            </div>

            <!-- Action Buttons matching ATK HUB segmented style -->
            <div style="display: flex; align-items: center; gap: 8px; flex-wrap: wrap;">
              <select id="atk-lua-presets-select" style="
                background: #f2f3f5;
                color: #1d2129;
                border: 1px solid #e5e6eb;
                border-radius: 6px;
                padding: 6px 12px;
                font-size: 12px;
                font-family: inherit;
                outline: none;
                cursor: pointer;
              ">
                <option value="">Selecionar um preset seguro…</option>
                <option value="template">Modelo seguro: registrar eventos (API ATK)</option>
                <option value="autoclicker">Exemplo de ação: clique enquanto segura botão 5</option>
                <option value="recoil">Exemplo de ação: movimento relativo</option>
                <option value="keyboard">Exemplo: captura de eventos do teclado</option>
                <option value="sequence">Exemplo de sequência: movimento e rolagem</option>
              </select>

              <button id="atk-lua-clear-btn" style="
                background: #f2f3f5;
                color: #4e5969;
                border: none;
                border-radius: 6px;
                padding: 6px 14px;
                font-size: 12px;
                font-weight: 500;
                font-family: inherit;
                cursor: pointer;
                transition: background 0.15s ease;
              ">Limpar Console</button>

              <button id="atk-lua-stop-btn" style="
                background: #ffece8;
                color: #f53f3f;
                border: 1px solid #ffccc7;
                border-radius: 6px;
                padding: 6px 16px;
                font-size: 12px;
                font-weight: 600;
                font-family: inherit;
                cursor: pointer;
                display: ${luaStatus.running || luaStatus.state === 'failed' ? 'inline-block' : 'none'};
              ">Parar</button>

              <button id="atk-lua-run-btn" style="
                background: #000000;
                color: #ffffff;
                border: none;
                border-radius: 6px;
                padding: 6px 20px;
                font-size: 12px;
                font-weight: 600;
                font-family: inherit;
                cursor: pointer;
                box-shadow: 0 1px 4px rgba(0, 0, 0, 0.15);
                display: ${luaStatus.running ? 'none' : 'inline-block'};
              ">Executar</button>
            </div>
          </div>

          <div class="atk-lua-library" style="display: flex; flex-direction: column; gap: 8px; padding: 12px; background: #f7f8fa; border: 1px solid #e5e6eb; border-radius: 6px;">
            <div style="display: flex; align-items: center; gap: 8px; flex-wrap: wrap;">
              <label for="atk-lua-script-select" style="font-size: 12px; color: #4e5969;">Script</label>
              <select id="atk-lua-script-select" aria-label="Script ativo" style="min-width: 200px; max-width: 100%; background: #fff; color: #1d2129; border: 1px solid #e5e6eb; border-radius: 6px; padding: 6px 10px; font: inherit;"></select>
              <button id="atk-lua-new-script-btn" type="button" style="padding: 6px 10px; border: 1px solid #e5e6eb; border-radius: 6px; background: #fff; cursor: pointer;">Novo</button>
              <button id="atk-lua-rename-script-btn" type="button" style="padding: 6px 10px; border: 1px solid #e5e6eb; border-radius: 6px; background: #fff; cursor: pointer;">Renomear</button>
              <button id="atk-lua-save-script-btn" type="button" style="padding: 6px 10px; border: 1px solid #e5e6eb; border-radius: 6px; background: #fff; cursor: pointer;">Salvar agora</button>
              <button id="atk-lua-checkpoint-script-btn" type="button" style="padding: 6px 10px; border: 1px solid #e5e6eb; border-radius: 6px; background: #fff; cursor: pointer;">Criar versão</button>
            </div>
            <form id="atk-lua-script-name-form" style="display: none; align-items: center; gap: 8px; flex-wrap: wrap;">
              <label id="atk-lua-script-name-label" for="atk-lua-script-name-input" style="font-size: 12px; color: #4e5969;">Nome do script</label>
              <input id="atk-lua-script-name-input" type="text" maxlength="64" autocomplete="off" style="min-width: 180px; max-width: 100%; padding: 6px 9px; border: 1px solid #e5e6eb; border-radius: 6px;">
              <button id="atk-lua-script-name-submit" type="submit" style="padding: 6px 10px; border: 1px solid #165dff; border-radius: 6px; background: #165dff; color: #fff; cursor: pointer;">Criar</button>
              <button id="atk-lua-script-name-cancel" type="button" style="padding: 6px 10px; border: 1px solid #e5e6eb; border-radius: 6px; background: #fff; cursor: pointer;">Cancelar</button>
            </form>
            <div style="display: flex; align-items: center; gap: 8px; flex-wrap: wrap;">
              <label for="atk-lua-version-select" style="font-size: 12px; color: #4e5969;">Versões salvas</label>
              <select id="atk-lua-version-select" aria-label="Versão salva para restaurar" style="min-width: 200px; max-width: 100%; background: #fff; color: #1d2129; border: 1px solid #e5e6eb; border-radius: 6px; padding: 6px 10px; font: inherit;"></select>
              <button id="atk-lua-restore-version-btn" type="button" style="padding: 6px 10px; border: 1px solid #e5e6eb; border-radius: 6px; background: #fff; cursor: pointer;">Restaurar versão</button>
            </div>
            <div id="atk-lua-library-message" role="status" aria-live="polite" style="font-size: 12px; color: #4e5969;"></div>
            <div id="atk-lua-library-warning" role="status" aria-live="polite" style="font-size: 12px; color: #ff7d00;"></div>
          </div>

          <div class="atk-lua-shortcut-panel" style="display: flex; align-items: center; gap: 8px; flex-wrap: wrap; padding: 12px; background: #f7f8fa; border: 1px solid #e5e6eb; border-radius: 6px;">
            <label for="atk-lua-stop-shortcut-input" style="font-size: 12px; font-weight: 600; color: #4e5969;">Atalho global para parar Lua</label>
            <input id="atk-lua-stop-shortcut-input" type="text" value="Ctrl+Alt+F11" maxlength="40" autocomplete="off" aria-describedby="atk-lua-stop-shortcut-help" style="width: 150px; max-width: 100%; padding: 6px 9px; border: 1px solid #e5e6eb; border-radius: 6px; font: inherit;">
            <button id="atk-lua-stop-shortcut-apply" type="button" style="padding: 6px 12px; border: 1px solid #165dff; border-radius: 6px; background: #165dff; color: #fff; cursor: pointer;">Aplicar atalho</button>
            <div id="atk-lua-stop-shortcut-status" role="status" aria-live="polite" style="font-size: 12px; color: #4e5969;"></div>
            <div id="atk-lua-stop-shortcut-help" style="flex-basis: 100%; font-size: 11px; color: #86909c;">Use um modificador e uma tecla, por exemplo Ctrl+Alt+F11. O estado mostra se o registro global foi aceito pelo Windows.</div>
          </div>

          <!-- Editor Wrapper -->
          <div style="
            display: flex;
            flex-direction: column;
            background: #1e1e24;
            border: 1px solid #e5e6eb;
            border-radius: 6px;
            overflow: hidden;
          ">
            <div style="
              height: 28px;
              background: #282830;
              border-bottom: 1px solid #33333d;
              display: flex;
              align-items: center;
              padding: 0 12px;
              justify-content: space-between;
            ">
              <span style="font-size: 11px; font-weight: 600; color: #a0a5b5; letter-spacing: 0.5px;">EDITOR LUA</span>
              <span style="font-size: 11px; color: #707585;">Auto-salvo localmente</span>
            </div>
            <textarea id="atk-lua-code" spellcheck="false" style="
              height: 240px;
              background: transparent;
              color: #f8f8f2;
              font-family: 'Consolas', 'Fira Code', monospace;
              font-size: 13px;
              line-height: 1.6;
              padding: 12px;
              border: none;
              outline: none;
              resize: vertical;
              tab-size: 4;
            "></textarea>
          </div>
        </div>

        <!-- Card 2: Real-time Output Console (Authentic ATK HUB Card Style) -->
        <div class="atk-card" style="
          background: #ffffff;
          border-radius: 8px;
          padding: 20px 24px;
          box-shadow: 0 1px 3px rgba(0, 0, 0, 0.02);
          display: flex;
          flex-direction: column;
          gap: 12px;
        ">
          <!-- Console Header -->
          <div class="atk-lua-console-toolbar" style="display: flex; justify-content: space-between; align-items: center; gap: 8px; flex-wrap: wrap;">
            <div style="display: flex; align-items: center; gap: 8px;">
              <svg width="18" height="18" viewBox="0 0 20 20" fill="none" xmlns="http://www.w3.org/2000/svg" style="color: #1d2129;">
                <path d="M17.5 3.333h-15a.833.833 0 0 0-.833.833v11.667c0 .46.373.833.833.833h15c.46 0 .833-.373.833-.833V4.166a.833.833 0 0 0-.833-.833Z" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"></path>
                <path d="m6 8 3 2.5-3 2.5m5 0h3" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"></path>
              </svg>
              <span style="font-size: 14px; font-weight: 600; color: #1d2129;">Console / Saída em Tempo Real</span>
            </div>
            <div style="display: flex; align-items: center; gap: 8px; flex-wrap: wrap;">
              <label for="atk-lua-log-filter" style="font-size: 11px; color: #86909c;">Mostrar</label>
              <select id="atk-lua-log-filter" aria-label="Filtrar mensagens do console">
                <option value="all">Todas</option>
                <option value="error">Erros</option>
                <option value="warning">Avisos</option>
              </select>
              <button id="atk-lua-log-pause" type="button" aria-pressed="false">Pausar rolagem</button>
              <span id="atk-lua-msg-count" style="font-size: 11px; color: #86909c;">0 mensagens</span>
            </div>
          </div>

          <!-- Console Terminal Box -->
          <div id="atk-lua-console" role="log" aria-live="off" tabindex="0" style="
            height: 150px;
            background: #f7f8fa;
            border: 1px solid #e5e6eb;
            border-radius: 6px;
            padding: 10px 14px;
            overflow-y: auto;
            font-family: 'Consolas', monospace;
            font-size: 12px;
            line-height: 1.6;
            color: #1d2129;
          ">
            <div style="color: #86909c;">[Console pronto] Aguardando execução do script Lua...</div>
          </div>
        </div>
      </div>
    `;

    // Elements
    const editor = container.querySelector('#atk-lua-code');
    const consoleBox = container.querySelector('#atk-lua-console');
    const msgCount = container.querySelector('#atk-lua-msg-count');
    const runBtn = container.querySelector('#atk-lua-run-btn');
    const stopBtn = container.querySelector('#atk-lua-stop-btn');
    const clearBtn = container.querySelector('#atk-lua-clear-btn');
    const presetsSelect = container.querySelector('#atk-lua-presets-select');
    const badge = container.querySelector('#atk-lua-badge');
    const statusMessage = container.querySelector('#atk-lua-status-message');
    const scriptSelect = container.querySelector('#atk-lua-script-select');
    const newScriptBtn = container.querySelector('#atk-lua-new-script-btn');
    const renameScriptBtn = container.querySelector('#atk-lua-rename-script-btn');
    const saveScriptBtn = container.querySelector('#atk-lua-save-script-btn');
    const checkpointScriptBtn = container.querySelector('#atk-lua-checkpoint-script-btn');
    const scriptNameForm = container.querySelector('#atk-lua-script-name-form');
    const scriptNameLabel = container.querySelector('#atk-lua-script-name-label');
    const scriptNameInput = container.querySelector('#atk-lua-script-name-input');
    const scriptNameSubmit = container.querySelector('#atk-lua-script-name-submit');
    const scriptNameCancel = container.querySelector('#atk-lua-script-name-cancel');
    const versionSelect = container.querySelector('#atk-lua-version-select');
    const restoreVersionBtn = container.querySelector('#atk-lua-restore-version-btn');
    const libraryMessage = container.querySelector('#atk-lua-library-message');
    const libraryWarning = container.querySelector('#atk-lua-library-warning');
    const shortcutInput = container.querySelector('#atk-lua-stop-shortcut-input');
    const shortcutApplyBtn = container.querySelector('#atk-lua-stop-shortcut-apply');
    const shortcutStatus = container.querySelector('#atk-lua-stop-shortcut-status');
    const logFilter = container.querySelector('#atk-lua-log-filter');
    const logPauseBtn = container.querySelector('#atk-lua-log-pause');

    if (activeLuaUi) {
      activeLuaUi.active = false;
      if (logPollInterval) clearInterval(logPollInterval);
    }
    const ui = {
      active: true,
      polling: false,
      pollingPromise: null,
      actionBusy: false,
      statusRequestId: 0,
      statusAppliedId: 0,
      statusQueryMessage: '',
      logsQueryMessage: '',
      logCursor: null,
      logIds: new Set(),
      logNodes: new Map(),
      logFilter: 'all',
      logScrollPaused: false,
      logCount: 0,
      logsPromise: null,
      clearingLogs: false,
      scriptFormMode: null,
      shortcutRequestId: 0,
      logsCleared: false,
      actionMessage: ''
    };
    activeLuaUi = ui;

    editor.value = savedCode;
    if (libraryInitializationError) {
      libraryMessage.textContent = 'Biblioteca de scripts indisponível: ' + libraryInitializationError;
      libraryMessage.style.color = '#f53f3f';
    }
    try {
      refreshScriptOptions();
    } catch (error) {
      showLibraryMessage('Não foi possível carregar scripts e versões: ' + errorMessage(error), true);
      refreshLibraryWarnings();
    }
    renderConsolePlaceholder();

    function errorMessage(error) {
      return error && error.message ? error.message : String(error);
    }

    async function invokeCommand(command, ...args) {
      const core = window.__TAURI__ && window.__TAURI__.core;
      if (!core || typeof core.invoke !== 'function') {
        throw new Error('Bridge Tauri indisponível; não foi possível chamar ' + command + '.');
      }
      return core.invoke(command, ...args);
    }

    function showLibraryMessage(message, isError = false) {
      libraryMessage.textContent = message || '';
      libraryMessage.style.color = isError ? '#f53f3f' : '#4e5969';
    }

    function refreshLibraryWarnings() {
      if (!libraryWarning) return;
      if (!scriptLibrary || typeof scriptLibrary.getWarnings !== 'function') {
        libraryWarning.textContent = '';
        return;
      }
      try {
        const warnings = scriptLibrary.getWarnings();
        libraryWarning.textContent = Array.isArray(warnings) ? warnings.map(String).join(' ') : '';
      } catch (error) {
        libraryWarning.textContent = 'Não foi possível carregar os avisos de recuperação: ' + errorMessage(error);
      }
    }

    function clearOptions(select) {
      if (typeof select.replaceChildren === 'function') select.replaceChildren();
      else while (select.firstChild) select.removeChild(select.firstChild);
    }

    function addOption(select, value, label) {
      const option = document.createElement('option');
      option.value = String(value);
      option.textContent = String(label);
      select.appendChild(option);
      return option;
    }

    function refreshVersionOptions() {
      if (!scriptLibrary) return;
      const versions = scriptLibrary.versions();
      clearOptions(versionSelect);
      versions.forEach((version, index) => {
        const savedAt = version.savedAt ? new Date(version.savedAt) : null;
        const timeLabel = savedAt && !Number.isNaN(savedAt.getTime()) ? savedAt.toLocaleString() : 'data indisponível';
        addOption(versionSelect, version.id, 'Versão ' + (versions.length - index) + ' · ' + timeLabel);
      });
      restoreVersionBtn.disabled = versions.length === 0;
      if (versions.length) versionSelect.value = versions[0].id;
    }

    function refreshScriptOptions() {
      if (!scriptLibrary) {
        scriptSelect.disabled = true;
        newScriptBtn.disabled = true;
        renameScriptBtn.disabled = true;
        saveScriptBtn.disabled = true;
        checkpointScriptBtn.disabled = true;
        versionSelect.disabled = true;
        restoreVersionBtn.disabled = true;
        return;
      }
      const scripts = scriptLibrary.list();
      clearOptions(scriptSelect);
      scripts.forEach(script => addOption(scriptSelect, script.id, script.name));
      scriptSelect.value = activeScript.id;
      const active = scripts.find(script => script.id === activeScript.id);
      renameScriptBtn.disabled = !active;
      refreshVersionOptions();
      refreshLibraryWarnings();
    }

    function saveActiveSource(checkpoint = false) {
      if (!scriptLibrary) throw new Error(libraryInitializationError || 'Biblioteca de scripts Lua indisponível.');
      activeScript = scriptLibrary.save(editor.value, { checkpoint });
      refreshScriptOptions();
      return activeScript;
    }

    function captureBeforeReplacement() {
      saveActiveSource(true);
    }

    function closeScriptNameForm() {
      ui.scriptFormMode = null;
      scriptNameForm.style.display = 'none';
      scriptNameInput.value = '';
    }

    function openScriptNameForm(mode) {
      if (!scriptLibrary) return;
      ui.scriptFormMode = mode;
      const rename = mode === 'rename';
      scriptNameLabel.textContent = rename ? 'Novo nome do script' : 'Nome do novo script';
      scriptNameSubmit.textContent = rename ? 'Renomear' : 'Criar';
      scriptNameInput.value = rename ? activeScript.name : '';
      scriptNameForm.style.display = 'flex';
      scriptNameInput.focus();
    }

    function classifyLog(message) {
      if (message.includes('[ERRO]') || /Erro:/i.test(message)) return 'error';
      if (message.includes('[WARN]') || /Aviso:/i.test(message)) return 'warning';
      return 'info';
    }

    function renderConsolePlaceholder() {
      if (ui.logCount !== 0) return;
      consoleBox.replaceChildren();
      const placeholder = document.createElement('div');
      placeholder.style.color = '#86909c';
      placeholder.textContent = ui.logsCleared ? '[Console limpo]' : '[Console pronto] Aguardando execução do script Lua...';
      consoleBox.appendChild(placeholder);
    }

    function updateLogCount() {
      msgCount.textContent = ui.logCount + (ui.logCount === 1 ? ' mensagem' : ' mensagens');
    }

    function isTextSelected() {
      if (typeof window.getSelection !== 'function') return false;
      const selection = window.getSelection();
      return Boolean(selection && String(selection));
    }

    function selectedLogNode() {
      if (typeof window.getSelection !== 'function') return null;
      const selection = window.getSelection();
      if (!selection || !selection.rangeCount || typeof selection.getRangeAt !== 'function') return null;
      let node = selection.getRangeAt(0).commonAncestorContainer;
      if (node && node.nodeType === 3) node = node.parentNode;
      while (node && node.parentNode !== consoleBox) node = node.parentNode;
      return node && Array.from(ui.logNodes.values()).includes(node) ? node : null;
    }

    function pruneOldestLog() {
      let selectedNode = selectedLogNode();
      let oldestId = null;
      for (const id of ui.logIds) {
        if (ui.logNodes.get(id) !== selectedNode) {
          oldestId = id;
          break;
        }
      }
      if (oldestId === null) oldestId = ui.logIds.values().next().value;
      if (oldestId === undefined) return;
      const node = ui.logNodes.get(oldestId);
      const index = node ? Array.prototype.indexOf.call(consoleBox.children, node) : -1;
      const scrollTop = consoleBox.scrollTop;
      if (node && node.parentNode === consoleBox) consoleBox.removeChild(node);
      ui.logIds.delete(oldestId);
      ui.logNodes.delete(oldestId);
      ui.logCount -= 1;
      if (index === 0 && scrollTop > 0 && selectedNode !== node) {
        consoleBox.scrollTop = Math.max(0, scrollTop - (node.offsetHeight || 20));
      }
    }

    function appendLogEntry(entry) {
      if (ui.logIds.has(entry.id)) return;
      const wasAtBottom = consoleBox.scrollHeight - consoleBox.scrollTop <= consoleBox.clientHeight + 4;
      const selectedText = isTextSelected();
      if (ui.logCount === 0) consoleBox.replaceChildren();

      const message = String(entry.message);
      const level = classifyLog(message);
      const line = document.createElement('div');
      line.logLevel = level;
      line.style.color = level === 'error' ? '#f53f3f' :
        level === 'warning' ? '#ff7d00' :
        message.includes('[Lua Worker]') ? '#165dff' : '#00b42a';
      line.style.whiteSpace = 'pre-wrap';
      line.textContent = message;
      line.style.display = ui.logFilter === 'all' || ui.logFilter === level ? '' : 'none';
      consoleBox.appendChild(line);

      ui.logIds.add(entry.id);
      ui.logNodes.set(entry.id, line);
      ui.logCount += 1;
      ui.logsCleared = false;
      while (ui.logCount > MAX_CONSOLE_ENTRIES) pruneOldestLog();
      updateLogCount();
      if (!ui.logScrollPaused && wasAtBottom && !selectedText) {
        consoleBox.scrollTop = consoleBox.scrollHeight;
      }
    }

    function resetConsole() {
      ui.logIds.clear();
      ui.logNodes.clear();
      ui.logCount = 0;
      updateLogCount();
      renderConsolePlaceholder();
    }

    function renderLogFilter() {
      ui.logNodes.forEach(node => {
        node.style.display = ui.logFilter === 'all' || ui.logFilter === node.logLevel ? '' : 'none';
      });
    }

    function renderStatusMessage(message) {
      if (!statusMessage) return;
      statusMessage.textContent = message || '';
      statusMessage.style.display = message ? 'block' : 'none';
    }

    function renderStatus(status) {
      luaStatus = status;
      const labels = {
        stopped: { label: 'Parado', background: '#f2f3f5', color: '#86909c' },
        starting: { label: 'Iniciando', background: '#fff7e8', color: '#ff7d00' },
        running: { label: 'Executando', background: '#e8ffea', color: '#00b42a' },
        stopping: { label: 'Parando', background: '#fff7e8', color: '#ff7d00' },
        failed: { label: 'Falha', background: '#ffece8', color: '#f53f3f' }
      };
      const visual = labels[status.state] || { label: 'Estado desconhecido', background: '#f2f3f5', color: '#86909c' };
      if (badge) {
        badge.style.background = visual.background;
        badge.style.color = visual.color;
        badge.textContent = visual.label;
      }
      const stopAvailable = status.running || ['starting', 'stopping', 'failed'].includes(status.state);
      if (runBtn) {
        const startAvailable = !status.running && !['starting', 'stopping'].includes(status.state);
        runBtn.style.display = startAvailable ? 'inline-block' : 'none';
        runBtn.disabled = ui.actionBusy;
      }
      if (stopBtn) {
        stopBtn.style.display = stopAvailable ? 'inline-block' : 'none';
        stopBtn.disabled = ui.actionBusy;
      }
      renderStatusMessage(status.message || ui.actionMessage || ui.statusQueryMessage || ui.logsQueryMessage);
    }

    async function syncStatus() {
      const requestId = ++ui.statusRequestId;
      try {
        const status = await invokeCommand('lua_status');
        if (!status || typeof status.running !== 'boolean' || typeof status.state !== 'string') {
          throw new Error('O backend retornou um estado Lua inválido.');
        }
        if (!ui.active || requestId < ui.statusAppliedId) return;
        ui.statusAppliedId = requestId;
        ui.statusQueryMessage = '';
        renderStatus({ running: status.running, state: status.state, message: status.message || null });
      } catch (error) {
        if (!ui.active || requestId < ui.statusAppliedId) return;
        ui.statusAppliedId = requestId;
        ui.statusQueryMessage = 'Não foi possível consultar o estado Lua: ' + errorMessage(error);
        renderStatus(luaStatus);
      }
    }

    async function syncLogs() {
      if (ui.clearingLogs) return;
      if (ui.logsPromise) return ui.logsPromise;
      ui.logsPromise = (async () => {
        try {
          const response = await invokeCommand('lua_logs_since', { cursor: ui.logCursor });
          if (!ui.active) return;
          if (!response || typeof response.cursor !== 'string' || typeof response.reset !== 'boolean' ||
              !Array.isArray(response.entries) ||
              !response.entries.every(entry => entry && typeof entry.id === 'string' && typeof entry.message === 'string')) {
            throw new Error('O backend retornou uma resposta incremental do console Lua inválida.');
          }
          if (response.reset) resetConsole();
          ui.logCursor = response.cursor;
          response.entries.forEach(appendLogEntry);
          if (ui.logCount === 0) renderConsolePlaceholder();
          ui.logsQueryMessage = '';
          renderStatus(luaStatus);
        } catch (error) {
          if (!ui.active) return;
          ui.logsQueryMessage = 'Não foi possível carregar o console Lua: ' + errorMessage(error);
          renderStatus(luaStatus);
        }
      })();
      try {
        await ui.logsPromise;
      } finally {
        ui.logsPromise = null;
      }
    }

    async function syncStopShortcut() {
      const requestId = ++ui.shortcutRequestId;
      try {
        const result = await invokeCommand('lua_stop_shortcut_get');
        if (!ui.active || requestId !== ui.shortcutRequestId) return;
        if (!result || typeof result.shortcut !== 'string' || typeof result.registered !== 'boolean') {
          throw new Error('O backend retornou um estado de atalho Lua inválido.');
        }
        shortcutInput.value = result.shortcut;
        if (result.registered) {
          if (result.error) {
            shortcutStatus.textContent = 'Atalho global ' + result.shortcut + ' registrado. Aviso: ' + String(result.error);
            shortcutStatus.style.color = '#ff7d00';
          } else {
            shortcutStatus.textContent = 'Atalho global ' + result.shortcut + ' registrado.';
            shortcutStatus.style.color = '#00a32a';
          }
        } else {
          shortcutStatus.textContent = 'Falha ao registrar ' + result.shortcut + ': ' +
            (result.error || 'o Windows não confirmou o registro.');
          shortcutStatus.style.color = '#f53f3f';
        }
      } catch (error) {
        if (!ui.active || requestId !== ui.shortcutRequestId) return;
        shortcutStatus.textContent = 'Não foi possível consultar o atalho global: ' + errorMessage(error);
        shortcutStatus.style.color = '#f53f3f';
      }
    }

    async function applyStopShortcut() {
      if (!ui.active || shortcutApplyBtn.disabled) return;
      shortcutApplyBtn.disabled = true;
      ui.shortcutRequestId += 1;
      const requestedShortcut = shortcutInput.value.trim();
      try {
        const result = await invokeCommand('lua_stop_shortcut_set', { shortcut: requestedShortcut });
        if (!ui.active) return;
        if (!result || typeof result.shortcut !== 'string' || typeof result.registered !== 'boolean') {
          throw new Error('O backend retornou um resultado de atalho Lua inválido.');
        }
        shortcutInput.value = result.shortcut;
        if (result.registered) {
          if (result.error) {
            shortcutStatus.textContent = 'Atalho global ' + result.shortcut + ' registrado. Aviso: ' + String(result.error);
            shortcutStatus.style.color = '#ff7d00';
          } else {
            shortcutStatus.textContent = 'Atalho global ' + result.shortcut + ' registrado.';
            shortcutStatus.style.color = '#00a32a';
          }
        } else {
          shortcutStatus.textContent = 'Falha ao registrar ' + result.shortcut + ': ' +
            (result.error || 'o Windows não confirmou o registro.');
          shortcutStatus.style.color = '#f53f3f';
        }
      } catch (error) {
        if (!ui.active) return;
        shortcutStatus.textContent = 'Falha ao registrar ' + (requestedShortcut || 'o atalho') + ': ' + errorMessage(error);
        shortcutStatus.style.color = '#f53f3f';
      } finally {
        if (ui.active) shortcutApplyBtn.disabled = false;
      }
    }

    function syncUi() {
      if (!ui.active) return Promise.resolve();
      if (ui.pollingPromise) return ui.pollingPromise;
      ui.polling = true;
      ui.pollingPromise = Promise.all([syncStatus(), syncLogs()])
        .finally(() => {
          ui.polling = false;
          ui.pollingPromise = null;
        });
      return ui.pollingPromise;
    }

    async function runAction(command, args, failurePrefix) {
      if (!ui.active || ui.actionBusy) return;
      ui.actionBusy = true;
      renderStatus(luaStatus);
      try {
        await invokeCommand(command, ...args);
        ui.actionMessage = '';
        ui.statusQueryMessage = '';
        ui.logsQueryMessage = '';
      } catch (error) {
        ui.actionMessage = failurePrefix + errorMessage(error);
      }
      await syncUi();
      if (ui.active) {
        ui.actionBusy = false;
        renderStatus(luaStatus);
      }
    }

    editor.addEventListener('input', () => {
      try {
        if (scriptLibrary) {
          activeScript = scriptLibrary.save(editor.value);
          showLibraryMessage('Salvo automaticamente: ' + activeScript.name);
          refreshVersionOptions();
          refreshLibraryWarnings();
        } else {
          localStorage.setItem(STORAGE_KEY, editor.value);
          showLibraryMessage('Salvo localmente; a biblioteca de scripts não está disponível.', true);
        }
      } catch (error) {
        showLibraryMessage('Erro ao salvar o script. O texto continua no editor: ' + errorMessage(error), true);
        refreshLibraryWarnings();
      }
    });

    newScriptBtn.addEventListener('click', () => openScriptNameForm('create'));
    renameScriptBtn.addEventListener('click', () => openScriptNameForm('rename'));
    scriptNameCancel.addEventListener('click', closeScriptNameForm);
    scriptNameForm.addEventListener('submit', event => {
      event.preventDefault();
      if (!scriptLibrary) return;
      const name = String(scriptNameInput.value).trim();
      if (!name) {
        showLibraryMessage('Informe um nome para o script.', true);
        scriptNameInput.focus();
        return;
      }
      try {
        if (ui.scriptFormMode === 'create') {
          captureBeforeReplacement();
          activeScript = scriptLibrary.create(name, '');
          editor.value = activeScript.source;
          showLibraryMessage('Script criado: ' + activeScript.name);
        } else if (ui.scriptFormMode === 'rename') {
          saveActiveSource();
          activeScript = scriptLibrary.rename(name);
          showLibraryMessage('Script renomeado para: ' + activeScript.name);
        }
        closeScriptNameForm();
        refreshScriptOptions();
      } catch (error) {
        showLibraryMessage('Não foi possível atualizar o script: ' + errorMessage(error), true);
        refreshLibraryWarnings();
      }
    });

    scriptSelect.addEventListener('change', () => {
      if (!scriptLibrary) return;
      const requestedId = scriptSelect.value;
      if (requestedId === activeScript.id) return;
      try {
        captureBeforeReplacement();
        activeScript = scriptLibrary.select(requestedId);
        editor.value = activeScript.source;
        showLibraryMessage('Script ativo: ' + activeScript.name);
        refreshScriptOptions();
      } catch (error) {
        scriptSelect.value = activeScript.id;
        showLibraryMessage('Não foi possível abrir o script. O texto atual continua no editor: ' + errorMessage(error), true);
        refreshLibraryWarnings();
      }
    });

    saveScriptBtn.addEventListener('click', () => {
      try {
        saveActiveSource();
        showLibraryMessage('Script salvo: ' + activeScript.name);
      } catch (error) {
        showLibraryMessage('Erro ao salvar o script. O texto continua no editor: ' + errorMessage(error), true);
        refreshLibraryWarnings();
      }
    });

    checkpointScriptBtn.addEventListener('click', () => {
      try {
        saveActiveSource();
        activeScript = scriptLibrary.checkpoint();
        refreshScriptOptions();
        showLibraryMessage('Versão criada para: ' + activeScript.name);
      } catch (error) {
        showLibraryMessage('Não foi possível criar uma versão. O texto continua no editor: ' + errorMessage(error), true);
        refreshLibraryWarnings();
      }
    });

    restoreVersionBtn.addEventListener('click', () => {
      if (!scriptLibrary || !versionSelect.value) return;
      const requestedVersionId = versionSelect.value;
      try {
        saveActiveSource(true);
        activeScript = scriptLibrary.restore(requestedVersionId);
        editor.value = activeScript.source;
        showLibraryMessage('Versão restaurada em: ' + activeScript.name);
        refreshScriptOptions();
      } catch (error) {
        showLibraryMessage('Não foi possível restaurar a versão. O texto atual continua no editor: ' + errorMessage(error), true);
        refreshLibraryWarnings();
      }
    });

    presetsSelect.addEventListener('change', () => {
      const selected = presetsSelect.value;
      if (!PRESETS[selected]) return;
      if (!scriptLibrary) {
        showLibraryMessage('Não é possível selecionar presets sem a biblioteca de scripts: ' + libraryInitializationError, true);
        return;
      }
      const presetNames = {
        template: 'Preset ATK · Modelo seguro',
        autoclicker: 'Preset ATK · Clique botão 5',
        recoil: 'Preset ATK · Movimento relativo',
        keyboard: 'Preset ATK · Captura de teclado',
        sequence: 'Preset ATK · Sequência e rolagem'
      };
      const presetName = presetNames[selected];
      try {
        saveActiveSource();
        activeScript = scriptLibrary.openPreset(presetName, PRESETS[selected]);
        editor.value = activeScript.source;
        showLibraryMessage('Preset protegido como script: ' + activeScript.name);
        refreshScriptOptions();
      } catch (error) {
        showLibraryMessage('Não foi possível abrir o preset. O texto atual continua no editor: ' + errorMessage(error), true);
        refreshLibraryWarnings();
      }
      presetsSelect.value = '';
    });

    shortcutApplyBtn.addEventListener('click', applyStopShortcut);

    logFilter.addEventListener('change', () => {
      ui.logFilter = ['error', 'warning'].includes(logFilter.value) ? logFilter.value : 'all';
      renderLogFilter();
    });

    logPauseBtn.addEventListener('click', () => {
      ui.logScrollPaused = !ui.logScrollPaused;
      logPauseBtn.textContent = ui.logScrollPaused ? 'Retomar rolagem' : 'Pausar rolagem';
      logPauseBtn.setAttribute('aria-pressed', ui.logScrollPaused ? 'true' : 'false');
    });

    // A successful command transport does not prove that the worker is running.
    runBtn.addEventListener('click', () => {
      const code = editor.value;
      return runAction('lua_start', [{ source: code }], 'Erro ao iniciar script Lua: ');
    });

    // Keep Stop available in failed state so the backend can clean up its session.
    stopBtn.addEventListener('click', () => {
      return runAction('lua_stop', [], 'Erro ao parar script Lua: ');
    });

    clearBtn.addEventListener('click', async () => {
      if (!ui.active || ui.clearingLogs) return;
      ui.clearingLogs = true;
      try {
        if (ui.logsPromise) await ui.logsPromise;
        await invokeCommand('lua_clear_log');
        ui.logsCleared = true;
        ui.logCursor = null;
        resetConsole();
        ui.logsQueryMessage = '';
        ui.clearingLogs = false;
        await syncLogs();
      } catch (error) {
        ui.actionMessage = 'Erro ao limpar o console Lua: ' + errorMessage(error);
        renderStatus(luaStatus);
      } finally {
        ui.clearingLogs = false;
      }
    });

    if (logPollInterval) clearInterval(logPollInterval);
    logPollInterval = setInterval(() => {
      if (!document.hidden) syncUi(true);
    }, 800);
    ui.onVisibilityChange = () => { if (!document.hidden && ui.active) syncUi(); };
    document.addEventListener('visibilitychange', ui.onVisibilityChange);
    syncUi();
    syncStopShortcut();
  };

  // Unmount callback when leaving Lua tab
  window.__unmountLuaUI = function() {
    console.log('[ATK HUB] Unmounting Lua Script UI');
    if (activeLuaUi) {
      document.removeEventListener('visibilitychange', activeLuaUi.onVisibilityChange);
      activeLuaUi.active = false;
      activeLuaUi = null;
    }
    if (logPollInterval) {
      clearInterval(logPollInterval);
      logPollInterval = null;
    }
  };

})();
