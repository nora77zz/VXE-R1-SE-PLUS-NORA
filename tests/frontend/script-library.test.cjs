const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const libraryPath = path.resolve(__dirname, '../../apps/desktop/web/lua_script_library.js');
const librarySource = fs.readFileSync(libraryPath, 'utf8');
const STORAGE_KEY = 'atk_vxe_lua_script_library_v1';
const LEGACY_KEY = 'atk_vxe_lua_script';
const RECOVERY_KEY = STORAGE_KEY + '.recovery';
const BACKUP_KEY = STORAGE_KEY + '.corrupt-backup';

class FakeStorage {
  constructor(initial = {}) {
    this.values = new Map(Object.entries(initial));
    this.failNextWrite = false;
    this.partialFailure = false;
  }

  getItem(key) {
    return this.values.has(key) ? this.values.get(key) : null;
  }

  setItem(key, value) {
    const text = String(value);
    if (this.failNextWrite) {
      this.failNextWrite = false;
      if (this.partialFailure) {
        this.partialFailure = false;
        this.values.set(key, text);
      }
      throw new Error('QuotaExceededError');
    }
    this.values.set(key, text);
  }

  removeItem(key) {
    this.values.delete(key);
  }
}

function makeHarness({ initialStorage = {}, now = 1_800_000_000_000 } = {}) {
  let clock = now;
  class FakeDate extends Date {
    static now() { return clock; }
  }
  const warnings = [];
  const storage = new FakeStorage(initialStorage);
  const window = {
    console: { warn(message) { warnings.push(String(message)); } }
  };
  vm.runInNewContext(librarySource, { window, Date: FakeDate }, { filename: libraryPath });
  return {
    storage,
    warnings,
    create(defaultSource = '-- default') {
      return window.__atkLuaLibrary.create(storage, defaultSource);
    },
    advance(milliseconds) { clock += milliseconds; },
    now() { return clock; }
  };
}

function plain(value) {
  if (Array.isArray(value)) return Array.from(value, plain);
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, plain(item)]));
  }
  return value;
}

test('migration preserves an empty legacy script and leaves the original key intact', () => {
  const harness = makeHarness({ initialStorage: { [LEGACY_KEY]: '' } });
  const library = harness.create('-- fallback');

  assert.equal(library.getActive().source, '');
  assert.equal(harness.storage.getItem(LEGACY_KEY), '');
  assert.ok(harness.storage.getItem(STORAGE_KEY));
  assert.deepEqual(plain(library.list()), [{ id: library.getActive().id, name: 'Meu primeiro script' }]);
});

test('migration preserves legacy whitespace and uses the default only when the key is absent', () => {
  const legacy = '  -- conteúdo antigo\n\n';
  const migrated = makeHarness({ initialStorage: { [LEGACY_KEY]: legacy } });
  assert.equal(migrated.create('-- default').getActive().source, legacy);
  assert.equal(migrated.storage.getItem(LEGACY_KEY), legacy);

  const fresh = makeHarness();
  assert.equal(fresh.create('-- default').getActive().source, '-- default');
});

test('named scripts can be created, selected, renamed, and reject duplicate names', () => {
  const harness = makeHarness();
  const library = harness.create();
  const first = library.getActive();
  const second = library.create('Macro lateral', '-- side');

  assert.equal(second.name, 'Macro lateral');
  assert.equal(second.source, '-- side');
  assert.deepEqual(plain(library.list()).map(item => item.name), ['Meu primeiro script', 'Macro lateral']);
  assert.throws(() => library.create(' macro LATERAL ', '-- duplicate'), /Já existe um script/);
  assert.equal(library.select(first.id).source, '-- default');
  assert.equal(library.rename('Template pessoal').name, 'Template pessoal');
  assert.throws(() => library.rename('Macro lateral'), /Já existe um script/);
  assert.throws(() => library.select('missing-script'), /não existe/);
});

