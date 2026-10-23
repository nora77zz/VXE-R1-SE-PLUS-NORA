const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const bridgePath = path.resolve(__dirname, '../../apps/desktop/web/device_memory.js');
const bundlePath = path.resolve(__dirname, '../../apps/desktop/web/static/index-DcEiQt_8.js');
const bridgeSource = fs.readFileSync(bridgePath, 'utf8');
const bundleSource = fs.readFileSync(bundlePath, 'utf8');

class FakeEvent {
  constructor(type) {
    this.type = type;
    this.defaultPrevented = false;
  }
}

class FakeCustomEvent extends FakeEvent {
  constructor(type, init = {}) {
    super(type);
    this.detail = init.detail;
  }
}

class FakeWindow {
  constructor() { this.listeners = new Map(); }

  addEventListener(type, listener, options = {}) {
    const listeners = this.listeners.get(type) || new Set();
    listeners.add({ listener, once: !!options.once });
    this.listeners.set(type, listeners);
  }

  removeEventListener(type, listener) {
    const listeners = this.listeners.get(type);
    if (!listeners) return;
    for (const entry of listeners) {
      if (entry.listener === listener) listeners.delete(entry);
    }
  }

  dispatchEvent(event) {
    const listeners = this.listeners.get(event.type);
    if (!listeners) return true;
    for (const entry of Array.from(listeners)) {
      entry.listener.call(this, event);
      if (entry.once) listeners.delete(entry);
    }
    return true;
  }
}

class FakeHid {
  constructor(chooser = async () => []) {
    this.chooser = chooser;
    this.chooserCalls = 0;
  }

  async getDevices() { return []; }

  async requestDevice(options) {
    this.chooserCalls += 1;
    return this.chooser(options);
  }
}

function receiver(deviceKey = 'remembered-key') {
  return {
    deviceKey,
    interface: { vendorId: 0x373b, productId: 0x1085, usagePage: 0xff02, usage: 2 },
    productName: 'ATK receiver',
    serialNumber: null,
    collections: [
      { usagePage: 0xff02, usage: 2, inputReports: [{ reportId: 8 }], outputReports: [{ reportId: 8 }], featureReports: [] },
      { usagePage: 0xff04, usage: 2, inputReports: [], outputReports: [], featureReports: [{ reportId: 6 }] },
    ],
  };
}

function connectedState(overrides = {}) {
  const selected = receiver();
  return {
    candidates: [selected],
    selected,
    remembered: true,
    connected: true,
    opened: false,
    ambiguous: false,
    error: null,
    ...overrides,
  };
}

function deviceInfo(cid = 2, mid = 32, dataValidLen = 2) {
  return {
    queries: [{
      commandId: 0x10,
      dataValidLen,
      knownValues: { cidRaw: cid, midRaw: mid },
    }],
  };
}

function makeHarness({ state = connectedState(), readInfo = async () => deviceInfo(), chooser } = {}) {
  const calls = [];
  const listeners = new Map();
  const window = new FakeWindow();
  const hid = new FakeHid(chooser);
  window.CustomEvent = FakeCustomEvent;
  window.navigator = { hid };
  window.__TAURI__ = {
    core: {
      async invoke(command, payload) {
        calls.push({ command, payload });
        if (command === 'device_memory_state') return state;
        if (command === 'device_info') return readInfo();
        if (command === 'device_memory_remember') return state;
        if (command === 'device_memory_open' || command === 'device_memory_close') return state;
        if (command === 'device_memory_send_report') return undefined;
        throw new Error(`unexpected command: ${command}`);
      },
    },
    event: {
      async listen(name, callback) {
        listeners.set(name, callback);
        return () => listeners.delete(name);
      },
    },
  };
  const context = {
    window,
    navigator: window.navigator,
    Event: FakeEvent,
    CustomEvent: FakeCustomEvent,
    DOMException,
    ArrayBuffer,
    Uint8Array,
    DataView,
    Map,
    Set,
    WeakSet,
    Promise,
    Number,
    Object,
    Date,
    console,
  };
  vm.runInNewContext(bridgeSource, context, { filename: bridgePath });
  return { window, hid, listeners, calls, getState: () => state, setState: (next) => { state = next; } };
}

function waitForIdentity(window) {
  return new Promise((resolve) => {
    window.addEventListener('atk-device-memory-identity-ready', (event) => resolve(event.detail), { once: true });
  });
}

function validContext(proof) {
  return {
    deviceKey: proof.deviceKey,
    remembered: true,
    connected: true,
    ambiguous: false,
    controllerInitialized: true,
    isOnline: true,
    controllerCid: proof.cid,
    controllerMid: proof.mid,
    routeName: 'Home',
    routeEndpointPath: undefined,
    currentApi: null,
    currentDevice: null,
    currentHidDevice: null,
  };
}

