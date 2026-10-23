const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const bridgePath = path.resolve(__dirname, '../../apps/desktop/web/atk_tauri_bridge.js');
const bridgeSource = fs.readFileSync(bridgePath, 'utf8');
const libraryPath = path.resolve(__dirname, '../../apps/desktop/web/lua_script_library.js');
const librarySource = fs.existsSync(libraryPath) ? fs.readFileSync(libraryPath, 'utf8') : '';

class FakeElement {
  constructor(id) {
    this.id = id;
    this.style = {};
    this._textContent = '';
    this.children = [];
    this.value = '';
    this.disabled = false;
    this.clientHeight = 150;
    this.scrollTop = 0;
    this.listeners = new Map();
    this.attributes = new Map();
    this.logLevel = '';
    this.parentNode = null;
    this.offsetHeight = 20;
  }

  get textContent() {
    return this.children.length ? this.children.map(child => child.textContent).join('') : this._textContent;
  }

  set textContent(value) {
    this._textContent = String(value);
    this.children = [];
  }

  get innerHTML() {
    return this.textContent;
  }

  set innerHTML(value) {
    this.textContent = String(value);
  }

  get firstChild() {
    return this.children[0] || null;
  }

  get scrollHeight() {
    return Math.max(this.clientHeight, this.children.length * 20);
  }

  appendChild(child) {
    this.children.push(child);
    child.parentNode = this;
    return child;
  }

  removeChild(child) {
    const index = this.children.indexOf(child);
    if (index >= 0) this.children.splice(index, 1);
    child.parentNode = null;
    return child;
  }

  replaceChildren(...children) {
    this.children.forEach(child => { child.parentNode = null; });
    this.children = [...children];
    this.children.forEach(child => { child.parentNode = this; });
    this._textContent = '';
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value));
  }

  focus() {}

  getAttribute(name) {
    return this.attributes.get(name);
  }

  addEventListener(name, callback) {
    this.listeners.set(name, callback);
  }

  async fire(name, event = {}) {
    const callback = this.listeners.get(name);
    if (callback) return callback(event);
  }
}

class FakeContainer {
  constructor() {
    this.elements = new Map();
    this.html = '';
  }

  set innerHTML(value) {
    this.html = value;
    for (const [, id] of value.matchAll(/\bid="([^"]+)"/g)) {
      if (!this.elements.has(id)) this.elements.set(id, new FakeElement(id));
    }
  }

  get innerHTML() {
    return this.html;
  }

  querySelector(selector) {
    return this.elements.get(selector.slice(1)) || null;
  }

  get(id) {
    return this.elements.get(id);
  }
}

class FakeLocalStorage {
  constructor(initial = {}) {
    this.values = new Map(Object.entries(initial));
    this.failWriteKeys = new Set();
  }

  getItem(key) {
    return this.values.has(key) ? this.values.get(key) : null;
  }

  setItem(key, value) {
    if (this.failWriteKeys.has(key)) throw new Error('QuotaExceededError');
    this.values.set(key, String(value));
  }

  removeItem(key) {
    this.values.delete(key);
  }
}

function makeHarness({
  invoke,
  initialStorage = {},
  useLibrary = false,
  shortcutGetResult = { shortcut: 'Ctrl+Alt+F11', registered: true }
} = {}) {
  let nextIntervalId = 1;
  const intervals = new Map();
  const localStorage = new FakeLocalStorage(initialStorage);
  const alerts = [];
  const selection = { text: '' };
  const window = {
    addEventListener() {},
    alert(message) { alerts.push(String(message)); },
    getSelection() {
      return {
        toString: () => selection.text,
        get rangeCount() { return selection.node ? 1 : 0; },
        getRangeAt() { return { commonAncestorContainer: selection.node }; }
      };
    }
  };
  if (invoke) window.__TAURI__ = { core: { invoke: async (command, ...args) => {
    if (command === 'lua_stop_shortcut_get') return shortcutGetResult;
    return invoke(command, ...args);
  } } };
  const document = {
    addEventListener() {},
    createElement(tagName) { return new FakeElement(tagName); }
  };
  const context = {
    window,
    document,
    localStorage,
    console: { log() {}, warn() {}, error() {} },
    setInterval(callback, delay) {
      const id = nextIntervalId++;
      intervals.set(id, { callback, delay, cleared: false });
      return id;
    },
    clearInterval(id) {
      const interval = intervals.get(id);
      if (interval) interval.cleared = true;
    }
  };
  if (useLibrary) vm.runInNewContext(librarySource, context, { filename: libraryPath });
  vm.runInNewContext(bridgeSource, context, { filename: bridgePath });
  const container = new FakeContainer();
  return { window, localStorage, intervals, container, alerts, selection };
}

