const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const modulePath = path.resolve(__dirname, '../../apps/desktop/web/app_updates.js');
const moduleSource = fs.readFileSync(modulePath, 'utf8');

class FakeElement {
  constructor(tagName, ownerDocument) {
    this.tagName = tagName.toUpperCase();
    this.ownerDocument = ownerDocument;
    this.children = [];
    this.parentNode = null;
    this.attributes = new Map();
    this.listeners = new Map();
    this.hidden = false;
    this.disabled = false;
    this.value = '';
    this.textContent = '';
    this.className = '';
  }

  append(...nodes) {
    nodes.forEach((node) => this.appendChild(node));
  }

  appendChild(node) {
    if (node.parentNode) node.parentNode.removeChild(node);
    node.parentNode = this;
    this.children.push(node);
    return node;
  }

  removeChild(node) {
    const index = this.children.indexOf(node);
    if (index >= 0) this.children.splice(index, 1);
    node.parentNode = null;
    return node;
  }

  replaceChildren(...nodes) {
    this.children.forEach((node) => { node.parentNode = null; });
    this.children = [];
    this.append(...nodes);
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value));
  }

  getAttribute(name) {
    return this.attributes.has(name) ? this.attributes.get(name) : null;
  }

  removeAttribute(name) {
    this.attributes.delete(name);
  }

  addEventListener(name, callback) {
    const callbacks = this.listeners.get(name) || [];
    callbacks.push(callback);
    this.listeners.set(name, callbacks);
  }

  dispatch(name, event = {}) {
    for (const callback of this.listeners.get(name) || []) callback({ target: this, ...event });
  }

  click() {
    if (this.disabled || this.hidden) return;
    this.dispatch('click');
  }

  focus() {
    this.ownerDocument.activeElement = this;
  }

  get isConnected() {
    let node = this;
    while (node) {
      if (node === this.ownerDocument.body) return true;
      node = node.parentNode;
    }
    return false;
  }

  get previousSibling() {
    if (!this.parentNode) return null;
    const siblings = this.parentNode.children;
    return siblings[siblings.indexOf(this) - 1] || null;
  }

  querySelector(selector) {
    const match = selector.match(/^\[([^=\]]+)(?:="([^"]*)")?\]$/);
    if (!match) return null;
    const [, name, expected] = match;
    const wanted = expected === undefined ? null : expected;
    return findElement(this, (node) => {
      const actual = node.getAttribute(name);
      return wanted === null ? actual !== null : actual === wanted;
    });
  }
}

function findElement(root, predicate) {
  for (const child of root.children || []) {
    if (predicate(child)) return child;
    const nested = findElement(child, predicate);
    if (nested) return nested;
  }
  return null;
}

function pressKey(harness, key, shiftKey = false) {
  const event = {
    key,
    shiftKey,
    prevented: false,
    preventDefault() { this.prevented = true; },
  };
  for (const callback of harness.document.listeners.get('keydown') || []) callback(event);
  return event;
}

function makeHarness({ tauri = true, invokeHandlers = {}, readyState = 'complete' } = {}) {
  const document = {
    readyState,
    listeners: new Map(),
    activeElement: null,
    createElement(tag) { return new FakeElement(tag, this); },
    addEventListener(name, callback) {
      const callbacks = this.listeners.get(name) || [];
      callbacks.push(callback);
      this.listeners.set(name, callbacks);
    },
  };
  document.body = document.createElement('body');
  document.activeElement = document.body;

  const calls = [];
  const events = new Map();
  const timers = new Map();
  let nextTimerId = 1;
  const window = { document };
  window.setTimeout = (callback, delay) => {
    const id = nextTimerId++;
    timers.set(id, { callback, delay, interval: false });
    return id;
  };
  window.setInterval = (callback, delay) => {
    const id = nextTimerId++;
    timers.set(id, { callback, delay, interval: true });
    return id;
  };
  window.clearInterval = (id) => timers.delete(id);
  window.clearTimeout = (id) => timers.delete(id);

  if (tauri) {
    window.__TAURI__ = {
      core: {
        invoke(command, payload) {
          calls.push({ command, payload });
          const handler = invokeHandlers[command];
          if (!handler) throw new Error(`unexpected command: ${command}`);
          return typeof handler === 'function' ? handler(payload) : handler;
        },
      },
      event: {
        listen(name, callback) {
          const callbacks = events.get(name) || [];
          callbacks.push(callback);
          events.set(name, callbacks);
          return Promise.resolve(() => {
            const current = events.get(name) || [];
            events.set(name, current.filter((item) => item !== callback));
          });
        },
      },
    };
  }

  vm.runInNewContext(moduleSource, { window, Promise, Number, Set, Object, String, Math }, { filename: modulePath });
  return {
    calls,
    document,
    events,
    timers,
    async flush() {
      for (let i = 0; i < 12; i += 1) await Promise.resolve();
    },
    async fireTimer(interval = false) {
      const entry = [...timers.entries()].find(([, timer]) => timer.interval === interval);
      assert.ok(entry, `expected ${interval ? 'interval' : 'timeout'} to be scheduled`);
      const [id, timer] = entry;
      if (!timer.interval) timers.delete(id);
      timer.callback();
      await this.flush();
    },
    emit(name, payload) {
      for (const callback of events.get(name) || []) callback({ event: name, id: 1, payload });
    },
    element(name) {
      const selectors = {
        root: '[data-app-update-modal]',
        currentVersion: '[data-app-update-current-version]',
        availableVersion: '[data-app-update-available-version]',
        notes: '[data-app-update-notes]',
        state: '[data-app-update-state]',
        error: '[data-app-update-error]',
        install: '[data-app-update-action="install"]',
        later: '[data-app-update-action="later"]',
        close: '[data-app-update-action="close"]',
        check: '[data-app-update-action="check"]',
      };
      return document.body.querySelector(selectors[name]);
    },
  };
}

