const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const bridgePath = path.resolve(__dirname, '../../apps/desktop/web/device_memory.js');
const bridgeSource = fs.readFileSync(bridgePath, 'utf8');

function collection(usagePage, usage, input = [], output = [], feature = []) {
  return {
    usagePage,
    usage,
    inputReports: input.map((reportId) => ({ reportId })),
    outputReports: output.map((reportId) => ({ reportId })),
    featureReports: feature.map((reportId) => ({ reportId })),
  };
}

function candidate(deviceKey = 'opaque-device-key', serialNumber = 'serial-private') {
  return {
    deviceKey,
    interface: { vendorId: 0x373b, productId: 0x1085, usagePage: 0xff02, usage: 2 },
    productName: 'VXZR1 SE Plus Receiver',
    serialNumber,
    collections: [
      collection(0xff02, 2, [8], [8]),
      collection(0xff04, 2, [], [], [6]),
    ],
    descriptorSource: 'evidence/hid_descriptor_diagnostics.json',
  };
}

function makeBrowserDevice(serialNumber = 'serial-private') {
  return {
    vendorId: 0x373b,
    productId: 0x1085,
    productName: 'VXZR1 SE Plus Receiver',
    serialNumber,
    collections: [{ usagePage: 0xff04, usage: 2 }],
  };
}

class FakeEvent {
  constructor(type) {
    this.type = type;
    this.defaultPrevented = false;
  }
}

class FakeHid extends EventTarget {
  constructor(devices = [], chooser = async () => []) {
    super();
    this.devices = devices;
    this.chooser = chooser;
    this.chooserCalls = 0;
  }

  async getDevices() { return this.devices; }

  async requestDevice(options) {
    this.chooserCalls += 1;
    return this.chooser(options);
  }
}

function makeHarness({ state, devices = [], chooser, invokeOverrides = {}, promptChoice = null, withoutEventListener = false } = {}) {
  const listeners = new Map();
  const calls = [];
  let currentState = state || {
    candidates: [], selected: null, remembered: false, connected: false,
    opened: false, ambiguous: false, error: null,
  };
  const hid = new FakeHid(devices, chooser);
  const window = {
    navigator: { hid },
    prompt: promptChoice === null ? undefined : () => promptChoice,
  };
  const tauri = {
    core: {
      async invoke(command, payload) {
        calls.push({ command, payload });
        if (Object.hasOwn(invokeOverrides, command)) {
          return invokeOverrides[command](payload, () => currentState);
        }
        if (command === 'device_memory_state') return currentState;
        if (command === 'device_memory_remember') {
          const key = payload && payload.deviceKey;
          const serial = payload && payload.serialNumber;
          const selected = currentState.candidates.find((item) => item.deviceKey === key && (!serial || item.serialNumber === serial));
          if (!selected) throw new Error('selection did not resolve');
          currentState = { ...currentState, selected, remembered: true, connected: true, ambiguous: false };
          return currentState;
        }
        if (command === 'device_memory_open') {
          currentState = { ...currentState, opened: true };
          return currentState;
        }
        if (command === 'device_memory_close') {
          currentState = { ...currentState, opened: false };
          return currentState;
        }
        if (command === 'device_memory_send_report') return undefined;
        throw new Error(`unexpected command: ${command}`);
      },
    },
    event: withoutEventListener ? {} : {
      async listen(name, callback) {
        listeners.set(name, callback);
        return () => listeners.delete(name);
      },
    },
  };
  window.__TAURI__ = tauri;
  const context = {
    window,
    navigator: window.navigator,
    Event: FakeEvent,
    EventTarget,
    DOMException,
    ArrayBuffer,
    Uint8Array,
    DataView,
    Map,
    Set,
    Promise,
    Number,
    Object,
    console,
  };
  vm.runInNewContext(bridgeSource, context, { filename: bridgePath });
  return { hid, listeners, calls, window, getState: () => currentState };
}

test('saved connected receiver is restored by getDevices without invoking the browser chooser', async () => {
  const selected = candidate();
  const harness = makeHarness({
    state: {
      candidates: [selected], selected, remembered: true, connected: true,
      opened: false, ambiguous: false, error: null,
    },
    devices: [makeBrowserDevice()],
  });

  const devices = await harness.hid.getDevices();

  assert.equal(devices.length, 1);
  assert.equal(devices[0].__atkDeviceKey, selected.deviceKey);
  assert.equal(devices[0].opened, false);
  assert.equal(harness.hid.chooserCalls, 0);
  assert.equal(harness.calls.some((call) => call.command === 'device_memory_remember'), false);
});

