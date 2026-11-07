const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const modulePath = path.resolve(__dirname, '../../apps/desktop/web/device_diagnostics.js');
const moduleSource = fs.readFileSync(modulePath, 'utf8');

function candidate(deviceKey = 'selected-receiver') {
  return { deviceKey, productName: 'ATK receiver' };
}

function receiverState(overrides = {}) {
  const selected = candidate();
  return {
    candidates: [selected],
    selected,
    remembered: true,
    connected: true,
    opened: false,
    ambiguous: false,
    error: null,
    receiverEnumerated: true,
    identityResponded: false,
    diagnosticStatus: 'not_checked',
    lastCheckAtMs: null,
    communicationError: null,
    ...overrides,
  };
}

function makeHarness({ state, onInfo, beforeInfo } = {}) {
  const calls = [];
  const listeners = new Map();
  let currentState = state || {
    candidates: [], selected: null, connected: false, ambiguous: false,
    receiverEnumerated: false, identityResponded: false, diagnosticStatus: 'not_checked',
    lastCheckAtMs: null, communicationError: null,
  };
  const window = {
    __TAURI__: {
      core: {
        async invoke(command, payload) {
          calls.push({ command, payload });
          if (command === 'device_memory_state') return currentState;
          if (command === 'device_info') {
            if (beforeInfo) await beforeInfo(payload);
            if (onInfo) return onInfo(payload, () => currentState, (next) => { currentState = next; });
            return { queries: [] };
          }
          throw new Error(`unexpected command: ${command}`);
        },
      },
      event: {
        async listen(name, callback) {
          listeners.set(name, callback);
          return () => listeners.delete(name);
        },
      },
    },
  };
  const context = { window, Promise, Date, Number, Object };
  vm.runInNewContext(moduleSource, context, { filename: modulePath });
  return {
    api: window.__atkDeviceDiagnostics,
    calls,
    listeners,
    setState(next) { currentState = next; },
    getState() { return currentState; },
  };
}

test('diagnostics follow USB charging and receiver transport without keeping stale charging text', async () => {
  const harness = makeHarness({ state: receiverState({ transport: 'usb', identityResponded: true, charging: true }) });
  await harness.api.refreshState();
  assert.match(harness.api.view().title, /USB conectado · carregando/);
  assert.match(harness.api.view().message, /3554:F58F/);
  harness.listeners.get('device-memory-state')({ payload: receiverState({ transport: 'receiver', identityResponded: true, charging: false }) });
  assert.doesNotMatch(harness.api.view().title, /carregando/);
  assert.match(harness.api.view().message, /373B:1085/);
});

test('a state event during page load waits for the docked panel instead of creating a floating one', async () => {
  const listeners = new Map();
  const created = [];
  const docked = {
    id: 'atk-device-diagnostics',
    className: '',
    style: {},
    children: [],
    setAttribute() {},
    appendChild(child) { this.children.push(child); return child; },
    replaceChildren(...children) { this.children = children; },
    querySelector() { return null; },
  };
  let parsed = false;
  const document = {
    readyState: 'loading',
    body: { appendChild(node) { created.push(node); } },
    getElementById(id) { return parsed && id === 'atk-device-diagnostics' ? docked : null; },
    createElement(tag) {
      const node = { tag, style: {}, children: [], setAttribute() {}, appendChild(child) { this.children.push(child); return child; },
        replaceChildren(...children) { this.children = children; }, addEventListener() {}, querySelector() { return null; } };
      if (tag === 'aside') created.push(node);
      return node;
    },
    addEventListener(name, callback) { listeners.set(`document:${name}`, callback); },
  };
  const window = {
    document,
    __TAURI__: {
      core: { async invoke(command) { if (command === 'device_memory_state') return receiverState(); throw new Error(command); } },
      event: { async listen(name, callback) { listeners.set(name, callback); return () => {}; } },
    },
  };
  vm.runInNewContext(moduleSource, { window, Promise, Date, Number, Object }, { filename: modulePath });
  await Promise.resolve();

  listeners.get('device-memory-state')({ payload: receiverState() });
  assert.equal(created.length, 0, 'no floating fallback is created while the document is loading');

  parsed = true;
  document.readyState = 'interactive';
  listeners.get('document:DOMContentLoaded')();
  assert.equal(created.length, 0);
  assert.ok(docked.children.length > 0, 'the docked panel from index.html is used');
  assert.equal(docked.style.cssText, undefined, 'the docked panel keeps the responsive stylesheet layout');
});