function deferred() {
  let resolve;
  const promise = new Promise(done => { resolve = done; });
  return { promise, resolve };
}

function waitForAsyncWork() {
  return new Promise(resolve => setImmediate(resolve));
}

function plain(value) {
  if (Array.isArray(value)) return value.map(plain);
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, plain(item)]));
  }
  return value;
}

const el = (harness, id) => harness.container.get(`atk-lua-${id}`);

function activeLibraryScript(harness) {
  const state = JSON.parse(harness.localStorage.getItem('atk_vxe_lua_script_library_v1'));
  return state.scripts.find(script => script.id === state.activeId);
}

test('a missing Tauri bridge is reported by Lua controls and Windows settings without fallback values', async () => {
  const harness = makeHarness({ initialStorage: { atk_pointer_speed: '17', atk_roller_speed: '4' } });
  harness.window.__mountLuaUI(harness.container);
  await waitForAsyncWork();

  await el(harness, 'run-btn').fire('click');
  assert.match(el(harness, 'status-message').textContent, /Bridge Tauri indisponível/);
  assert.equal(el(harness, 'badge').textContent, 'Parado');
  assert.equal(el(harness, 'run-btn').style.display, 'inline-block');

  await assert.rejects(harness.window.__electronIpc.invoke('setPointerSpeed', 17), /Bridge Tauri indisponível/);
  await assert.rejects(harness.window.__electronIpc.invoke('setRollerSpeed', 5), /Bridge Tauri indisponível/);
  const visibleErrors = harness.alerts.length;
  await assert.rejects(harness.window.__electronIpc.invoke('getPointerSpeed'), /Bridge Tauri indisponível/);
  await assert.rejects(harness.window.__electronIpc.invoke('getRollerSpeed'), /Bridge Tauri indisponível/);
  assert.equal(harness.alerts.length, visibleErrors, 'falhas de leitura devem ser explícitas sem abrir pop-ups na inicialização');
  assert.equal(harness.localStorage.getItem('atk_pointer_speed'), '17');
  assert.equal(harness.localStorage.getItem('atk_roller_speed'), '4');
});