test('actual WebHID chooser selection is persisted and mapped to the unique native endpoint', async () => {
  const selected = candidate();
  const chooserCalls = [];
  const harness = makeHarness({
    state: {
      candidates: [selected], selected: null, remembered: false, connected: false,
      opened: false, ambiguous: false, error: null,
    },
    devices: [makeBrowserDevice()],
    chooser: async (options) => {
      chooserCalls.push(options);
      return [makeBrowserDevice()];
    },
  });

  const result = await harness.hid.requestDevice({ filters: [{ vendorId: 0x373b, productId: 0x1085, usagePage: 0xff04, usage: 2 }] });

  assert.equal(chooserCalls.length, 1);
  assert.equal(result.length, 1);
  assert.equal(result[0].__atkDeviceKey, selected.deviceKey);
  const remember = harness.calls.find((call) => call.command === 'device_memory_remember');
  assert.equal(remember.payload.deviceKey, selected.deviceKey);
  assert.equal(remember.payload.serialNumber, selected.serialNumber);
});

test('ambiguous or disconnected saved selection never exposes an arbitrary target device', async () => {
  const first = candidate('first', null);
  const second = candidate('second', null);
  const harness = makeHarness({
    state: {
      candidates: [first, second], selected: null, remembered: true, connected: false,
      opened: false, ambiguous: true, error: null,
    },
    devices: [makeBrowserDevice(''), makeBrowserDevice('')],
  });

  assert.equal((await harness.hid.getDevices()).length, 0);
  assert.equal(harness.hid.chooserCalls, 0);
});

test('native report event reaches the original HID inputreport listeners with the physical bytes', async () => {
  const selected = candidate();
  const harness = makeHarness({
    state: {
      candidates: [selected], selected, remembered: true, connected: true,
      opened: false, ambiguous: false, error: null,
    },
  });
  const [device] = await harness.hid.getDevices();
  let observed;
  device.addEventListener('inputreport', (event) => { observed = event; });
  await device.open();
  const openCall = harness.calls.find((call) => call.command === 'device_memory_open');
  assert.equal(openCall.payload.deviceKey, selected.deviceKey);
  await device.sendReport(8, new Uint8Array(16));
  const sendCall = harness.calls.find((call) => call.command === 'device_memory_send_report');
  assert.equal(sendCall.payload.deviceKey, selected.deviceKey);
  assert.equal(sendCall.payload.data.length, 16);

  harness.listeners.get('device-memory-input-report')({ payload: { reportId: 8, data: Array.from({ length: 16 }, (_, index) => index + 1) } });

  assert.equal(device.opened, true);
  assert.equal(observed.reportId, 8);
  assert.equal(observed.device, device);
  assert.deepEqual(Array.from(new Uint8Array(observed.data.buffer)), Array.from({ length: 16 }, (_, index) => index + 1));
});

test('missing native HID event bridge rejects instead of exposing a connected device', async () => {
  const selected = candidate();
  const harness = makeHarness({
    state: {
      candidates: [selected], selected, remembered: true, connected: true,
      opened: false, ambiguous: false, error: null,
    },
    withoutEventListener: true,
  });

  await assert.rejects(harness.hid.getDevices(), /eventos HID nativos/);
});

test('fallback chooser asks once for explicit selection and does not display serial identifiers', async () => {
  const first = candidate('first', 'secret-one');
  const second = candidate('second', 'secret-two');
  let chooserText = '';
  const prompt = (message) => { chooserText = String(message); return '2'; };
  const harness = makeHarness({
    state: {
      candidates: [first, second], selected: null, remembered: false, connected: false,
      opened: false, ambiguous: false, error: null,
    },
    promptChoice: '2',
  });
  // Reinstall with WebHID absent so the explicit native fallback is selected.
  const noHidWindow = {
    navigator: {},
    prompt,
    __TAURI__: harness.window.__TAURI__,
  };
  const context = {
    window: noHidWindow,
    navigator: noHidWindow.navigator,
    Event: FakeEvent,
    EventTarget,
    DOMException,
    ArrayBuffer,
    Uint8Array,
    DataView,
    Map,
    Set,
    Promise,
    Number,
    Object,
    console,
  };
  vm.runInNewContext(bridgeSource, context, { filename: bridgePath });

  const result = await noHidWindow.navigator.hid.requestDevice({ filters: [{ vendorId: 0x373b }] });

  assert.equal(result.length, 1);
  assert.equal(result[0].__atkDeviceKey, 'second');
  assert.equal(chooserText.includes('secret-one'), false);
  assert.equal(chooserText.includes('secret-two'), false);
});
