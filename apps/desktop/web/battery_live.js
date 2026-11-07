// Keeps the embedded UI's battery display in sync with native readings.
// The original UI reads the battery only when a device initializes; the native
// actor now reads it on every new connection and every second while this
// window is visible, and publishes it in device-memory-state.
(function installBatteryLive(global) {
  'use strict';

  const tauri = global.__TAURI__;
  const invoke = tauri && tauri.core && tauri.core.invoke;
  const document = global.document;
  if (typeof invoke !== 'function' || !document) return;

  const STORE_RETRY_MS = 500;
  let reading = null;
  let subscribedStore = null;
  let storeTimer = null;

  // Mirrors the original UI's presentation (jon.create in the bundle).
  function batteryIcon(level, charging) {
    if (charging) return 'v3-device-charge-charging';
    if (!level) return null;
    const ranges = [
      [80, 100, 'v3-device-charge-80percent-charge'],
      [60, 80, 'v3-device-charge-60percent-charge'],
      [40, 60, 'v3-device-charge-40percent-charge'],
      [20, 40, 'v3-device-charge-20percent-charge'],
      [0, 20, 'v3-device-charge-lower-charge'],
    ];
    const match = ranges.find(([min, max]) => level >= min && level <= max);
    return match ? match[2] : null;
  }

  function batteryInfo(level, charging, previous) {
    return {
      level,
      isCharging: charging,
      chargeRemainingTime: previous ? previous.chargeRemainingTime : undefined,
      batteryIcon: batteryIcon(level, charging),
      batteryTooltip: level ? `${level}%` : (previous && previous.batteryTooltip) || '',
    };
  }

  function devicesStore() {
    const root = document.getElementById('app');
    const app = root && root.__vue_app__;
    const pinia = app && app.config && app.config.globalProperties && app.config.globalProperties.$pinia;
    return pinia && pinia._s && typeof pinia._s.get === 'function' ? pinia._s.get('devices') || null : null;
  }

  function entryFor(store, deviceKey) {
    const devices = store && Array.isArray(store.devices) ? store.devices : [];
    return devices.find((entry) => entry && entry.deviceHid && entry.deviceHid.device
      && entry.deviceHid.device.__atkDeviceKey === deviceKey) || null;
  }

  function apply() {
    if (!reading) return false;
    const store = devicesStore();
    if (!store) return false;
    const entry = entryFor(store, reading.deviceKey);
    if (!entry || !entry.deviceInfo) return false;
    const current = entry.deviceInfo.batteryInfo || null;
    const charging = reading.charging === null ? !!(current && current.isCharging) : reading.charging;
    if (current && current.level === reading.level && current.isCharging === charging) return true;
    entry.deviceInfo.batteryInfo = batteryInfo(reading.level, charging, current);
    return true;
  }

  // The original UI rebuilds its device entry when a device (re)initializes;
  // reapply after its own mutations so a newer native reading always wins.
  function watchStore() {
    if (subscribedStore) return;
    const store = devicesStore();
    if (!store || typeof store.$subscribe !== 'function') {
      if (storeTimer === null) {
        storeTimer = global.setTimeout(() => { storeTimer = null; watchStore(); }, STORE_RETRY_MS);
      }
      return;
    }
    subscribedStore = store;
    store.$subscribe(() => { apply(); }, { detached: true });
    apply();
  }

  function onState(state) {
    if (!state || !state.selected || typeof state.selected.deviceKey !== 'string') return;
    const level = state.batteryLevel;
    if (!Number.isInteger(level) || level < 0 || level > 100) return;
    reading = {
      deviceKey: state.selected.deviceKey,
      level,
      charging: typeof state.charging === 'boolean' ? state.charging : null,
    };
    if (!apply()) watchStore();
  }

  function reportVisibility() {
    // Read visibility at the event, not when the promise runs.
    const visible = !document.hidden;
    Promise.resolve()
      .then(() => invoke.call(tauri.core, 'device_battery_watch', { visible }))
      .catch(() => {});
  }

  if (tauri.event && typeof tauri.event.listen === 'function') {
    Promise.resolve(tauri.event.listen('device-memory-state', (event) => onState(event && event.payload)))
      .catch(() => {});
  }
  document.addEventListener('visibilitychange', reportVisibility);

  function start() {
    reportVisibility();
    watchStore();
    Promise.resolve()
      .then(() => invoke.call(tauri.core, 'device_memory_state'))
      .then(onState)
      .catch(() => {});
  }

  global.__atkBatteryLive = Object.freeze({
    view: () => (reading ? Object.assign({}, reading) : null),
    batteryInfo,
  });

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start, { once: true });
  } else {
    Promise.resolve().then(start);
  }
})(typeof window !== 'undefined' ? window : globalThis);