test('openPreset atomically replaces or creates a named preset and keeps active sources on quota failure', () => {
  const harness = makeHarness();
  const library = harness.create('-- editor source');
  const editorScript = library.getActive();
  const presetScript = library.create('Recoil', '-- original preset source');
  library.select(editorScript.id);
  const storedBeforeReplacement = harness.storage.getItem(STORAGE_KEY);

  harness.storage.failNextWrite = true;
  assert.throws(() => library.openPreset(' recoil ', '-- replacement source'), /armazenamento local/);
  assert.equal(library.getActive().id, editorScript.id);
  assert.equal(library.getActive().source, '-- editor source');
  assert.equal(harness.storage.getItem(STORAGE_KEY), storedBeforeReplacement);
  assert.equal(JSON.parse(storedBeforeReplacement).scripts.find(script => script.id === presetScript.id).source, '-- original preset source');

  const replaced = library.openPreset('RECOIL', '-- replacement source');
  assert.deepEqual(plain(replaced), { id: presetScript.id, name: 'Recoil', source: '-- replacement source' });
  assert.equal(library.versions()[0].source, '-- original preset source');
  assert.equal(library.select(editorScript.id).source, '-- editor source');

  const activeBeforeCreateFailure = library.getActive();
  const storedBeforeCreateFailure = harness.storage.getItem(STORAGE_KEY);
  harness.storage.failNextWrite = true;
  assert.throws(() => library.openPreset('Novo', '-- new preset source'), /armazenamento local/);
  assert.equal(library.getActive().id, activeBeforeCreateFailure.id);
  assert.equal(harness.storage.getItem(STORAGE_KEY), storedBeforeCreateFailure);
  assert.equal(library.list().some(script => script.name === 'Novo'), false);

  const created = library.openPreset('Novo', '-- new preset source');
  assert.equal(created.name, 'Novo');
  assert.equal(created.source, '-- new preset source');
  assert.equal(library.list().length, 3);
});

test('autosave avoids per-edit history while checkpoints and restore preserve recoverable versions', () => {
  const harness = makeHarness();
  const library = harness.create('-- original');
  library.save('-- edit 1');
  library.save('-- edit 2');
  assert.equal(library.versions().length, 0);

  const beforePreset = library.checkpoint();
  assert.equal(beforePreset.source, '-- edit 2');
  const savedVersion = library.versions()[0];
  library.save('-- preset replacement');
  assert.equal(library.versions().length, 1);

  const restored = library.restore(savedVersion.id);
  assert.equal(restored.source, '-- edit 2');
  assert.equal(library.versions()[0].source, '-- preset replacement');
});

test('version history follows insertion order when timestamps tie across base36 counter rollover', () => {
  const now = 1_800_000_000_000;
  const olderId = 'v-clock-z';
  const rolloverId = 'v-clock-10';
  const backwardsClockId = 'v-clock-11';
  const payload = {
    schemaVersion: 1,
    activeId: 's-clock-0',
    scripts: [{
      id: 's-clock-0',
      name: 'Histórico',
      source: '-- atual',
      createdAt: now,
      updatedAt: now,
      versions: [
        { id: olderId, savedAt: now, source: '-- antes da virada' },
        { id: rolloverId, savedAt: now, source: '-- depois da virada' },
        { id: backwardsClockId, savedAt: now - 1, source: '-- relógio recuou' }
      ]
    }]
  };
  const harness = makeHarness({ initialStorage: { [STORAGE_KEY]: JSON.stringify(payload) }, now });
  const versions = plain(harness.create('-- fallback').versions());

  assert.deepEqual(versions.map(version => version.id), [backwardsClockId, rolloverId, olderId]);
  assert.deepEqual(versions.map(version => version.source), ['-- relógio recuou', '-- depois da virada', '-- antes da virada']);
});

test('autosave creates occasional snapshots and the per-script history stays bounded', () => {
  const harness = makeHarness();
  const library = harness.create('-- original');
  harness.advance(3 * 60 * 1000);
  library.save('-- first timed edit');
  assert.equal(library.versions().length, 0);
  harness.advance(3 * 60 * 1000);
  library.save('-- second timed edit');
  assert.equal(library.versions()[0].source, '-- first timed edit');
  library.save('-- next key');
  library.save('-- another key');
  assert.equal(library.versions().length, 1);

  for (let index = 0; index < 12; index += 1) {
    library.checkpoint();
    library.save('-- edit ' + index);
  }
  assert.equal(library.versions().length, 8);
});