async function nextTurn() {
  await new Promise((resolve) => setTimeout(resolve, 0));
}

async function waitFor(predicate) {
  for (let attempt = 0; attempt < 40; attempt += 1) {
    if (predicate()) return true;
    await nextTurn();
  }
  return predicate();
}

function extractBundleRestoreHooks() {
  const startMarker = 'let atkRestoreProof=null,atkRestoreRunning=!1,atkRestoreFinished=!1;';
  const endMarker = 'function H(e){const t=sZt.getDeviceHid(e.data);t&&Zsn(t,l)}';
  const start = bundleSource.indexOf(startMarker);
  const end = bundleSource.indexOf(endMarker, start);
  assert.ok(start >= 0 && end > start, 'active bundle restore hook markers must exist in order');
  const setterStartMarker = 'setDevice(e){const t=this.getDeviceStore().setDeviceByDeviceHid(e);';
  const setterEndMarker = 'removeDevice(e)';
  const setterStart = bundleSource.indexOf(setterStartMarker);
  const setterEnd = bundleSource.indexOf(setterEndMarker, setterStart);
  assert.ok(setterStart >= 0 && setterEnd > setterStart, 'active bundle native manager setter markers must exist');
  return {
    hooks: bundleSource.slice(start, end),
    setter: bundleSource.slice(setterStart, setterEnd),
  };
}

async function makeBundleIntegrationHarness() {
  const harness = makeHarness();
  const identityReady = waitForIdentity(harness.window);
  harness.window.dispatchEvent(new FakeEvent('load'));
  const proof = await identityReady;
  const [nativeDevice] = await harness.hid.getDevices();
  let entry = { device: nativeDevice, endpointPath: 'remembered-endpoint' };
  let currentApi = null;
  let currentHidDevice = null;
  let resolveInitialization;
  const initialization = new Promise((resolve) => { resolveInitialization = resolve; });
  let selectorCalls = 0;
  const record = { deviceInfo: { isOnline: true, CID: proof.cid, MID: proof.mid } };
  const router = {
    currentRoute: { value: { name: 'Home', query: {} } },
    async replace(location) {
      this.currentRoute.value = { name: location.name || 'Mouse', query: location.query || {} };
    },
  };
  const store = {
    getDevice: (endpointPath) => endpointPath === entry.endpointPath ? record : null,
    getCurrentDevice() {
      const endpointPath = router.currentRoute.value.query.endpointPath;
      return endpointPath ? this.getDevice(endpointPath) || { endpointPath } : null;
    },
  };
  const manager = {
    async scan() {},
    getDeviceListAll: () => [entry],
    getCurrentApi: () => currentApi,
    getCurDeviceHid: () => currentHidDevice,
    setCurrentApi: (value) => { currentApi = value; },
    setDeviceHid: (value) => { currentHidDevice = value; },
    getDeviceStore: () => ({ setDeviceByDeviceHid: () => initialization }),
  };
  const runtime = {
    window: harness.window,
    Event: FakeEvent,
    sZt: manager,
    sln: () => store,
    l: router,
    Zsn(target, targetRouter) {
      selectorCalls += 1;
      currentApi = target;
      currentHidDevice = target;
      return targetRouter.replace({ query: { endpointPath: target.endpointPath } });
    },
    console,
  };
  const extracted = extractBundleRestoreHooks();
  const source = `${extracted.hooks}\n`
    + 'globalThis.__atkRestoreHooks={identity:atkOnRestoreIdentity,controllerReady:atkOnRestoreControllerReady};\n'
    + `globalThis.__atkManagerSetDevice=({${extracted.setter}}).setDevice;`;
  vm.runInNewContext(source, runtime, { filename: bundlePath });
  manager.setDevice = runtime.__atkManagerSetDevice;
  harness.window.addEventListener('atk-device-memory-identity-ready', runtime.__atkRestoreHooks.identity);
  harness.window.addEventListener('atk-device-memory-controller-ready', runtime.__atkRestoreHooks.controllerReady);
  manager.setDevice(entry);
  harness.window.dispatchEvent(new FakeCustomEvent('atk-device-memory-identity-ready', { detail: proof }));

  return {
    ...harness,
    entry,
    router,
    resolveInitialization,
    getSelectorCalls: () => selectorCalls,
    setManualRoute() {
      currentApi = { manual: true };
      currentHidDevice = { manual: true };
      router.currentRoute.value = { name: 'AnotherMouse', query: { endpointPath: 'manual-endpoint' } };
    },
    getCurrentApi: () => currentApi,
    getCurrentHidDevice: () => currentHidDevice,
    async switchTransport(selected) {
      const next = connectedState({ candidates: [selected], selected });
      harness.setState(next);
      const [device] = await harness.hid.getDevices();
      entry = { device, endpointPath: selected.deviceKey + '-endpoint', __atkControllerInitialization: Promise.resolve(true) };
      // getDevices above refreshed state; include the previous selection in the
      // event sequence so the bridge observes a real routing transition.
      harness.listeners.get('device-memory-state')({ payload: connectedState({ selected: null, connected: false, candidates: [] }) });
      harness.listeners.get('device-memory-state')({ payload: next });
    },
  };
}

