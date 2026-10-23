// Restores the user's previously selected ATK receiver to the existing
// WebHID manager while keeping all native I/O on the serialized Tauri actor.
(function installDeviceMemoryBridge(global) {
  'use strict';

  const TARGET_VENDOR_ID = 0x373b;
  const TARGET_PRODUCT_ID = 0x1085;
  const TARGET_RECEIVER_COLLECTION = { usagePage: 0xff04, usage: 0x0002 };
  const TARGET_CONFIG_COLLECTION = { usagePage: 0xff02, usage: 0x0002 };
  const tauri = global.__TAURI__;
  const invoke = tauri && tauri.core && tauri.core.invoke;
  if (typeof invoke !== 'function') return;

  const navigatorObject = global.navigator;
  if (!navigatorObject) return;
  const nativeHid = navigatorObject.hid || null;
  const nativeGetDevices = nativeHid && typeof nativeHid.getDevices === 'function'
    ? nativeHid.getDevices.bind(nativeHid)
    : async () => [];
  const nativeRequestDevice = nativeHid && typeof nativeHid.requestDevice === 'function'
    ? nativeHid.requestDevice.bind(nativeHid)
    : null;
  const nativeDispatchEvent = nativeHid && typeof nativeHid.dispatchEvent === 'function'
    ? nativeHid.dispatchEvent.bind(nativeHid)
    : null;
  const wrappers = new Map();
  const localListeners = new Map();
  let eventTarget = nativeHid;
  let lastState = null;
  let stateRevision = 0;
  let autoRestoreStarted = false;
  let autoRestoreCancelled = false;
  let autoRestoreConsumed = false;
  let autoRestoreProof = null;
  let autoRestoreSequence = 0;
  let restoreGeneration = 0;
  let restoreEnabled = false;
  let refreshPromise = null;
  const autoRestoreProofs = new WeakSet();

  function invokeNative(command, payload) {
    return payload === undefined
      ? invoke.call(tauri.core, command)
      : invoke.call(tauri.core, command, payload);
  }

  function isTargetDevice(device) {
    return !!device && ((device.vendorId === TARGET_VENDOR_ID && device.productId === TARGET_PRODUCT_ID)
      || (device.vendorId === 0x3554 && device.productId === 0xf58f));
  }

  function descriptorHasTargetCollection(device, collection) {
    const collections = Array.isArray(device && device.collections) ? device.collections : [];
    return collections.some((item) => item.usagePage === collection.usagePage && item.usage === collection.usage);
  }

  function candidateHasExpectedCollections(candidate) {
    const collections = Array.isArray(candidate && candidate.collections) ? candidate.collections : [];
    return collections.some((item) => item.usagePage === TARGET_CONFIG_COLLECTION.usagePage && item.usage === TARGET_CONFIG_COLLECTION.usage)
      && collections.some((item) => item.usagePage === TARGET_RECEIVER_COLLECTION.usagePage && item.usage === TARGET_RECEIVER_COLLECTION.usage);
  }

  function eventWithDevice(type, device, fields = {}) {
    const event = new Event(type);
    Object.defineProperty(event, 'device', { configurable: true, enumerable: true, value: device });
    for (const [key, value] of Object.entries(fields)) {
      Object.defineProperty(event, key, { configurable: true, enumerable: true, value });
    }
    return event;
  }

  function createNativeDevice(candidate) {
    const key = candidate.deviceKey;
    let device = wrappers.get(key);
    if (device) {
      device.refresh(candidate);
      return device;
    }

    const callbacks = new Map();
    device = {
      vendorId: candidate.interface.vendorId,
      productId: candidate.interface.productId,
      productName: candidate.productName || 'ATK receiver',
      serialNumber: candidate.serialNumber || '',
      collections: candidate.collections || [],
      opened: false,
      __atkDeviceKey: key,
      __atkNativeDevice: true,
      refresh(next) {
        this.vendorId = next.interface.vendorId;
        this.productId = next.interface.productId;
        this.productName = next.productName || 'ATK receiver';
        this.serialNumber = next.serialNumber || '';
        this.collections = next.collections || [];
      },
      addEventListener(type, listener) {
        if (typeof listener !== 'function' && !(listener && typeof listener.handleEvent === 'function')) return;
        const listeners = callbacks.get(type) || new Set();
        listeners.add(listener);
        callbacks.set(type, listeners);
      },
      removeEventListener(type, listener) {
        const listeners = callbacks.get(type);
        if (listeners) listeners.delete(listener);
      },
      dispatchEvent(event) {
        const listeners = callbacks.get(event.type);
        if (listeners) {
          for (const listener of Array.from(listeners)) {
            if (typeof listener === 'function') listener.call(this, event);
            else listener.handleEvent(event);
          }
        }
        const propertyHandler = this[`on${event.type}`];
        if (typeof propertyHandler === 'function') propertyHandler.call(this, event);
        return !event.defaultPrevented;
      },
      async open() {
        await eventsReady;
        await invokeNative('device_memory_open', { deviceKey: this.__atkDeviceKey });
        this.opened = true;
      },
      async close() {
        await invokeNative('device_memory_close', { deviceKey: this.__atkDeviceKey });
        this.opened = false;
      },
      async sendReport(reportId, data) {
        const bytes = toUint8Array(data);
        await eventsReady;
        await invokeNative('device_memory_send_report', {
          deviceKey: this.__atkDeviceKey,
          reportId,
          data: Array.from(bytes),
        });
      },
      async sendFeatureReport() {
        throw new DOMException('O transporte nativo não expõe escrita de feature report.', 'NotSupportedError');
      },
      async receiveFeatureReport() {
        throw new DOMException('O transporte nativo não expõe leitura de feature report.', 'NotSupportedError');
      },
    };
    wrappers.set(key, device);
    return device;
  }

  function toUint8Array(data) {
    if (data instanceof ArrayBuffer) return new Uint8Array(data);
    if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    throw new TypeError('sendReport espera um ArrayBuffer ou uma view de bytes.');
  }

  function dispatchInputReport(payload) {
    if (!payload || payload.reportId !== 8 || !Array.isArray(payload.data)) return;
    const state = lastState;
    const selected = state && state.selected;
    if (!selected || !state.connected) return;
    if (payload.deviceKey !== selected.deviceKey) return;
    const device = createNativeDevice(selected);
    const bytes = Uint8Array.from(payload.data);
    const data = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    device.dispatchEvent(eventWithDevice('inputreport', device, {
      reportId: payload.reportId,
      data,
    }));
  }

  function dispatchLifecycle(payload) {
    if (!payload || (payload.eventType !== 'connect' && payload.eventType !== 'disconnect')) return;
    const device = createNativeDevice(payload.device);
    if (payload.eventType === 'disconnect') device.opened = false;
    const event = eventWithDevice(payload.eventType, device);
    if (nativeDispatchEvent) nativeDispatchEvent(event);
    else {
      const listeners = localListeners.get(payload.eventType);
      if (listeners) {
        for (const listener of Array.from(listeners)) {
          if (typeof listener === 'function') listener.call(eventTarget, event);
          else listener.handleEvent(event);
        }
      }
    }
  }

  const eventsReady = (async () => {
    const listen = tauri.event && tauri.event.listen;
    if (typeof listen !== 'function') {
      throw new Error('O transporte ATK não iniciou os eventos HID nativos.');
    }
    await Promise.all([
      listen.call(tauri.event, 'device-memory-input-report', (event) => dispatchInputReport(event && event.payload)),
      listen.call(tauri.event, 'device-memory-state', (event) => {
        stateRevision += 1;
        const oldKey = lastState && lastState.selected && lastState.selected.deviceKey;
        lastState = event && event.payload ? event.payload : lastState;
        const newKey = lastState && lastState.selected && lastState.selected.deviceKey;
        if (lastState && lastState.selected && lastState.connected) createNativeDevice(lastState.selected);
        if (oldKey !== newKey) {
          restoreGeneration += 1;
          autoRestoreProof = null;
          autoRestoreStarted = false;
          autoRestoreConsumed = false;
          autoRestoreCancelled = false;
          if (restoreEnabled && newKey) Promise.resolve().then(beginAutoRestore);
        }
      }),
      listen.call(tauri.event, 'device-memory-lifecycle', (event) => dispatchLifecycle(event && event.payload)),
    ]);
  })();
  eventsReady.catch(() => {});

  function refreshState() {
    if (refreshPromise) return refreshPromise;
    refreshPromise = eventsReady.then(async () => {
      const revision = stateRevision;
      const state = await invokeNative('device_memory_state');
      if (revision !== stateRevision) return lastState;
      lastState = state;
      if (state && state.selected && state.connected) createNativeDevice(state.selected);
      return state;
    }).finally(() => { refreshPromise = null; });
    return refreshPromise;
  }

  function isRememberedConnectedTarget(state) {
    if (!state) return false;
    const selected = state && state.selected;
    const deviceKey = selected && selected.deviceKey;
    const matchingCandidates = Array.isArray(state && state.candidates)
      ? state.candidates.filter((candidate) => candidate && candidate.deviceKey === deviceKey)
      : [];
    const nativeInterface = selected && selected.interface;
    return state.remembered === true
      && state.connected === true
      && state.ambiguous === false
      && typeof deviceKey === 'string'
      && deviceKey.length > 0
      && matchingCandidates.length === 1
      && nativeInterface
      && isTargetDevice(nativeInterface)
      && nativeInterface.usagePage === TARGET_CONFIG_COLLECTION.usagePage
      && nativeInterface.usage === TARGET_CONFIG_COLLECTION.usage
      && isNativeTarget(selected);
  }

  function readIdentity(info) {
    const query = info && Array.isArray(info.queries)
      ? info.queries.find((item) => item && item.commandId === 0x10)
      : null;
    const knownValues = query && query.knownValues;
    if (!query || !Number.isInteger(query.dataValidLen)
      || query.dataValidLen < 2 || query.dataValidLen > 10 || !knownValues) return null;
    const cid = knownValues.cidRaw;
    const mid = knownValues.midRaw;
    if (!Number.isInteger(cid) || cid < 0 || cid > 0xff
      || !Number.isInteger(mid) || mid < 0 || mid > 0xff) return null;
    return { cid, mid };
  }

  async function beginAutoRestore() {
    restoreEnabled = true;
    if (autoRestoreStarted || autoRestoreCancelled) return;
    autoRestoreStarted = true;
    const generation = restoreGeneration;
    try {
      let state = await refreshState();
      if (!isRememberedConnectedTarget(state)) return;
      const deviceKey = state.selected.deviceKey;

      // This is one bounded read-only identity probe. The native command returns
      // the existing four-query snapshot; only its real 0x10 CID/MID are used.
      const identity = readIdentity(await invokeNative('device_info', { deviceKey }));
      if (!identity || autoRestoreCancelled || generation !== restoreGeneration) return;

      state = await refreshState();
      if (autoRestoreCancelled || generation !== restoreGeneration || !isRememberedConnectedTarget(state)
        || state.selected.deviceKey !== deviceKey) return;

      const proof = Object.freeze({
        requestId: `connection-${Date.now()}-${++autoRestoreSequence}`,
        deviceKey,
        cid: identity.cid,
        mid: identity.mid,
      });
      autoRestoreProof = proof;
      autoRestoreProofs.add(proof);
      global.dispatchEvent(new global.CustomEvent('atk-device-memory-identity-ready', { detail: proof }));
    } catch (_) {
      // Keep the saved preference. A failed or sleeping-device probe must not
      // be mistaken for a connected, identified mouse or retried continuously.
    }
  }

  async function validateAutoRestore(proof) {
    if (proof !== autoRestoreProof || !autoRestoreProofs.has(proof)
      || autoRestoreCancelled || autoRestoreConsumed) return false;
    try {
      const state = await refreshState();
      return !autoRestoreCancelled
        && proof === autoRestoreProof
        && isRememberedConnectedTarget(state)
        && state.selected.deviceKey === proof.deviceKey
        ? state
        : null;
    } catch (_) {
      return null;
    }
  }

  function claimAutoRestore(proof, candidate, context, activate) {
    const state = lastState;
    if (proof !== autoRestoreProof || !autoRestoreProofs.has(proof)
      || autoRestoreCancelled || autoRestoreConsumed
      || !isRememberedConnectedTarget(state)
      || state.selected.deviceKey !== proof.deviceKey
      || !candidate || candidate.__atkNativeDevice !== true
      || candidate.__atkDeviceKey !== proof.deviceKey
      || wrappers.get(proof.deviceKey) !== candidate
      || !context || context.deviceKey !== proof.deviceKey
      || context.remembered !== true || context.connected !== true || context.ambiguous !== false
      || context.controllerInitialized !== true || context.isOnline !== true
      || context.controllerCid !== proof.cid || context.controllerMid !== proof.mid
      || context.routeName !== 'Home' || context.routeEndpointPath
      || context.currentApi != null || context.currentDevice != null || context.currentHidDevice != null
      || typeof activate !== 'function') return false;

    // Mark consumed before invoking app code so a route/event re-entry cannot
    // start a second activation.
    autoRestoreConsumed = true;
    try {
      activate();
    } catch (_) {
      return false;
    }
    return true;
  }

  function isNativeTarget(candidate) {
    return candidate && isTargetDevice(candidate.interface) && candidateHasExpectedCollections(candidate);
  }

  function filterMatches(candidate, filter) {
    if (filter.vendorId !== undefined && filter.vendorId !== candidate.interface.vendorId) return false;
    if (filter.productId !== undefined && filter.productId !== candidate.interface.productId) return false;
    if (filter.usagePage !== undefined || filter.usage !== undefined) {
      return candidate.collections.some((collection) =>
        (filter.usagePage === undefined || filter.usagePage === collection.usagePage)
        && (filter.usage === undefined || filter.usage === collection.usage));
    }
    return true;
  }

  function candidateMatchesOptions(candidate, options) {
    const filters = options && Array.isArray(options.filters) ? options.filters : [];
    const exclusions = options && Array.isArray(options.exclusionFilters) ? options.exclusionFilters : [];
    const included = filters.length === 0 || filters.some((filter) => filterMatches(candidate, filter));
    const excluded = exclusions.some((filter) => filterMatches(candidate, filter));
    return included && !excluded;
  }

  async function fallbackDevicePicker(candidates, options) {
    const matches = candidates.filter((candidate) => isNativeTarget(candidate) && candidateMatchesOptions(candidate, options));
    if (matches.length === 0) return [];
    if (matches.length === 1) return [matches[0]];
    const wired = matches.filter((candidate) => candidate.interface.vendorId === 0x3554 && candidate.interface.productId === 0xf58f);
    const receivers = matches.filter((candidate) => candidate.interface.vendorId === TARGET_VENDOR_ID && candidate.interface.productId === TARGET_PRODUCT_ID);
    if (wired.length === 1 && receivers.length === 1 && matches.length === 2) return wired;
    if (typeof global.prompt !== 'function') {
      throw new DOMException('Mais de um receptor ATK foi encontrado; a seleção precisa ser explícita.', 'NotFoundError');
    }
    const labels = matches.map((candidate, index) => `${index + 1}. ${candidate.productName || 'ATK receiver'}`).join('\n');
    const choice = global.prompt(`Selecione o receptor ATK:\n${labels}`, '1');
    if (choice === null) return [];
    const index = Number.parseInt(choice, 10) - 1;
    if (!Number.isInteger(index) || index < 0 || index >= matches.length) {
      throw new DOMException('A seleção do receptor ATK é inválida.', 'NotFoundError');
    }
    return [matches[index]];
  }

  async function requestDevice(options) {
    autoRestoreCancelled = true;
    const statePromise = invokeNative('device_memory_state');
    if (!nativeRequestDevice) {
      const state = await statePromise;
      const picked = await fallbackDevicePicker(state.candidates || [], options || {});
      if (picked.length === 0) return [];
      const candidate = picked[0];
      await invokeNative('device_memory_remember', {
        deviceKey: candidate.deviceKey,
        serialNumber: candidate.serialNumber || null,
      });
      const refreshed = await refreshState();
      return refreshed.selected ? [createNativeDevice(refreshed.selected)] : [];
    }

    // Call Chromium's chooser before awaiting Tauri, preserving the user
    // activation required by WebHID requestDevice().
    let browserPromise;
    try {
      browserPromise = nativeRequestDevice(options);
    } catch (error) {
      throw error;
    }
    const [state, browserDevices] = await Promise.all([statePromise, browserPromise]);
    const result = [];
    for (const browserDevice of browserDevices || []) {
      if (!isTargetDevice(browserDevice)) {
        result.push(browserDevice);
        continue;
      }
      if (!descriptorHasTargetCollection(browserDevice, TARGET_RECEIVER_COLLECTION)) {
        throw new DOMException('O receptor escolhido não corresponde à coleção WebHID do ATK.', 'NotFoundError');
      }
      const candidates = (state.candidates || []).filter((candidate) =>
        isNativeTarget(candidate)
        && candidate.interface.vendorId === browserDevice.vendorId
        && candidate.interface.productId === browserDevice.productId
        && (browserDevice.serialNumber
          ? candidate.serialNumber === browserDevice.serialNumber
          : true));
      let candidate;
      if (browserDevice.serialNumber) {
        if (candidates.length !== 1) {
          throw new DOMException('Não foi possível identificar unicamente o receptor escolhido.', 'NotFoundError');
        }
        [candidate] = candidates;
      } else if (candidates.length === 1) {
        [candidate] = candidates;
      } else {
        throw new DOMException('O receptor não expõe um número de série e há mais de uma opção conectada.', 'NotFoundError');
      }
      await invokeNative('device_memory_remember', {
        deviceKey: candidate.deviceKey,
        serialNumber: candidate.serialNumber || null,
      });
      const refreshed = await refreshState();
      if (!refreshed.selected || !refreshed.connected) {
        throw new DOMException('O receptor escolhido desconectou antes de abrir.', 'NotFoundError');
      }
      result.push(createNativeDevice(refreshed.selected));
    }
    return result;
  }

  async function getDevices() {
    await eventsReady;
    const [state, browserDevices] = await Promise.all([
      refreshState(),
      nativeGetDevices(),
    ]);
    const result = (browserDevices || []).filter((device) => !isTargetDevice(device));
    if (state && state.selected && state.connected && isNativeTarget(state.selected)) {
      result.push(createNativeDevice(state.selected));
    }
    return result;
  }

  const hid = nativeHid || {};
  Object.defineProperties(hid, {
    getDevices: { configurable: true, enumerable: true, value: getDevices, writable: true },
    requestDevice: { configurable: true, enumerable: true, value: requestDevice, writable: true },
  });
  if (nativeHid) {
    eventTarget = hid;
  } else {
    Object.defineProperties(hid, {
      addEventListener: {
        configurable: true,
        value(type, listener) {
          const listeners = localListeners.get(type) || new Set();
          listeners.add(listener);
          localListeners.set(type, listeners);
        },
      },
      removeEventListener: {
        configurable: true,
        value(type, listener) {
          const listeners = localListeners.get(type);
          if (listeners) listeners.delete(listener);
        },
      },
    });
    eventTarget = hid;
    try {
      Object.defineProperty(navigatorObject, 'hid', { configurable: true, enumerable: true, value: hid });
    } catch (_) {
      // Some WebViews expose a non-configurable unsupported HID property.
    }
  }

  global.__atkDeviceMemory = Object.freeze({ refreshState, validateAutoRestore, claimAutoRestore });
  if (typeof global.addEventListener === 'function') {
    if (global.document && global.document.readyState === 'loading') {
      global.addEventListener('DOMContentLoaded', beginAutoRestore, { once: true });
    } else if (global.document) {
      Promise.resolve().then(beginAutoRestore);
    } else {
      global.addEventListener('load', beginAutoRestore, { once: true });
    }
  }
})(typeof window !== 'undefined' ? window : globalThis);