function available(version = '1.2.0', overrides = {}) {
  return {
    currentVersion: '1.0.0',
    state: 'available',
    availableVersion: version,
    notes: 'Melhorias e correções.',
    downloadedBytes: 0,
    totalBytes: null,
    ...overrides,
  };
}

test('automatic check waits for the user and installs only the exact offered version after a click', async () => {
  let finishInstall;
  const harness = makeHarness({
    invokeHandlers: {
      app_update_check: async () => available('1.2.0-beta+17'),
      app_update_install: () => new Promise((resolve) => { finishInstall = resolve; }),
    },
  });
  await harness.flush();
  assert.equal(harness.timers.size, 2);
  assert.equal(harness.calls.some((call) => call.command === 'app_update_check'), false);

  await harness.fireTimer(false);
  assert.equal(harness.calls.filter((call) => call.command === 'app_update_check').length, 1);
  assert.equal(harness.calls.some((call) => call.command === 'app_update_install'), false);
  assert.equal(harness.element('root').hidden, false);
  assert.equal(harness.element('currentVersion').textContent, '1.0.0');
  assert.equal(harness.element('availableVersion').textContent, '1.2.0-beta+17');

  harness.element('install').click();
  harness.element('install').click();
  await harness.flush();
  const installs = harness.calls.filter((call) => call.command === 'app_update_install');
  assert.equal(installs.length, 1);
  assert.equal(installs[0].payload.expectedVersion, '1.2.0-beta+17');
  assert.equal(harness.element('install').disabled, true);
  finishInstall({ ...available('1.2.0-beta+17'), state: 'installing' });
  await harness.flush();
  assert.equal(harness.element('later').disabled, true);
  assert.equal(harness.element('close').disabled, true);
});

test('release notes remain literal text and cannot create markup', async () => {
  const maliciousNotes = '<img src=x onerror="window.pwned=true"><script>attack()</script>';
  const harness = makeHarness({
    invokeHandlers: { app_update_check: async () => available('1.2.0', { notes: maliciousNotes }) },
  });
  await harness.flush();
  await harness.fireTimer(false);

  assert.equal(harness.element('notes').textContent, maliciousNotes);
  assert.equal(harness.document.body.querySelector('img'), null);
  assert.equal(harness.document.body.querySelector('script'), null);
  assert.equal(harness.document.body.querySelector('[data-app-update-modal]').children.length, 1);
});

test('manual tray opens a result and coalesces repeated requests while a check is active', async () => {
  let finishCheck;
  let checksStarted = 0;
  const harness = makeHarness({
    invokeHandlers: {
      app_update_status: async () => ({ currentVersion: '1.0.0', state: 'idle' }),
      app_update_check: () => {
        checksStarted += 1;
        if (checksStarted === 1) return new Promise((resolve) => { finishCheck = resolve; });
        return available('1.5.0');
      },
    },
  });
  await harness.flush();
  harness.emit('app-update-open');
  harness.emit('app-update-open');
  await harness.flush();

  assert.equal(harness.element('root').hidden, false);
  assert.equal(harness.calls.filter((call) => call.command === 'app_update_status').length, 1);
  assert.equal(harness.calls.filter((call) => call.command === 'app_update_check').length, 1);
  finishCheck(available('1.4.0'));
  await harness.flush();
  assert.equal(harness.element('availableVersion').textContent, '1.4.0');
  assert.equal(harness.element('install').disabled, false);

  await harness.fireTimer(true);
  assert.equal(harness.calls.filter((call) => call.command === 'app_update_status').length, 1);
  assert.equal(harness.calls.filter((call) => call.command === 'app_update_check').length, 2);
});