function wiredReceiver(key = 'wired-key') {
  const device = receiver(key);
  device.interface.vendorId = 0x3554;
  device.interface.productId = 0xf58f;
  return device;
}

test('live transport changes issue fresh identity proofs and invalidate old proofs in both directions', async () => {
  const harness = makeHarness();
  let ready = waitForIdentity(harness.window);
  harness.window.dispatchEvent(new FakeEvent('load'));
  let previous = await ready;
  for (const selected of [wiredReceiver(), receiver()]) {
    ready = waitForIdentity(harness.window);
    const state = connectedState({ candidates: [selected], selected });
    harness.setState(state);
    harness.listeners.get('device-memory-state')({ payload: state });
    const proof = await ready;
    assert.equal(proof.deviceKey, selected.deviceKey);
    assert.notEqual(proof.requestId, previous.requestId);
    assert.equal(await harness.window.__atkDeviceMemory.validateAutoRestore(previous), false);
    const [wrapper] = await harness.hid.getDevices();
    assert.equal(harness.window.__atkDeviceMemory.claimAutoRestore(proof, wrapper, validContext(proof), () => {}), true);
    previous = proof;
  }
  assert.deepEqual(harness.calls.filter((call) => call.command === 'device_info').map((call) => call.payload.deviceKey),
    ['remembered-key', 'wired-key', 'remembered-key']);
});

test('the active bundle selects the new transport after cable insertion and removal', async () => {
  const harness = await makeBundleIntegrationHarness();
  harness.resolveInitialization(true);
  assert.equal(await waitFor(() => harness.getSelectorCalls() === 1), true);
  await harness.switchTransport(wiredReceiver());
  assert.equal(await waitFor(() => harness.getSelectorCalls() === 2), true);
  assert.equal(harness.router.currentRoute.value.query.endpointPath, 'wired-key-endpoint');
  await harness.switchTransport(receiver());
  assert.equal(await waitFor(() => harness.getSelectorCalls() === 3), true);
  assert.equal(harness.router.currentRoute.value.query.endpointPath, 'remembered-key-endpoint');
});

test('one startup identity probe yields one proof and activates only once for the exact native receiver', async () => {
  const harness = makeHarness();
  const identityReady = waitForIdentity(harness.window);
  harness.window.dispatchEvent(new FakeEvent('load'));
  const proof = await identityReady;
  harness.window.dispatchEvent(new FakeEvent('load'));

  assert.equal(proof.deviceKey, 'remembered-key');
  assert.equal(proof.cid, 2);
  assert.equal(proof.mid, 32);
  assert.equal(harness.calls.filter((call) => call.command === 'device_info').length, 1);

  const [nativeDevice] = await harness.hid.getDevices();
  const state = await harness.window.__atkDeviceMemory.validateAutoRestore(proof);
  assert.equal(state.selected.deviceKey, proof.deviceKey);

  let activations = 0;
  assert.equal(harness.window.__atkDeviceMemory.claimAutoRestore(
    proof,
    nativeDevice,
    validContext(proof),
    () => { activations += 1; },
  ), true);
  assert.equal(harness.window.__atkDeviceMemory.claimAutoRestore(
    proof,
    nativeDevice,
    validContext(proof),
    () => { activations += 1; },
  ), false);
  assert.equal(activations, 1);
});

test('a manual chooser started while the identity read is pending cancels automatic restoration', async () => {
  let resolveInfo;
  const pendingInfo = new Promise((resolve) => { resolveInfo = resolve; });
  const harness = makeHarness({ readInfo: () => pendingInfo });
  let identityEvents = 0;
  harness.window.addEventListener('atk-device-memory-identity-ready', () => { identityEvents += 1; });
  harness.window.dispatchEvent(new FakeEvent('load'));

  for (let attempt = 0; attempt < 20 && !harness.calls.some((call) => call.command === 'device_info'); attempt += 1) {
    await nextTurn();
  }
  assert.equal(harness.calls.filter((call) => call.command === 'device_info').length, 1);
  assert.equal((await harness.hid.requestDevice({ filters: [] })).length, 0);
  resolveInfo(deviceInfo());
  await nextTurn();

  assert.equal(identityEvents, 0);
  assert.equal(harness.hid.chooserCalls, 1);
});