test('native export cancellation and settings bridge keep legacy preferences in their original storage', async () => {
  const initialSettings = { lowChargeTips: true, unrelatedPreference: 'keep' };
  const calls = [];
  let nativeSettings = null;
  let exportCancelled = true;
  const harness = makeHarness({
    initialStorage: { atk_system_settings: JSON.stringify(initialSettings) },
    invoke: async (command, payload) => {
      calls.push({ command, payload });
      if (command === 'system_migrate_legacy_settings') {
        if (nativeSettings === null) nativeSettings = {
          openAtLogin: false,
          startMinimized: false,
          closeMinimizeTray: true,
          lowChargeTips: payload.settings.lowChargeTips
        };
        return undefined;
      }
      if (command === 'system_get_settings') return { ...nativeSettings };
      if (command === 'system_set_settings') {
        Object.assign(nativeSettings, payload.patch);
        return { ...nativeSettings };
      }
      if (command === 'configuration_export') {
        assert.deepEqual(payload.configuration, { name: 'Perfil do usuário', dpi: 1100 });
        return exportCancelled ? { cancelled: true, path: null } : { cancelled: false, path: 'C:\\exports\\perfil.json' };
      }
      throw new Error(`Unexpected command: ${command}`);
    }
  });

  const cancelled = await harness.window.__electronIpc.invoke('saveConfiguration', { name: 'Perfil do usuário', dpi: 1100 });
  assert.deepEqual(cancelled, { cancelled: true, path: null });
  assert.equal(harness.alerts.length, 0, 'cancelar a janela de exportação não deve mostrar falso erro ou sucesso');

  assert.deepEqual(await harness.window.__electronIpc.invoke('getSetting'), {
    openAtLogin: false, startMinimized: false, closeMinimizeTray: true, lowChargeTips: true
  });
  await harness.window.__electronIpc.invoke('getSetting');
  assert.equal(calls.filter(call => call.command === 'system_migrate_legacy_settings').length, 1);
  assert.deepEqual(plain(calls.find(call => call.command === 'system_migrate_legacy_settings').payload), { settings: initialSettings });

  await harness.window.__electronIpc.invoke('setSetting', { openAtLogin: true, closeMinimizeTray: false });
  assert.deepEqual(plain(calls.at(-1)), {
    command: 'system_set_settings', payload: { patch: { openAtLogin: true, closeMinimizeTray: false } }
  });
  assert.deepEqual(nativeSettings, {
    openAtLogin: true, startMinimized: false, closeMinimizeTray: false, lowChargeTips: true
  });

  const legacyValueAfterSettings = harness.localStorage.getItem('atk_system_settings');
  assert.equal(legacyValueAfterSettings, JSON.stringify(initialSettings), 'native settings must not rewrite the legacy key on later reads or writes');
  exportCancelled = false;
  assert.deepEqual(await harness.window.__electronIpc.invoke('saveConfiguration', { name: 'Perfil do usuário', dpi: 1100 }), {
    cancelled: false, path: 'C:\\exports\\perfil.json'
  });
});