test('explains when no target receiver is enumerated and does not start a probe', async () => {
  const harness = makeHarness();

  await harness.api.refreshState();
  const view = harness.api.view();
  const result = await harness.api.retry();

  assert.match(view.title, /Nenhum receptor ATK detectado/);
  assert.match(view.message, /Conecte o receptor USB/);
  assert.equal(view.canRetry, false);
  assert.equal(result, false);
  assert.equal(harness.calls.filter((call) => call.command === 'device_info').length, 0);
});

test('manual retry probes only the selected receiver and reflects identity confirmation', async () => {
  const state = receiverState();
  const harness = makeHarness({
    state,
    onInfo(payload, getState, setState) {
      assert.equal(payload.deviceKey, 'selected-receiver');
      setState({
        ...getState(),
        identityResponded: true,
        diagnosticStatus: 'responded',
        lastCheckAtMs: Date.now(),
      });
    },
  });

  const result = await harness.api.retry();

  assert.equal(result, true);
  assert.equal(harness.calls.filter((call) => call.command === 'device_info').length, 1);
  assert.equal(harness.calls.find((call) => call.command === 'device_info').payload.deviceKey, 'selected-receiver');
  assert.equal(harness.calls.some((call) => call.command === 'device_memory_send_report'), false);
  assert.match(harness.api.view().title, /mouse respondeu/);
  assert.match(harness.api.view().lastCheck, /Última verificação:/);
});

test('a rejected probe displays the backend communication error and remains unproven', async () => {
  const harness = makeHarness({
    state: receiverState(),
    onInfo(_payload, getState, setState) {
      setState({
        ...getState(),
        identityResponded: false,
        diagnosticStatus: 'failed',
        lastCheckAtMs: Date.now(),
        communicationError: 'A operação HID excedeu o prazo de resposta.',
      });
      throw new Error('A operação HID excedeu o prazo de resposta.');
    },
  });

  const result = await harness.api.retry();

  assert.equal(result, false);
  assert.equal(harness.api.view().diagnosticStatus, 'failed');
  assert.match(harness.api.view().title, /Falha de comunicação/);
  assert.match(harness.api.view().message, /excedeu o prazo/);
  assert.equal(harness.api.view().canRetry, true);
});

test('string rejection survives the final state refresh when no backend diagnostic error exists', async () => {
  const harness = makeHarness({
    state: receiverState(),
    onInfo() { throw 'A seleção do receptor mudou durante a solicitação.'; },
  });

  const result = await harness.api.retry();

  assert.equal(result, false);
  assert.equal(harness.api.view().diagnosticStatus, 'not_checked');
  assert.match(harness.api.view().title, /Não foi possível iniciar/);
  assert.equal(harness.api.view().message, 'A seleção do receptor mudou durante a solicitação.');
});

test('concurrent clicks share one in-flight manual probe', async () => {
  let markStarted;
  let releaseProbe;
  const started = new Promise((resolve) => { markStarted = resolve; });
  const gate = new Promise((resolve) => { releaseProbe = resolve; });
  const harness = makeHarness({
    state: receiverState(),
    async beforeInfo() {
      markStarted();
      await gate;
    },
    onInfo(_payload, getState, setState) {
      setState({ ...getState(), identityResponded: true, diagnosticStatus: 'responded' });
    },
  });

  const first = harness.api.retry();
  await started;
  const second = harness.api.retry();
  assert.equal(second, first);
  releaseProbe();
  assert.equal(await first, true);
  assert.equal(harness.calls.filter((call) => call.command === 'device_info').length, 1);
});

test('ambiguous selection cannot be probed', async () => {
  const selected = candidate('one');
  const harness = makeHarness({
    state: {
      ...receiverState({ candidates: [selected, candidate('two')], ambiguous: true }),
      selected,
      connected: false,
    },
  });

  const result = await harness.api.retry();

  assert.equal(result, false);
  assert.equal(harness.calls.filter((call) => call.command === 'device_info').length, 0);
});