test('failed installation keeps its offer and permits one explicit retry with the same version', async () => {
  let attempt = 0;
  const harness = makeHarness({
    invokeHandlers: {
      app_update_check: async () => available('1.2.0'),
      app_update_install: async () => {
        attempt += 1;
        if (attempt === 1) throw 'Download falhou por falta de conexão.';
        return { ...available('1.2.0'), state: 'downloading', downloadedBytes: 7, totalBytes: 100 };
      },
    },
  });
  await harness.flush();
  await harness.fireTimer(false);

  harness.element('install').click();
  await harness.flush();
  assert.match(harness.element('error').textContent, /Download falhou/);
  assert.equal(harness.element('install').disabled, false);
  assert.equal(harness.element('install').textContent, 'Tentar instalar novamente');

  harness.element('install').click();
  await harness.flush();
  const installs = harness.calls.filter((call) => call.command === 'app_update_install');
  assert.equal(installs.length, 2);
  assert.deepEqual(installs.map((call) => call.payload.expectedVersion), ['1.2.0', '1.2.0']);
  assert.equal(harness.element('later').disabled, true);
});

test('deferring suppresses the same version during the session but permits a newer version', async () => {
  const updates = [available('1.2.0'), available('1.2.0'), available('1.3.0')];
  const harness = makeHarness({
    invokeHandlers: { app_update_check: async () => updates.shift() },
  });
  await harness.flush();
  await harness.fireTimer(false);
  assert.equal(harness.element('root').hidden, false);
  harness.element('later').click();
  assert.equal(harness.element('root').hidden, true);

  harness.emit('app-update-status', available('1.2.0'));
  assert.equal(harness.element('root').hidden, true);
  await harness.fireTimer(true);
  assert.equal(harness.element('root').hidden, true);

  await harness.fireTimer(true);
  assert.equal(harness.element('root').hidden, false);
  assert.equal(harness.element('availableVersion').textContent, '1.3.0');
});

test('Escape closes a safe dialog and returns focus to its opener but cannot dismiss an installation', async () => {
  const harness = makeHarness({
    invokeHandlers: {
      app_update_check: async () => available('1.2.0'),
      app_update_install: async () => ({ ...available('1.2.0'), state: 'installing' }),
    },
  });
  const opener = harness.document.createElement('button');
  harness.document.body.appendChild(opener);
  opener.focus();
  await harness.flush();
  await harness.fireTimer(false);

  const escape = { key: 'Escape', preventDefault() { this.prevented = true; } };
  for (const callback of harness.document.listeners.get('keydown') || []) callback(escape);
  assert.equal(escape.prevented, true);
  assert.equal(harness.element('root').hidden, true);
  assert.equal(harness.document.activeElement, opener);

  harness.emit('app-update-open');
  await harness.flush();
  harness.element('install').click();
  await harness.flush();
  const duringInstall = { key: 'Escape', preventDefault() { this.prevented = true; } };
  for (const callback of harness.document.listeners.get('keydown') || []) callback(duringInstall);
  assert.equal(duringInstall.prevented, undefined);
  assert.equal(harness.element('root').hidden, false);
});

test('Tab and Shift+Tab traverse the entire dialog in DOM order and wrap both boundaries', async () => {
  const failure = available('1.2.0', { state: 'error', error: 'Falha na verificação; tente novamente.' });
  const harness = makeHarness({
    invokeHandlers: {
      app_update_status: async () => failure,
      app_update_check: async () => failure,
    },
  });
  await harness.flush();
  harness.emit('app-update-open');
  await harness.flush();

  const close = harness.element('close');
  const notes = harness.element('notes');
  const check = harness.element('check');
  const later = harness.element('later');
  const install = harness.element('install');
  const forward = [close, notes, check, later, install];

  close.focus();
  for (let index = 1; index < forward.length; index += 1) {
    const event = pressKey(harness, 'Tab');
    assert.equal(event.prevented, true);
    assert.equal(harness.document.activeElement, forward[index]);
  }
  const forwardWrap = pressKey(harness, 'Tab');
  assert.equal(forwardWrap.prevented, true);
  assert.equal(harness.document.activeElement, close);

  const backward = [install, later, check, notes];
  close.focus();
  for (let index = 0; index < backward.length; index += 1) {
    const event = pressKey(harness, 'Tab', true);
    assert.equal(event.prevented, true);
    assert.equal(harness.document.activeElement, backward[index]);
  }
  const fromNotes = pressKey(harness, 'Tab', true);
  assert.equal(fromNotes.prevented, true);
  assert.equal(harness.document.activeElement, close);
  const backwardWrap = pressKey(harness, 'Tab', true);
  assert.equal(backwardWrap.prevented, true);
  assert.equal(harness.document.activeElement, install);
});

test('normal browser mode has no updater UI, listeners, timers, or native calls', async () => {
  const harness = makeHarness({ tauri: false });
  await harness.flush();
  assert.equal(harness.document.body.children.length, 0);
  assert.equal(harness.timers.size, 0);
  assert.equal(harness.events.size, 0);
  assert.deepEqual(harness.calls, []);
});
