const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const modulePath = path.resolve(__dirname, '../../apps/desktop/web/battery_live.js');
const moduleSource = fs.readFileSync(modulePath, 'utf8');

function deviceEntry(deviceKey, batteryInfo) {
  return {
    deviceHid: { device: { __atkDeviceKey: deviceKey } },
    deviceInfo: { batteryInfo },
  };
}

function nativeState(overrides = {}) {
  return {
    selected: { deviceKey: 'mouse-key' },
    identityResponded: true,
    batteryLevel: 35,
    charging: false,
    ...overrides,
  };
}

function makeHarness({ initialState = nativeState(), storeReady = true, devices } = {}) {
  const calls = [];
  const listeners = new Map();
  const documentListeners = new Map();
  const timers = [];
  const subscribers = [];
  const store = {
    devices: devices || [
      deviceEntry('mouse-key', { level: 35, isCharging: false, batteryIcon: 'v3-device-charge-20percent-charge', batteryTooltip: '35%' }),
      deviceEntry('other-key', { level: 90, isCharging: false, batteryIcon: 'v3-device-charge-80percent-charge', batteryTooltip: '90%' }),
    ],
    $subscribe(callback) { subscribers.push(callback); return () => {}; },
  };
  const pinia = { _s: new Map() };
  if (storeReady) pinia._s.set('devices', store);
  const appRoot = { __vue_app__: { config: { globalProperties: { $pinia: pinia } } } };
  const document = {
    readyState: 'complete',
    hidden: false,
    getElementById(id) { return id === 'app' ? appRoot : null; },
    addEventListener(name, callback) { documentListeners.set(name, callback); },
  };
  const window = {
    document,
    setTimeout(callback, delay) { timers.push({ callback, delay }); return timers.length; },
    __TAURI__: {
      core: {
        async invoke(command, payload) {
          calls.push({ command, payload });
          if (command === 'device_memory_state') return initialState;
          if (command === 'device_battery_watch') return undefined;
          throw new Error(`unexpected command: ${command}`);
        },
      },
      event: {
        async listen(name, callback) { listeners.set(name, callback); return () => {}; },
      },
    },
  };
  vm.runInNewContext(moduleSource, { window, Promise, Number, Object, Array }, { filename: modulePath });
  return {
    window, document, store, pinia, calls, timers, subscribers,
    async flush() { for (let i = 0; i < 10; i += 1) await Promise.resolve(); },
    emitState(state) { listeners.get('device-memory-state')({ payload: state }); },
    fireVisibility() { documentListeners.get('visibilitychange')(); },
    battery(key = 'mouse-key') {
      return store.devices.find((entry) => entry.deviceHid.device.__atkDeviceKey === key).deviceInfo.batteryInfo;
    },
  };
}

test('a native reading updates only the matching device with the original icon and tooltip', async () => {
  const harness = makeHarness();
  await harness.flush();

  harness.emitState(nativeState({ batteryLevel: 36, charging: true }));
  assert.equal(harness.battery().level, 36);
  assert.equal(harness.battery().isCharging, true);
  assert.equal(harness.battery().batteryIcon, 'v3-device-charge-charging');
  assert.equal(harness.battery().batteryTooltip, '36%');
  assert.equal(harness.battery('other-key').level, 90, 'another device is untouched');

  harness.emitState(nativeState({ batteryLevel: 81, charging: false }));
  assert.equal(harness.battery().batteryIcon, 'v3-device-charge-80percent-charge');
  harness.emitState(nativeState({ batteryLevel: 12, charging: false }));
  assert.equal(harness.battery().batteryIcon, 'v3-device-charge-lower-charge');
});

test('an unknown charging code keeps the previous charging flag and invalid levels are ignored', async () => {
  const harness = makeHarness();
  await harness.flush();
  harness.emitState(nativeState({ batteryLevel: 50, charging: true }));
  harness.emitState(nativeState({ batteryLevel: 51, charging: null }));
  assert.equal(harness.battery().level, 51);
  assert.equal(harness.battery().isCharging, true);

  for (const batteryLevel of [null, undefined, -1, 101, 4.5]) {
    harness.emitState(nativeState({ batteryLevel }));
  }
  harness.emitState({ selected: null, batteryLevel: 5 });
  assert.equal(harness.battery().level, 51);
});

test('the reading is applied once the UI store exists and wins over a stale UI rewrite', async () => {
  const harness = makeHarness({ storeReady: false, initialState: nativeState({ batteryLevel: 40, charging: true }) });
  await harness.flush();
  assert.equal(harness.battery().level, 35, 'nothing to update before the store exists');

  harness.pinia._s.set('devices', harness.store);
  const retry = harness.timers.find((timer) => timer.delay === 500);
  assert.ok(retry, 'the store lookup is retried');
  retry.callback();
  assert.equal(harness.battery().level, 40);
  assert.equal(harness.subscribers.length, 1);

  harness.store.devices[0].deviceInfo.batteryInfo = { level: 35, isCharging: false, batteryTooltip: '35%' };
  harness.subscribers[0]();
  assert.equal(harness.battery().level, 40);
  assert.equal(harness.battery().isCharging, true);
});

test('window visibility is reported so periodic reads run only while the window is visible', async () => {
  const harness = makeHarness();
  await harness.flush();
  const watch = () => harness.calls.filter((call) => call.command === 'device_battery_watch').map((call) => call.payload.visible);
  assert.deepEqual(watch(), [true]);

  harness.document.hidden = true;
  harness.fireVisibility();
  harness.document.hidden = false;
  harness.fireVisibility();
  await harness.flush();
  assert.deepEqual(watch(), [true, false, true]);
});