test('the safe ATK API template is sent to lua_start and backend status is queried after start', async () => {
  const calls = [];
  let status = { running: false, state: 'stopped', message: null };
  let startedSource = '';
  const harness = makeHarness({ invoke: async (command, ...args) => {
    calls.push({ command, args });
    if (command === 'lua_status') return { ...status };
    if (command === 'lua_logs_since') return { cursor: '0', reset: false, entries: [] };
    if (command === 'lua_start') {
      startedSource = args[0].source;
      status = { running: true, state: 'running', message: null };
      return { running: true, state: 'running', message: null };
    }
    throw new Error(`Unexpected command: ${command}`);
  } });

  harness.window.__mountLuaUI(harness.container);
  await waitForAsyncWork();
  const editor = el(harness, 'code');
  assert.match(editor.value, /atk\.on_event/);
  assert.match(editor.value, /atk\.on_stop/);
  assert.match(editor.value, /atk\.capabilities/);
  assert.doesNotMatch(editor.value, /(?:press|click|type_text|move_to)\s*\(/i, 'default must only log events and stop');

  await el(harness, 'run-btn').fire('click');
  assert.match(startedSource, /atk\.on_event/);
  assert.match(startedSource, /atk\.on_stop/);
  assert.ok(calls.filter(call => call.command === 'lua_status').length >= 2);
  assert.equal(el(harness, 'badge').textContent, 'Executando');
});

test('the recoil preset gates on left and right mouse buttons and shows queried starting/failed states', async () => {
  const calls = [];
  let status = { running: false, state: 'stopped', message: null };
  let startedSource = '';
  const harness = makeHarness({ useLibrary: true, invoke: async (command, ...args) => {
    calls.push(command);
    if (command === 'lua_status') return { ...status };
    if (command === 'lua_logs_since') return { cursor: '0', reset: false, entries: [] };
    if (command === 'lua_start') {
      startedSource = args[0].source;
      status = { running: false, state: 'starting', message: null };
      return { running: true, state: 'running', message: null };
    }
    if (command === 'lua_stop') {
      status = { running: false, state: 'stopped', message: null };
      return { ...status };
    }
    throw new Error(`Unexpected command: ${command}`);
  } });

  harness.window.__mountLuaUI(harness.container);
  await waitForAsyncWork();
  const presetSelect = el(harness, 'presets-select');
  presetSelect.value = 'recoil';
  await presetSelect.fire('change');
  await el(harness, 'run-btn').fire('click');

  assert.match(startedSource, /atk\.mouse\.is_pressed\(1\) and atk\.mouse\.is_pressed\(3\)/);
  assert.doesNotMatch(startedSource, /atk\.mouse\.is_pressed\(2\)/);
  assert.match(startedSource, /atk\.mouse\.move\(0, 2\)/);
  assert.equal(el(harness, 'badge').textContent, 'Iniciando');
  assert.equal(el(harness, 'run-btn').style.display, 'none');
  assert.equal(el(harness, 'stop-btn').style.display, 'inline-block');

  status = { running: false, state: 'failed', message: 'worker encerrou inesperadamente' };
  const poll = [...harness.intervals.values()][0];
  await poll.callback();
  assert.equal(el(harness, 'badge').textContent, 'Falha');
  assert.match(el(harness, 'status-message').textContent, /worker encerrou inesperadamente/);
  assert.equal(el(harness, 'stop-btn').style.display, 'inline-block');

  await el(harness, 'stop-btn').fire('click');
  assert.ok(calls.includes('lua_stop'));
  assert.equal(el(harness, 'badge').textContent, 'Parado');
  assert.equal(el(harness, 'run-btn').style.display, 'inline-block');
  assert.equal(el(harness, 'stop-btn').style.display, 'none');
});

test('keyboard capture and sequence examples are explicit presets and never start on selection', async () => {
  const started = [];
  const harness = makeHarness({ useLibrary: true, invoke: async command => {
    if (command === 'lua_status') return { running: false, state: 'stopped', message: null };
    if (command === 'lua_logs_since') return { cursor: '0', reset: false, entries: [] };
    if (command === 'lua_start') started.push(command);
    throw new Error(`Unexpected command: ${command}`);
  } });

  harness.window.__mountLuaUI(harness.container);
  await waitForAsyncWork();
  const presetSelect = el(harness, 'presets-select');
  const editor = el(harness, 'code');

  presetSelect.value = 'keyboard';
  await presetSelect.fire('change');
  assert.match(editor.value, /atk\.keyboard\.capture_events\(true\)/);
  assert.equal(activeLibraryScript(harness).source, editor.value);
  assert.equal(started.length, 0);

  presetSelect.value = 'sequence';
  await presetSelect.fire('change');
  assert.match(editor.value, /atk\.sequence\(\{/);
  assert.match(editor.value, /kind = "scroll"/);
  assert.equal(activeLibraryScript(harness).source, editor.value);
  assert.equal(started.length, 0);
});

test('rapid repeated run clicks start a saved custom script once and preserve its source', async () => {
  const savedScript = 'atk.on_event(function(event) atk.log.write(event.name) end)';
  const pendingStart = deferred();
  let status = { running: false, state: 'stopped', message: null };
  let startCalls = 0;
  const harness = makeHarness({
    initialStorage: { atk_vxe_lua_script: savedScript },
    invoke: async (command, args) => {
      if (command === 'lua_status') return { ...status };
      if (command === 'lua_logs_since') return { cursor: '0', reset: false, entries: [] };
      if (command === 'lua_start') {
        startCalls += 1;
        assert.equal(args.source, savedScript);
        return pendingStart.promise;
      }
      throw new Error(`Unexpected command: ${command}`);
    }
  });

  harness.window.__mountLuaUI(harness.container);
  await waitForAsyncWork();
  assert.equal(el(harness, 'code').value, savedScript);
  const firstStart = el(harness, 'run-btn').fire('click');
  await el(harness, 'run-btn').fire('click');
  assert.equal(startCalls, 1);

  status = { running: true, state: 'running', message: null };
  pendingStart.resolve({ running: true, state: 'running', message: null });
  await firstStart;
  assert.equal(startCalls, 1);
  assert.equal(harness.localStorage.getItem('atk_vxe_lua_script'), savedScript);
  assert.equal(el(harness, 'code').value, savedScript);
});

test('clearing logs reflects an empty backend console and polling preserves the saved editor text', async () => {
  const savedScript = 'function OnEvent(event, arg)\n    -- script do usuário\nend';
  let logs = [{ id: 'log-1', message: 'mensagem antiga' }];
  const harness = makeHarness({
    initialStorage: { atk_vxe_lua_script: savedScript },
    invoke: async command => {
      if (command === 'lua_status') return { running: false, state: 'stopped', message: null };
      if (command === 'lua_logs_since') return { cursor: '1', reset: false, entries: [...logs] };
      if (command === 'lua_clear_log') {
        logs = [];
        return undefined;
      }
      throw new Error(`Unexpected command: ${command}`);
    }
  });

  harness.window.__mountLuaUI(harness.container);
  await waitForAsyncWork();
  const editor = el(harness, 'code');
  assert.equal(editor.value, savedScript);
  assert.match(el(harness, 'console').innerHTML, /mensagem antiga/);

  editor.value = `${savedScript}\n-- edição local`;
  await editor.fire('input');
  const editedScript = editor.value;
  await el(harness, 'clear-btn').fire('click');
  assert.match(el(harness, 'console').innerHTML, /\[Console limpo\]/);
  assert.doesNotMatch(el(harness, 'console').innerHTML, /mensagem antiga/);
  assert.equal(el(harness, 'msg-count').textContent, '0 mensagens');

  await [...harness.intervals.values()][0].callback();
  assert.equal(editor.value, editedScript);
  assert.equal(harness.localStorage.getItem('atk_vxe_lua_script'), editedScript);
});

test('late polling responses do not mutate an unmounted Lua page', async () => {
  const pendingStatus = deferred();
  const pendingLogs = deferred();
  const harness = makeHarness({ invoke: command => {
    if (command === 'lua_status') return pendingStatus.promise;
      if (command === 'lua_logs_since') return pendingLogs.promise;
    throw new Error(`Unexpected command: ${command}`);
  } });

  harness.window.__mountLuaUI(harness.container);
  harness.window.__unmountLuaUI();
  pendingStatus.resolve({ running: true, state: 'running', message: null });
  pendingLogs.resolve({ cursor: '1', reset: false, entries: [{ id: 'stale', message: 'stale result' }] });
  await waitForAsyncWork();

  assert.notEqual(el(harness, 'badge').textContent, 'Executando');
  assert.doesNotMatch(el(harness, 'console').innerHTML, /stale result/);
  assert.ok([...harness.intervals.values()].every(interval => interval.cleared));
});

test('named scripts preserve empty sources, create checkpoints, restore versions, and isolate presets', async () => {
  const harness = makeHarness({
    useLibrary: true,
    initialStorage: { atk_vxe_lua_script: 'legacy source' },
    invoke: async command => {
      if (command === 'lua_status') return { running: false, state: 'stopped', message: null };
      if (command === 'lua_logs_since') return { cursor: '0', reset: false, entries: [] };
      throw new Error('Unexpected command: ' + command);
    }
  });

  harness.window.__mountLuaUI(harness.container);
  await waitForAsyncWork();
  const editor = el(harness, 'code');
  const original = activeLibraryScript(harness);
  assert.equal(original.source, 'legacy source');

  editor.value = 'edited legacy source';
  await editor.fire('input');
  await el(harness, 'new-script-btn').fire('click');
  const nameInput = el(harness, 'script-name-input');
  nameInput.value = 'Blank';
  await el(harness, 'script-name-form').fire('submit', { preventDefault() {} });

  assert.equal(editor.value, '', 'creating a script keeps an intentionally empty source');
  assert.equal(activeLibraryScript(harness).source, '');
  const stateAfterCreate = JSON.parse(harness.localStorage.getItem('atk_vxe_lua_script_library_v1'));
  const preservedOriginal = stateAfterCreate.scripts.find(script => script.id === original.id);
  assert.equal(preservedOriginal.source, 'edited legacy source');

  editor.value = 'first version';
  await editor.fire('input');
  await el(harness, 'checkpoint-script-btn').fire('click');
  let active = activeLibraryScript(harness);
  const firstVersion = active.versions.find(version => version.source === 'first version');
  assert.ok(firstVersion);

  editor.value = 'second version';
  await editor.fire('input');
  el(harness, 'version-select').value = firstVersion.id;
  await el(harness, 'restore-version-btn').fire('click');
  assert.equal(editor.value, 'first version');
  active = activeLibraryScript(harness);
  assert.ok(active.versions.some(version => version.source === 'second version'));

  await el(harness, 'rename-script-btn').fire('click');
  nameInput.value = 'Rascunho renomeado';
  await el(harness, 'script-name-form').fire('submit', { preventDefault() {} });
  assert.equal(activeLibraryScript(harness).name, 'Rascunho renomeado');

  const originalSourceBeforePreset = preservedOriginal.source;
  const presetSelect = el(harness, 'presets-select');
  presetSelect.value = 'template';
  await presetSelect.fire('change');
  assert.match(activeLibraryScript(harness).name, /^Preset ATK/);
  const stateAfterPreset = JSON.parse(harness.localStorage.getItem('atk_vxe_lua_script_library_v1'));
  assert.equal(stateAfterPreset.scripts.find(script => script.id === original.id).source, originalSourceBeforePreset);
  assert.equal(editor.value, activeLibraryScript(harness).source);
});

test('a storage failure stays visible and leaves the edited source in the editor', async () => {
  const harness = makeHarness({
    useLibrary: true,
    invoke: async command => {
      if (command === 'lua_status') return { running: false, state: 'stopped', message: null };
      if (command === 'lua_logs_since') return { cursor: '0', reset: false, entries: [] };
      throw new Error('Unexpected command: ' + command);
    }
  });
  harness.window.__mountLuaUI(harness.container);
  await waitForAsyncWork();

  const editor = el(harness, 'code');
  const key = 'atk_vxe_lua_script_library_v1';
  const previouslySaved = activeLibraryScript(harness).source;
  harness.localStorage.failWriteKeys.add(key);
  editor.value = 'source that must stay visible';
  await editor.fire('input');

  assert.equal(editor.value, 'source that must stay visible');
  assert.match(el(harness, 'library-message').textContent, /Não foi possível salvar.*O texto atual foi mantido/i);
  assert.equal(activeLibraryScript(harness).source, previouslySaved);
});

test('a failed atomic preset open keeps the active script and editor aligned', async () => {
  const harness = makeHarness({
    useLibrary: true,
    invoke: async command => {
      if (command === 'lua_status') return { running: false, state: 'stopped', message: null };
      if (command === 'lua_logs_since') return { cursor: '0', reset: false, entries: [] };
      throw new Error('Unexpected command: ' + command);
    }
  });
  harness.window.__mountLuaUI(harness.container);
  await waitForAsyncWork();

  const editor = el(harness, 'code');
  editor.value = 'edited before preset';
  await editor.fire('input');
  const previousActive = activeLibraryScript(harness);
  harness.localStorage.failWriteKeys.add('atk_vxe_lua_script_library_v1');
  el(harness, 'presets-select').value = 'template';
  await el(harness, 'presets-select').fire('change');

  assert.equal(editor.value, 'edited before preset');
  assert.equal(activeLibraryScript(harness).id, previousActive.id);
  assert.equal(activeLibraryScript(harness).source, previousActive.source);
  assert.equal(el(harness, 'script-select').value, previousActive.id);
  assert.match(el(harness, 'library-message').textContent, /Não foi possível salvar.*texto atual foi mantido/i);
});

test('incremental console appends safe text once, filters in place, pauses scrolling, and resets after rotation', async () => {
  const calls = [];
  let page = {
    cursor: '1',
    reset: false,
    entries: [
      { id: 'same-1', message: 'repetida' },
      { id: 'same-2', message: 'repetida' }
    ]
  };
  const harness = makeHarness({ invoke: async command => {
    calls.push(command);
    if (command === 'lua_status') return { running: false, state: 'stopped', message: null };
    if (command === 'lua_logs_since') return page;
    throw new Error('Unexpected command: ' + command);
  } });
  harness.window.__mountLuaUI(harness.container);
  await waitForAsyncWork();

  const consoleBox = el(harness, 'console');
  assert.equal(consoleBox.children.length, 2, 'identical messages with different stable ids both remain visible');
  const firstNode = consoleBox.children[0];
  const secondNode = consoleBox.children[1];
  assert.equal(firstNode.textContent, 'repetida');

  page = {
    cursor: '2',
    reset: false,
    entries: [
      { id: 'same-2', message: 'repetida' },
      { id: 'html', message: '<img src=x onerror=alert(1)>' },
      { id: 'error', message: '[ERRO] falhou' },
      { id: 'warning', message: '[WARN] atenção' }
    ]
  };
  await [...harness.intervals.values()][0].callback();
  assert.equal(consoleBox.children.length, 5);
  assert.equal(consoleBox.children[0], firstNode, 'polling retains existing DOM nodes');
  assert.equal(consoleBox.children[1], secondNode, 'duplicate id does not add a second copy');
  assert.equal(consoleBox.children[2].textContent, '<img src=x onerror=alert(1)>');
  assert.equal(consoleBox.children[2].children.length, 0, 'log text is assigned as text instead of parsed markup');

  const filter = el(harness, 'log-filter');
  filter.value = 'error';
  await filter.fire('change');
  assert.equal(consoleBox.children[0].style.display, 'none');
  assert.equal(consoleBox.children[3].style.display, '');
  assert.equal(consoleBox.children[4].style.display, 'none');

  const pause = el(harness, 'log-pause');
  await pause.fire('click');
  consoleBox.scrollTop = 0;
  page = { cursor: '3', reset: false, entries: [{ id: 'later', message: 'nova' }] };
  await [...harness.intervals.values()][0].callback();
  assert.equal(consoleBox.scrollTop, 0, 'paused output leaves the reader at their current scroll position');
  assert.equal(pause.getAttribute('aria-pressed'), 'true');

  await pause.fire('click');
  consoleBox.scrollTop = 0;
  harness.selection.text = 'trecho selecionado';
  page = { cursor: '4', reset: false, entries: [{ id: 'selected', message: 'mais tarde' }] };
  await [...harness.intervals.values()][0].callback();
  assert.equal(consoleBox.scrollTop, 0, 'text selection also prevents automatic scrolling');

  harness.selection.text = '';
  page = {
    cursor: '5',
    reset: true,
    entries: [{ id: 'after-rotation', message: 'após rotação' }]
  };
  await [...harness.intervals.values()][0].callback();
  assert.equal(consoleBox.children.length, 1);
  assert.equal(consoleBox.children[0].textContent, 'após rotação');
  assert.equal(el(harness, 'msg-count').textContent, '1 mensagem');
  assert.equal(calls.filter(command => command === 'lua_logs_since').length, 5);
});

test('global stop shortcut displays the backend registration failure', async () => {
  const calls = [];
  const harness = makeHarness({
    invoke: async (command, ...args) => {
      calls.push({ command, args });
      if (command === 'lua_status') return { running: false, state: 'stopped', message: null };
      if (command === 'lua_logs_since') return { cursor: '0', reset: false, entries: [] };
      if (command === 'lua_stop_shortcut_set') {
        return { shortcut: 'Ctrl+Shift+F9', registered: false, error: 'Atalho já está em uso.' };
      }
      throw new Error('Unexpected command: ' + command);
    }
  });
  harness.window.__mountLuaUI(harness.container);
  await waitForAsyncWork();
  assert.equal(el(harness, 'stop-shortcut-input').value, 'Ctrl+Alt+F11');

  el(harness, 'stop-shortcut-input').value = 'Ctrl+Shift+F9';
  await el(harness, 'stop-shortcut-apply').fire('click');
  assert.deepEqual(plain(calls.find(call => call.command === 'lua_stop_shortcut_set')), {
    command: 'lua_stop_shortcut_set',
    args: [{ shortcut: 'Ctrl+Shift+F9' }]
  });
  assert.match(el(harness, 'stop-shortcut-status').textContent, /Falha ao registrar Ctrl\+Shift\+F9: Atalho já está em uso/);
});

test('a registered stop shortcut still shows backend warnings after get and set', async () => {
  const getWarning = 'A configuração salva era inválida; padrão aplicado.';
  const setWarning = 'O atalho foi registrado, mas a persistência falhou.';
  const harness = makeHarness({
    shortcutGetResult: { shortcut: 'Ctrl+Alt+F11', registered: true, error: getWarning },
    invoke: async command => {
      if (command === 'lua_status') return { running: false, state: 'stopped', message: null };
      if (command === 'lua_logs_since') return { cursor: '0', reset: false, entries: [] };
      if (command === 'lua_stop_shortcut_set') {
        return { shortcut: 'Ctrl+Shift+F9', registered: true, error: setWarning };
      }
      throw new Error('Unexpected command: ' + command);
    }
  });
  harness.window.__mountLuaUI(harness.container);
  await waitForAsyncWork();

  const status = el(harness, 'stop-shortcut-status');
  assert.match(status.textContent, /Ctrl\+Alt\+F11 registrado\. Aviso: A configuração salva era inválida/);
  assert.equal(status.style.color, '#ff7d00');

  el(harness, 'stop-shortcut-input').value = 'Ctrl+Shift+F9';
  await el(harness, 'stop-shortcut-apply').fire('click');
  assert.match(status.textContent, /Ctrl\+Shift\+F9 registrado\. Aviso: O atalho foi registrado, mas a persistência falhou/);
  assert.equal(status.style.color, '#ff7d00');
});

test('incremental console cache stays bounded when the backend cursor remains current', async () => {
  const entries = Array.from({ length: 1000 }, (_, index) => ({
    id: 'bounded-' + index,
    message: 'entry-' + index
  }));
  let page = { cursor: 'current', reset: false, entries };
  const harness = makeHarness({ invoke: async command => {
    if (command === 'lua_status') return { running: false, state: 'stopped', message: null };
    if (command === 'lua_logs_since') return page;
    throw new Error('Unexpected command: ' + command);
  } });
  harness.window.__mountLuaUI(harness.container);
  await waitForAsyncWork();

  const consoleBox = el(harness, 'console');
  assert.equal(consoleBox.children.length, 1000);
  assert.equal(el(harness, 'msg-count').textContent, '1000 mensagens');
  assert.equal(consoleBox.children[0].textContent, 'entry-0');
  const selectedOldestLine = consoleBox.children[0];
  harness.selection.text = 'entry-0';
  harness.selection.node = selectedOldestLine;
  page = {
    cursor: 'current',
    reset: false,
    entries: Array.from({ length: 5 }, (_, index) => ({
      id: 'bounded-' + (1000 + index),
      message: 'entry-' + (1000 + index)
    }))
  };
  await [...harness.intervals.values()][0].callback();
  assert.equal(consoleBox.children.length, 1000);
  assert.equal(el(harness, 'msg-count').textContent, '1000 mensagens');
  assert.equal(consoleBox.children[0], selectedOldestLine, 'pruning preserves a selected old line when possible');
  assert.equal(selectedOldestLine.parentNode, consoleBox);
  assert.equal(consoleBox.children.at(-1).textContent, 'entry-1004');
});

test('successful status polling does not clear a console polling error', async () => {
  let statusFails = false;
  let logsFail = true;
  const harness = makeHarness({ invoke: async command => {
    if (command === 'lua_status') {
      if (statusFails) throw new Error('status indisponível');
      return { running: false, state: 'stopped', message: null };
    }
    if (command === 'lua_logs_since') {
      if (logsFail) throw new Error('console indisponível');
      return { cursor: 'later', reset: false, entries: [] };
    }
    throw new Error('Unexpected command: ' + command);
  } });
  harness.window.__mountLuaUI(harness.container);
  await waitForAsyncWork();
  assert.match(el(harness, 'status-message').textContent, /console indisponível/);

  logsFail = false;
  statusFails = true;
  await [...harness.intervals.values()][0].callback();
  assert.match(el(harness, 'status-message').textContent, /status indisponível/);
});