test('route, failed controller initialization, mismatched identity, and non-native wrappers cannot claim the restore', async () => {
  const harness = makeHarness();
  const identityReady = waitForIdentity(harness.window);
  harness.window.dispatchEvent(new FakeEvent('load'));
  const proof = await identityReady;
  const [nativeDevice] = await harness.hid.getDevices();
  await harness.window.__atkDeviceMemory.validateAutoRestore(proof);

  let activations = 0;
  const claim = (candidate, context) => harness.window.__atkDeviceMemory.claimAutoRestore(
    proof,
    candidate,
    context,
    () => { activations += 1; },
  );
  assert.equal(claim(nativeDevice, { ...validContext(proof), routeName: 'Settings' }), false);
  assert.equal(claim(nativeDevice, { ...validContext(proof), routeEndpointPath: 'another-device' }), false);
  assert.equal(claim(nativeDevice, { ...validContext(proof), controllerInitialized: false }), false);
  assert.equal(claim(nativeDevice, { ...validContext(proof), isOnline: false }), false);
  assert.equal(claim(nativeDevice, { ...validContext(proof), controllerCid: 1 }), false);
  assert.equal(claim({ ...nativeDevice }, validContext(proof)), false);
  assert.equal(claim(nativeDevice, validContext({ ...proof, deviceKey: 'other-key' })), false);
  assert.equal(activations, 0);
  assert.equal(claim(nativeDevice, validContext(proof)), true);
  assert.equal(activations, 1);
});

test('the active bundle handler waits for real store initialization, selects once, and leaves a manual route untouched', async (t) => {
  await t.test('startup on Home waits for controller init before calling the existing selector once', async () => {
    const harness = await makeBundleIntegrationHarness();
    await nextTurn();
    assert.equal(harness.getSelectorCalls(), 0);
    assert.equal(harness.router.currentRoute.value.name, 'Home');

    harness.resolveInitialization(true);
    assert.equal(await waitFor(() => harness.getSelectorCalls() === 1), true);
    await nextTurn();

    assert.equal(harness.getSelectorCalls(), 1);
    assert.equal(harness.router.currentRoute.value.name, 'Mouse');
    assert.equal(harness.router.currentRoute.value.query.endpointPath, harness.entry.endpointPath);
    assert.equal(harness.getCurrentApi(), harness.entry);
    assert.equal(harness.getCurrentHidDevice(), harness.entry);
  });

  await t.test('manual navigation during controller init wins and is not replaced', async () => {
    const harness = await makeBundleIntegrationHarness();
    await nextTurn();
    harness.setManualRoute();
    harness.resolveInitialization(true);
    assert.equal(await waitFor(() => harness.calls.filter((call) => call.command === 'device_memory_state').length >= 4), true);
    await nextTurn();

    assert.equal(harness.getSelectorCalls(), 0);
    assert.equal(harness.router.currentRoute.value.name, 'AnotherMouse');
    assert.equal(harness.router.currentRoute.value.query.endpointPath, 'manual-endpoint');
    assert.deepEqual(harness.getCurrentApi(), { manual: true });
    assert.deepEqual(harness.getCurrentHidDevice(), { manual: true });
  });
});

test('ambiguous, disconnected, or unremembered state is rejected before the native identity read', async () => {
  const cases = [
    connectedState({ ambiguous: true }),
    connectedState({ connected: false }),
    connectedState({ remembered: false }),
    connectedState({ selected: null }),
  ];

  for (const state of cases) {
    const harness = makeHarness({ state });
    harness.window.dispatchEvent(new FakeEvent('load'));
    await nextTurn();
    assert.equal(harness.calls.some((call) => call.command === 'device_info'), false);
  }
});

test('missing or failed 0x10 identity data does not emit a proof or retry', async (t) => {
  const cases = [
    ['short identity payload', async () => deviceInfo(2, 32, 1)],
    ['missing data length', async () => ({ queries: [{ commandId: 0x10, knownValues: { cidRaw: 2, midRaw: 32 } }] })],
    ['non-integer data length', async () => ({ queries: [{ commandId: 0x10, dataValidLen: Number.NaN, knownValues: { cidRaw: 2, midRaw: 32 } }] })],
    ['oversized data length', async () => deviceInfo(2, 32, 11)],
    ['missing identity command', async () => ({ queries: [] })],
    ['failed native query', async () => { throw new Error('native GET failed'); }],
  ];

  for (const [name, readInfo] of cases) {
    await t.test(name, async () => {
      const harness = makeHarness({ readInfo });
      let identityEvents = 0;
      harness.window.addEventListener('atk-device-memory-identity-ready', () => { identityEvents += 1; });
      harness.window.dispatchEvent(new FakeEvent('load'));
      harness.window.dispatchEvent(new FakeEvent('load'));
      await nextTurn();
      assert.equal(identityEvents, 0);
      assert.equal(harness.calls.filter((call) => call.command === 'device_info').length, 1);
    });
  }
});
