(function (global) {
  'use strict';

  const STORAGE_KEY = 'atk_vxe_lua_script_library_v1';
  const LEGACY_KEY = 'atk_vxe_lua_script';
  const RECOVERY_PREFIX = STORAGE_KEY + '.recovery';
  const CORRUPT_BACKUP_PREFIX = STORAGE_KEY + '.corrupt-backup';
  const MAX_SCRIPTS = 32;
  const MAX_NAME_LENGTH = 64;
  const MAX_SOURCE_LENGTH = 128 * 1024;
  const MAX_TOTAL_SOURCE_LENGTH = 512 * 1024;
  const MAX_VERSIONS_PER_SCRIPT = 8;
  const MAX_PAYLOAD_LENGTH = 1024 * 1024;
  const AUTO_CHECKPOINT_INTERVAL_MS = 5 * 60 * 1000;
  const MAX_RECOVERY_SLOTS = 8;
  const MAX_CORRUPT_BACKUPS = 8;

  function isRecord(value) {
    return value !== null && typeof value === 'object' && !Array.isArray(value);
  }

  function cloneState(state) {
    return JSON.parse(JSON.stringify(state));
  }

  function normalizeName(name) {
    if (typeof name !== 'string') throw new TypeError('O nome do script precisa ser texto.');
    const normalized = name.trim();
    if (!normalized) throw new TypeError('Informe um nome para o script Lua.');
    if (normalized.length > MAX_NAME_LENGTH) {
      throw new RangeError('O nome do script deve ter no máximo ' + MAX_NAME_LENGTH + ' caracteres.');
    }
    return normalized;
  }

  function validateSource(source) {
    if (typeof source !== 'string') throw new TypeError('O código Lua precisa ser texto.');
    if (source.length > MAX_SOURCE_LENGTH) {
      throw new RangeError('O código Lua excede o limite de ' + MAX_SOURCE_LENGTH + ' caracteres.');
    }
    return source;
  }

  function totalSourceLength(state) {
    let total = 0;
    for (const script of state.scripts) {
      total += script.source.length;
      for (const version of script.versions) total += version.source.length;
    }
    return total;
  }

  function validateState(state) {
    if (!isRecord(state) || state.schemaVersion !== 1 || !Array.isArray(state.scripts)) {
      throw new Error('A estrutura da biblioteca não corresponde ao formato v1.');
    }
    if (state.scripts.length < 1 || state.scripts.length > MAX_SCRIPTS) {
      throw new Error('A biblioteca precisa conter de 1 a ' + MAX_SCRIPTS + ' scripts.');
    }
    if (typeof state.activeId !== 'string') throw new Error('A biblioteca não contém um script ativo válido.');

    const ids = new Set();
    const names = new Set();
    let activeFound = false;
    for (const script of state.scripts) {
      if (!isRecord(script) || typeof script.id !== 'string' || !/^[a-zA-Z0-9_-]{1,100}$/.test(script.id)) {
        throw new Error('A biblioteca contém um identificador de script inválido.');
      }
      if (ids.has(script.id)) throw new Error('A biblioteca contém identificadores repetidos.');
      ids.add(script.id);
      if (typeof script.name !== 'string' || script.name.trim() !== script.name || !script.name || script.name.length > MAX_NAME_LENGTH) {
        throw new Error('A biblioteca contém um nome de script inválido.');
      }
      const normalizedName = script.name.toLowerCase();
      if (names.has(normalizedName)) throw new Error('A biblioteca contém nomes de script repetidos.');
      names.add(normalizedName);
      validateSource(script.source);
      if (!Number.isSafeInteger(script.createdAt) || script.createdAt < 0 || !Number.isSafeInteger(script.updatedAt) || script.updatedAt < 0) {
        throw new Error('A biblioteca contém datas de script inválidas.');
      }
      if (!Array.isArray(script.versions) || script.versions.length > MAX_VERSIONS_PER_SCRIPT) {
        throw new Error('A biblioteca contém um histórico de versões inválido.');
      }
      for (const version of script.versions) {
        if (!isRecord(version) || typeof version.id !== 'string' || !/^[a-zA-Z0-9_-]{1,100}$/.test(version.id)) {
          throw new Error('A biblioteca contém uma versão com identificador inválido.');
        }
        if (ids.has(version.id)) throw new Error('A biblioteca contém identificadores repetidos.');
        ids.add(version.id);
        if (!Number.isSafeInteger(version.savedAt) || version.savedAt < 0) {
          throw new Error('A biblioteca contém uma versão com data inválida.');
        }
        validateSource(version.source);
      }
      if (script.id === state.activeId) activeFound = true;
    }
    if (!activeFound) throw new Error('O script ativo não existe na biblioteca.');
    if (totalSourceLength(state) > MAX_TOTAL_SOURCE_LENGTH) {
      throw new Error('O conteúdo total da biblioteca excede o limite permitido.');
    }
    return state;
  }

  function parseState(raw) {
    if (typeof raw !== 'string' || raw.length > MAX_PAYLOAD_LENGTH) {
      throw new Error('O conteúdo salvo está ausente ou excede o limite permitido.');
    }
    let parsed;
    try {
      parsed = JSON.parse(raw);
    } catch (error) {
      throw new Error('O conteúdo salvo não é JSON válido.');
    }
    return validateState(parsed);
  }

  function createFactory() {
    function create(storage, defaultSource) {
      if (!storage || typeof storage.getItem !== 'function' || typeof storage.setItem !== 'function') {
        throw new TypeError('O armazenamento da biblioteca Lua precisa oferecer getItem e setItem.');
      }
      validateSource(defaultSource);

      let warnings = [];
      let nextId = 0;
      let state;
      let activeStorageKey = STORAGE_KEY;

      function warn(message) {
        if (!warnings.includes(message)) warnings.push(message);
        if (global.console && typeof global.console.warn === 'function') {
          global.console.warn('[ATK HUB] ' + message);
        }
      }

      function readStorage(key) {
        try {
          return storage.getItem(key);
        } catch (error) {
          throw new Error('Não foi possível ler a biblioteca Lua do armazenamento local: ' + errorMessage(error));
        }
      }

      function errorMessage(error) {
        return error && error.message ? String(error.message) : String(error);
      }

      function makeId(prefix, currentState) {
        const taken = new Set();
        if (currentState) {
          for (const script of currentState.scripts) {
            taken.add(script.id);
            for (const version of script.versions) taken.add(version.id);
          }
        }
        let candidate;
        do {
          candidate = prefix + '-' + Date.now().toString(36) + '-' + (nextId++).toString(36);
        } while (taken.has(candidate));
        return candidate;
      }

      function makeScript(name, source, currentState) {
        const now = Date.now();
        return {
          id: makeId('s', currentState),
          name: normalizeName(name),
          source: validateSource(source),
          createdAt: now,
          updatedAt: now,
          versions: []
        };
      }

      function findRecoverySlot() {
        let firstEmpty = null;
        for (let index = 0; index < MAX_RECOVERY_SLOTS; index += 1) {
          const key = index === 0 ? RECOVERY_PREFIX : RECOVERY_PREFIX + '.' + index;
          const raw = readStorage(key);
          if (raw === null) {
            if (!firstEmpty) firstEmpty = { key, raw: null };
            continue;
          }
          try {
            return { key, raw, state: parseState(raw) };
          } catch (error) {
            preserveCorruptPayload(key, raw);
            warn('Um arquivo de recuperação da biblioteca Lua estava corrompido. O conteúdo original foi preservado em backup.');
          }
        }
        if (firstEmpty) return firstEmpty;
        throw new Error('Não há espaço para uma nova recuperação da biblioteca Lua; os dados existentes foram preservados.');
      }

      function preserveCorruptPayload(sourceKey, raw) {
        for (let index = 0; index < MAX_CORRUPT_BACKUPS; index += 1) {
          const backupKey = index === 0
            ? CORRUPT_BACKUP_PREFIX
            : CORRUPT_BACKUP_PREFIX + '.' + index;
          const existing = readStorage(backupKey);
          if (existing === raw) return backupKey;
          if (existing !== null) continue;
          try {
            storage.setItem(backupKey, raw);
            if (storage.getItem(backupKey) !== raw) {
              throw new Error('o conteúdo de backup não pôde ser confirmado');
            }
            return backupKey;
          } catch (error) {
            throw new Error('Não foi possível preservar o conteúdo corrompido da biblioteca Lua (' + sourceKey + ') em backup. O original foi mantido. ' + errorMessage(error));
          }
        }
        throw new Error('Os backups disponíveis para a biblioteca Lua estão ocupados; o conteúdo original foi mantido em ' + sourceKey + '.');
      }

      function persist(key, nextState) {
        validateState(nextState);
        const raw = JSON.stringify(nextState);
        if (raw.length > MAX_PAYLOAD_LENGTH) {
          throw new RangeError('A biblioteca Lua excede o limite de armazenamento. Reduza scripts ou versões antes de salvar.');
        }
        let previous;
        try {
          previous = storage.getItem(key);
          storage.setItem(key, raw);
          if (storage.getItem(key) !== raw) throw new Error('a gravação não foi confirmada');
        } catch (error) {
          // localStorage.setItem is atomic in browsers. This best-effort rollback also
          // protects small storage adapters that throw after partially writing.
          try {
            const after = storage.getItem(key);
            if (after !== previous) {
              if (previous === null && typeof storage.removeItem === 'function') storage.removeItem(key);
              else if (previous !== null) storage.setItem(key, previous);
            }
          } catch (rollbackError) {
            // Keep the original persistence failure visible; the in-memory state is
            // still the previous committed object.
          }
          throw new Error('Não foi possível salvar a biblioteca Lua no armazenamento local (quota ou permissão). O texto atual foi mantido no editor. ' + errorMessage(error));
        }
        state = nextState;
      }

      function initialState(source) {
        const script = makeScript('Meu primeiro script', source, null);
        return { schemaVersion: 1, activeId: script.id, scripts: [script] };
      }

      function initialize() {
        const primaryRaw = readStorage(STORAGE_KEY);
        if (primaryRaw !== null) {
          try {
            state = parseState(primaryRaw);
            return;
          } catch (error) {
            preserveCorruptPayload(STORAGE_KEY, primaryRaw);
            warn('A biblioteca Lua salva estava corrompida. O conteúdo original foi preservado em backup; os scripts foram recuperados de uma cópia válida ou do script legado.');
            const recovery = findRecoverySlot();
            if (recovery.state) {
              activeStorageKey = recovery.key;
              state = recovery.state;
              return;
            }
            const legacy = readStorage(LEGACY_KEY);
            const source = legacy === null ? defaultSource : legacy;
            state = initialState(source);
            activeStorageKey = recovery.key;
            persist(activeStorageKey, state);
            return;
          }
        }

        const recovery = findRecoverySlot();
        if (recovery.state) {
          activeStorageKey = recovery.key;
          state = recovery.state;
          warn('A biblioteca Lua foi carregada de uma cópia de recuperação.');
          return;
        }

        const legacy = readStorage(LEGACY_KEY);
        state = initialState(legacy === null ? defaultSource : legacy);
        activeStorageKey = STORAGE_KEY;
        persist(activeStorageKey, state);
      }

      function getActiveScript(targetState) {
        const active = targetState.scripts.find(script => script.id === targetState.activeId);
        if (!active) throw new Error('O script Lua ativo não foi encontrado.');
        return active;
      }

      function activeRecord(script) {
        return { id: script.id, name: script.name, source: script.source };
      }

      function ensureUniqueName(name, currentState, exceptId) {
        const normalized = name.toLowerCase();
        if (currentState.scripts.some(script => script.id !== exceptId && script.name.toLowerCase() === normalized)) {
          throw new Error('Já existe um script com esse nome. Escolha outro nome.');
        }
      }

      function appendVersion(script, currentState, timestamp) {
        const last = script.versions[script.versions.length - 1];
        if (last && last.source === script.source) return last.id;
        const version = {
          id: makeId('v', currentState),
          savedAt: timestamp === undefined ? Date.now() : timestamp,
          source: script.source
        };
        script.versions.push(version);
        while (script.versions.length > MAX_VERSIONS_PER_SCRIPT) script.versions.shift();
        return version.id;
      }

      function oldestVersion(stateCandidate, protectedVersionIds) {
        let selected = null;
        for (const script of stateCandidate.scripts) {
          for (const version of script.versions) {
            if (protectedVersionIds && protectedVersionIds.has(version.id)) continue;
            if (!selected || version.savedAt < selected.version.savedAt) selected = { script, version };
          }
        }
        return selected;
      }

      function fitBounds(stateCandidate, protectedVersionIds) {
        let removedVersions = 0;
        while (totalSourceLength(stateCandidate) > MAX_TOTAL_SOURCE_LENGTH || JSON.stringify(stateCandidate).length > MAX_PAYLOAD_LENGTH) {
          const oldest = oldestVersion(stateCandidate, protectedVersionIds);
          if (!oldest) {
            const message = protectedVersionIds && protectedVersionIds.size > 0
              ? 'O checkpoint necessário não cabe no limite de armazenamento da biblioteca Lua. A operação foi cancelada e os dados atuais foram mantidos.'
              : 'O conteúdo da biblioteca Lua excede o limite seguro. A operação foi cancelada e os dados atuais foram mantidos.';
            throw new RangeError(message);
          }
          oldest.script.versions = oldest.script.versions.filter(version => version.id !== oldest.version.id);
          removedVersions += 1;
        }
        return removedVersions;
      }

      function commitCandidate(candidate, protectedVersionIds) {
        const pruned = fitBounds(candidate, protectedVersionIds);
        persist(activeStorageKey, candidate);
        if (pruned > 0) warn('O histórico mais antigo da biblioteca Lua foi reduzido para respeitar o limite de armazenamento.');
        return candidate;
      }

      initialize();

      return {
        list() {
          return state.scripts.map(script => ({ id: script.id, name: script.name }));
        },
        getActive() {
          return activeRecord(getActiveScript(state));
        },
        getWarnings() {
          return warnings.slice();
        },
        select(id) {
          if (typeof id !== 'string') throw new TypeError('O identificador do script precisa ser texto.');
          const target = state.scripts.find(script => script.id === id);
          if (!target) throw new Error('O script Lua selecionado não existe.');
          if (id === state.activeId) return activeRecord(target);
          const candidate = cloneState(state);
          const previous = getActiveScript(candidate);
          const protectedVersionId = appendVersion(previous, candidate, Date.now());
          candidate.activeId = id;
          commitCandidate(candidate, new Set([protectedVersionId]));
          return activeRecord(getActiveScript(state));
        },
        create(name, source) {
          const normalized = normalizeName(name);
          validateSource(source);
          if (state.scripts.length >= MAX_SCRIPTS) throw new RangeError('A biblioteca atingiu o limite de ' + MAX_SCRIPTS + ' scripts.');
          ensureUniqueName(normalized, state, null);
          const candidate = cloneState(state);
          const script = makeScript(normalized, source, candidate);
          candidate.scripts.push(script);
          candidate.activeId = script.id;
          commitCandidate(candidate);
          return activeRecord(getActiveScript(state));
        },
        openPreset(name, source) {
          const normalized = normalizeName(name);
          validateSource(source);
          const existing = state.scripts.find(script => script.name.toLowerCase() === normalized.toLowerCase());
          const candidate = cloneState(state);
          const protectedVersionIds = new Set();
          let target;
          const now = Date.now();

          if (existing) {
            target = candidate.scripts.find(script => script.id === existing.id);
            if (target.source !== source) {
              protectedVersionIds.add(appendVersion(target, candidate, now));
              target.source = source;
              target.updatedAt = now;
            }
          } else {
            if (candidate.scripts.length >= MAX_SCRIPTS) {
              throw new RangeError('A biblioteca atingiu o limite de ' + MAX_SCRIPTS + ' scripts.');
            }
            target = makeScript(normalized, source, candidate);
            candidate.scripts.push(target);
          }

          candidate.activeId = target.id;
          commitCandidate(candidate, protectedVersionIds);
          return activeRecord(getActiveScript(state));
        },
        rename(name) {
          const normalized = normalizeName(name);
          const candidate = cloneState(state);
          const active = getActiveScript(candidate);
          ensureUniqueName(normalized, candidate, active.id);
          active.name = normalized;
          commitCandidate(candidate);
          return activeRecord(getActiveScript(state));
        },
        save(source, options) {
          validateSource(source);
          const settings = options === undefined ? {} : options;
          if (!isRecord(settings)) throw new TypeError('As opções de salvamento precisam ser um objeto.');
          if (settings.checkpoint !== undefined && typeof settings.checkpoint !== 'boolean') {
            throw new TypeError('A opção checkpoint precisa ser verdadeira ou falsa.');
          }
          const candidate = cloneState(state);
          const active = getActiveScript(candidate);
          if (active.source === source) return activeRecord(getActiveScript(state));
          const now = Date.now();
          const protectedVersionIds = new Set();
          if (settings.checkpoint === true) {
            protectedVersionIds.add(appendVersion(active, candidate, now));
          } else {
            const lastVersion = active.versions[active.versions.length - 1];
            const baseline = lastVersion ? lastVersion.savedAt : active.createdAt;
            if (now - baseline >= AUTO_CHECKPOINT_INTERVAL_MS) appendVersion(active, candidate, now);
          }
          active.source = source;
          active.updatedAt = now;
          commitCandidate(candidate, protectedVersionIds);
          return activeRecord(getActiveScript(state));
        },
        checkpoint() {
          const candidate = cloneState(state);
          const active = getActiveScript(candidate);
          const latest = active.versions[active.versions.length - 1];
          if (latest && latest.source === active.source) return activeRecord(getActiveScript(state));
          const protectedVersionId = appendVersion(active, candidate, Date.now());
          commitCandidate(candidate, new Set([protectedVersionId]));
          return activeRecord(getActiveScript(state));
        },
        versions() {
          return getActiveScript(state).versions
            .slice()
            .reverse()
            .map(version => ({ id: version.id, savedAt: version.savedAt, source: version.source }));
        },
        restore(versionId) {
          if (typeof versionId !== 'string') throw new TypeError('O identificador da versão precisa ser texto.');
          const candidate = cloneState(state);
          const active = getActiveScript(candidate);
          const version = active.versions.find(item => item.id === versionId);
          if (!version) throw new Error('A versão selecionada não existe para este script.');
          const protectedVersionId = appendVersion(active, candidate, Date.now());
          active.source = version.source;
          active.updatedAt = Date.now();
          commitCandidate(candidate, new Set([protectedVersionId]));
          return activeRecord(getActiveScript(state));
        }
      };
    }

    return { create };
  }

  global.__atkLuaLibrary = createFactory();
})(typeof window !== 'undefined' ? window : globalThis);