test('global source limit preserves explicit checkpoint snapshots or cancels the whole operation', () => {
  const maxSource = 'x'.repeat(128 * 1024);
  const fullHarness = makeHarness();
  const fullLibrary = fullHarness.create('');
  const originalId = fullLibrary.getActive().id;
  for (let index = 0; index < 4; index += 1) fullLibrary.create('Full ' + index, maxSource);
  const fullPayload = fullHarness.storage.getItem(STORAGE_KEY);
  const activeBefore = fullLibrary.getActive();

  assert.throws(() => fullLibrary.checkpoint(), /checkpoint necessário/);
  assert.throws(() => fullLibrary.save('-- preset', { checkpoint: true }), /checkpoint necessário/);
  assert.throws(() => fullLibrary.select(originalId), /checkpoint necessário/);
  assert.equal(fullLibrary.getActive().id, activeBefore.id);
  assert.equal(fullLibrary.getActive().source, activeBefore.source);
  assert.equal(fullLibrary.versions().length, 0);
  assert.equal(fullHarness.storage.getItem(STORAGE_KEY), fullPayload);

  const restoreHarness = makeHarness();
  const restoreLibrary = restoreHarness.create('-- empty');
  const scriptId = restoreLibrary.getActive().id;
  const targetSource = 'a'.repeat(128 * 1024);
  const currentSource = 'b'.repeat(128 * 1024);
  restoreLibrary.save(currentSource);
  restoreLibrary.create('Other 1', maxSource);
  restoreLibrary.create('Other 2', maxSource);
  restoreLibrary.select(scriptId);
  restoreLibrary.save(targetSource);
  restoreLibrary.checkpoint();
  assert.ok(restoreLibrary.versions().some(version => version.source === targetSource));
  restoreLibrary.save(currentSource);
  const targetVersion = restoreLibrary.versions().find(version => version.source === targetSource);

  const restored = restoreLibrary.restore(targetVersion.id);
  assert.equal(restored.source, targetSource);
  assert.deepEqual(plain(restoreLibrary.versions().map(version => version.source)), [currentSource]);
});

test('corrupt primary storage is backed up without replacing it and valid state continues in recovery storage', () => {
  const damaged = '{not-json';
  const harness = makeHarness({ initialStorage: { [STORAGE_KEY]: damaged, [LEGACY_KEY]: '-- legacy recovery' } });
  const library = harness.create('-- fallback');

  assert.equal(library.getActive().source, '-- legacy recovery');
  assert.equal(harness.storage.getItem(STORAGE_KEY), damaged);
  assert.equal(harness.storage.getItem(BACKUP_KEY), damaged);
  assert.ok(harness.storage.getItem(RECOVERY_KEY));
  assert.ok(library.getWarnings().some(message => /corrompida/.test(message)));
  assert.ok(harness.warnings.some(message => /ATK HUB/.test(message)));

  library.create('Recuperado', '-- new script');
  const reloaded = harness.create('-- fallback');
  assert.equal(reloaded.getActive().name, 'Recuperado');
  assert.equal(harness.storage.getItem(STORAGE_KEY), damaged);
});

test('invalid imported schema is preserved as raw backup and a default library is created', () => {
  const damaged = JSON.stringify({ schemaVersion: 1, activeId: 'missing', scripts: [] });
  const harness = makeHarness({ initialStorage: { [STORAGE_KEY]: damaged } });
  const library = harness.create('-- safe default');

  assert.equal(library.getActive().source, '-- safe default');
  assert.equal(harness.storage.getItem(STORAGE_KEY), damaged);
  assert.equal(harness.storage.getItem(BACKUP_KEY), damaged);
  assert.ok(library.getWarnings().some(message => /corrompida/.test(message)));
});

test('a quota failure keeps both the prior in-memory script and stored payload', () => {
  const harness = makeHarness();
  const library = harness.create('-- saved');
  const storedBefore = harness.storage.getItem(STORAGE_KEY);
  harness.storage.failNextWrite = true;

  assert.throws(() => library.save('-- unsaved editor text'), /armazenamento local.*texto atual foi mantido/);
  assert.equal(library.getActive().source, '-- saved');
  assert.equal(harness.storage.getItem(STORAGE_KEY), storedBefore);
  assert.equal(library.save('-- retry').source, '-- retry');
});

test('a storage adapter that writes partially before failing is rolled back', () => {
  const harness = makeHarness();
  const library = harness.create('-- saved');
  const storedBefore = harness.storage.getItem(STORAGE_KEY);
  harness.storage.failNextWrite = true;
  harness.storage.partialFailure = true;

  assert.throws(() => library.save('-- partial write'), /QuotaExceededError/);
  assert.equal(harness.storage.getItem(STORAGE_KEY), storedBefore);
  assert.equal(library.getActive().source, '-- saved');
});

test('oversized source is rejected before changing memory or storage', () => {
  const harness = makeHarness();
  const library = harness.create('-- saved');
  const storedBefore = harness.storage.getItem(STORAGE_KEY);

  assert.throws(() => library.save('x'.repeat(128 * 1024 + 1)), /limite de 131072 caracteres/);
  assert.equal(library.getActive().source, '-- saved');
  assert.equal(harness.storage.getItem(STORAGE_KEY), storedBefore);
});
