// Consent-based update UI for the native ATK HUB desktop app.
(function installAppUpdates(global) {
  'use strict';

  const tauri = global.__TAURI__;
  const invoke = tauri && tauri.core && tauri.core.invoke;
  const document = global.document;
  if (typeof invoke !== 'function' || !document) return;

  const INITIAL_CHECK_DELAY_MS = 8000;
  const CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000;
  const ALLOWED_STATES = new Set([
    'idle', 'checking', 'upToDate', 'available', 'downloading', 'installing', 'error',
  ]);
  const promptVersions = new Set();

  let status = normalizeStatus({ state: 'idle' });
  let offerVersion = null;
  let deferredVersion = null;
  let dismissedWhileChecking = false;
  let modalOpen = false;
  let checkPromise = null;
  let manualCheckPending = false;
  let installPromise = null;
  let installLocked = false;
  let installAttemptId = 0;
  let installErrorObserved = false;
  let previousFocus = null;
  let queuedTrayOpen = false;
  let initialTimer = null;
  let periodicTimer = null;
  let elements = null;

  function errorText(error, fallback) {
    if (typeof error === 'string' && error.trim()) return error.trim();
    if (error && typeof error.message === 'string' && error.message.trim()) return error.message.trim();
    return fallback;
  }

  function boundedString(value, limit) {
    return typeof value === 'string' ? value.slice(0, limit) : '';
  }

  function normalizeStatus(value) {
    const source = value && typeof value === 'object' ? value : {};
    const state = ALLOWED_STATES.has(source.state) ? source.state : 'error';
    return {
      currentVersion: boundedString(source.currentVersion, 120),
      state,
      availableVersion: typeof source.availableVersion === 'string' ? source.availableVersion : '',
      notes: boundedString(source.notes, 6000),
      downloadedBytes: Number.isFinite(source.downloadedBytes) && source.downloadedBytes >= 0
        ? source.downloadedBytes : 0,
      totalBytes: Number.isFinite(source.totalBytes) && source.totalBytes > 0
        ? source.totalBytes : null,
      error: boundedString(source.error, 2400),
    };
  }

  function isNonEmptyVersion(version) {
    return typeof version === 'string' && version.trim().length > 0;
  }

  function callNative(command, payload) {
    try {
      return Promise.resolve(payload === undefined
        ? invoke.call(tauri.core, command)
        : invoke.call(tauri.core, command, payload));
    } catch (error) {
      return Promise.reject(error);
    }
  }

  function makeElement(tag, className, text, attributes) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    if (attributes) {
      Object.keys(attributes).forEach((name) => node.setAttribute(name, attributes[name]));
    }
    return node;
  }

  function setHidden(node, hidden) {
    node.hidden = !!hidden;
    node.setAttribute('aria-hidden', String(!!hidden));
  }

  function createModal() {
    if (elements || !document.body) return elements;

    const root = makeElement('div', 'atk-updates-backdrop');
    root.setAttribute('data-app-update-modal', '');
    setHidden(root, true);

    const dialog = makeElement('section', 'atk-updates-dialog', undefined, {
      role: 'dialog',
      'aria-modal': 'true',
      'aria-labelledby': 'atk-updates-title',
      'aria-describedby': 'atk-updates-summary',
      tabindex: '-1',
    });

    const header = makeElement('header', 'atk-updates-header');
    const heading = makeElement('h2', 'atk-updates-title', 'Atualização do ATK HUB', { id: 'atk-updates-title' });
    const closeButton = makeElement('button', 'atk-updates-close', '×', {
      type: 'button',
      'aria-label': 'Depois',
      'data-app-update-action': 'close',
    });
    header.append(heading, closeButton);

    const content = makeElement('div', 'atk-updates-content');
    const summary = makeElement('p', 'atk-updates-summary', '');
    summary.id = 'atk-updates-summary';

    const versions = makeElement('dl', 'atk-updates-versions');
    const currentTerm = makeElement('dt', '', 'Versão atual');
    const currentValue = makeElement('dd', 'atk-updates-current-version', 'Consultando…');
    currentValue.setAttribute('data-app-update-current-version', '');
    const availableTerm = makeElement('dt', '', 'Nova versão');
    const availableValue = makeElement('dd', 'atk-updates-available-version', '');
    availableValue.setAttribute('data-app-update-available-version', '');
    versions.append(currentTerm, currentValue, availableTerm, availableValue);

    const stateMessage = makeElement('p', 'atk-updates-state', '');
    stateMessage.setAttribute('data-app-update-state', '');
    stateMessage.setAttribute('role', 'status');
    stateMessage.setAttribute('aria-live', 'polite');

    const errorMessage = makeElement('p', 'atk-updates-error', '');
    errorMessage.setAttribute('data-app-update-error', '');
    errorMessage.setAttribute('role', 'alert');

    const notesLabel = makeElement('h3', 'atk-updates-notes-label', 'O que mudou');
    const notes = makeElement('div', 'atk-updates-notes', '');
    notes.setAttribute('data-app-update-notes', '');
    notes.setAttribute('tabindex', '0');

    const progressWrap = makeElement('div', 'atk-updates-progress-wrap');
    const progressLabel = makeElement('div', 'atk-updates-progress-label', '');
    progressLabel.setAttribute('data-app-update-progress-label', '');
    const progress = makeElement('progress', 'atk-updates-progress');
    progress.setAttribute('max', '100');
    progress.value = 0;
    progressWrap.append(progressLabel, progress);

    const safety = makeElement('p', 'atk-updates-safety',
      'A instalação fecha e abre o ATK HUB novamente. Scripts Lua em execução serão interrompidos.');

    content.append(summary, versions, stateMessage, errorMessage, notesLabel, notes, progressWrap, safety);

    const footer = makeElement('footer', 'atk-updates-actions');
    const retryCheckButton = makeElement('button', 'atk-updates-check', 'Verificar novamente', {
      type: 'button', 'data-app-update-action': 'check',
    });
    const laterButton = makeElement('button', 'atk-updates-later', 'Depois', {
      type: 'button', 'data-app-update-action': 'later',
    });
    const installButton = makeElement('button', 'atk-updates-install', 'Atualizar agora', {
      type: 'button', 'data-app-update-action': 'install',
    });
    footer.append(retryCheckButton, laterButton, installButton);
    dialog.append(header, content, footer);
    root.appendChild(dialog);
    document.body.appendChild(root);

    closeButton.addEventListener('click', dismiss);
    laterButton.addEventListener('click', dismiss);
    installButton.addEventListener('click', () => { void installUpdate(); });
    retryCheckButton.addEventListener('click', () => { void requestCheck(true); });
    document.addEventListener('keydown', onKeyDown);

    elements = {
      root, dialog, heading, summary, currentValue, availableValue, stateMessage,
      errorMessage, notesLabel, notes, progressWrap, progressLabel, progress,
      safety, closeButton, retryCheckButton, laterButton, installButton,
    };
    return elements;
  }

  function statusDescription(value) {
    switch (value.state) {
      case 'idle': return 'A verificação de atualizações ainda não começou.';
      case 'checking': return 'Verificando se há uma versão mais recente…';
      case 'upToDate': return 'Você está usando a versão mais recente disponível.';
      case 'available': return 'Uma atualização está disponível. Você decide quando instalar.';
      case 'downloading': return 'Baixando a atualização…';
      case 'installing': return 'Preparando a instalação… O ATK HUB será fechado e aberto novamente.';
      case 'error': return value.error || 'Não foi possível concluir a verificação ou instalação.';
      default: return 'Estado da atualização indisponível.';
    }
  }

  function formatBytes(bytes) {
    if (!Number.isFinite(bytes) || bytes < 0) return '';
    if (bytes < 1024) return `${Math.round(bytes)} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  }

  function updateProgress(value) {
    const active = value.state === 'downloading' || value.state === 'installing';
    setHidden(elements.progressWrap, !active);
    if (!active) return;

    const hasTotal = Number.isFinite(value.totalBytes) && value.totalBytes > 0;
    if (hasTotal) {
      const percentage = Math.max(0, Math.min(100, (value.downloadedBytes / value.totalBytes) * 100));
      elements.progress.removeAttribute('aria-valuetext');
      elements.progress.value = percentage;
      elements.progressLabel.textContent = value.state === 'installing'
        ? 'Instalando a atualização…'
        : `Baixando ${formatBytes(value.downloadedBytes)} de ${formatBytes(value.totalBytes)} (${Math.floor(percentage)}%).`;
    } else {
      elements.progress.removeAttribute('value');
      elements.progress.setAttribute('aria-valuetext', value.state === 'installing'
        ? 'Instalando a atualização' : 'Baixando a atualização');
      elements.progressLabel.textContent = value.state === 'installing'
        ? 'Instalando a atualização…' : 'Baixando a atualização…';
    }
  }

  function render() {
    if (!modalOpen) return;
    const view = createModal();
    if (!view) return;

    const visibleVersion = offerVersion || (isNonEmptyVersion(status.availableVersion) ? status.availableVersion : null);
    const checking = status.state === 'checking';
    const installing = installLocked || status.state === 'downloading' || status.state === 'installing';
    view.currentValue.textContent = status.currentVersion || 'Não informado';
    view.availableValue.textContent = visibleVersion || 'Nenhuma versão disponível no momento';
    setHidden(view.availableValue, !visibleVersion);
    const availableTerm = view.availableValue.previousSibling;
    if (availableTerm) setHidden(availableTerm, !visibleVersion);
    view.summary.textContent = statusDescription(status);
    view.stateMessage.textContent = status.state === 'error' && visibleVersion
      ? `A versão ${visibleVersion} continua disponível para uma nova tentativa.`
      : statusDescription(status);
    view.errorMessage.textContent = status.state === 'error' ? status.error || 'Ocorreu um erro na atualização.' : '';
    setHidden(view.errorMessage, status.state !== 'error');
    view.notes.textContent = status.notes || '';
    setHidden(view.notes, !status.notes);
    setHidden(view.notesLabel, !status.notes);
    view.safety.textContent = 'A instalação fecha e abre o ATK HUB novamente. Scripts Lua em execução serão interrompidos.';
    updateProgress(status);

    const canInstall = !!visibleVersion && !checking && !installing
      && (status.state === 'available' || (status.state === 'error' && !!offerVersion));
    view.installButton.textContent = status.state === 'error' && canInstall
      ? 'Tentar instalar novamente' : 'Atualizar agora';
    view.installButton.disabled = !canInstall || !!installPromise || !!checkPromise;
    setHidden(view.installButton, !canInstall);
    view.retryCheckButton.disabled = checking || installing || !!checkPromise;
    setHidden(view.retryCheckButton, status.state !== 'error');
    view.laterButton.disabled = installing;
    view.closeButton.disabled = installing;
    view.closeButton.setAttribute('aria-disabled', String(installing));
    view.laterButton.setAttribute('aria-disabled', String(installing));
    view.root.setAttribute('aria-hidden', 'false');
    view.dialog.setAttribute('aria-busy', String(checking || installing));
  }

  function rememberPrompt(version) {
    if (!isNonEmptyVersion(version)) return;
    promptVersions.add(version);
    while (promptVersions.size > 32) promptVersions.delete(promptVersions.values().next().value);
  }

  function openModal(source) {
    if (!document.body) {
      if (source === 'manual') queuedTrayOpen = true;
      return false;
    }
    if (modalOpen) {
      render();
      return true;
    }
    const view = createModal();
    if (!view) return false;
    previousFocus = document.activeElement || null;
    modalOpen = true;
    setHidden(view.root, false);
    if (source === 'automatic' || source === 'manual') rememberPrompt(offerVersion || status.availableVersion);
    render();
    const focusTarget = status.state === 'available' && offerVersion
      ? view.installButton : view.laterButton;
    focusTarget.focus();
    return true;
  }

  function closeModal(shouldDefer) {
    if (!modalOpen || installLocked || status.state === 'downloading' || status.state === 'installing') return false;
    const view = elements || createModal();
    modalOpen = false;
    if (shouldDefer) {
      if (isNonEmptyVersion(offerVersion)) deferredVersion = offerVersion;
      else if (status.state === 'checking') dismissedWhileChecking = true;
    }
    if (view) setHidden(view.root, true);
    const focusTarget = previousFocus;
    previousFocus = null;
    if (focusTarget && focusTarget.isConnected && typeof focusTarget.focus === 'function') focusTarget.focus();
    return true;
  }

  function dismiss() {
    closeModal(true);
  }

  function onKeyDown(event) {
    if (!modalOpen) return;
    if (event.key === 'Escape') {
      if (!installLocked && status.state !== 'downloading' && status.state !== 'installing') {
        event.preventDefault();
        dismiss();
      }
      return;
    }
    if (event.key !== 'Tab' || !elements) return;
    const focusable = [
      elements.closeButton,
      elements.notes,
      elements.retryCheckButton,
      elements.laterButton,
      elements.installButton,
    ].filter((node) => node && !node.hidden && !node.disabled && node.getAttribute('tabindex') !== '-1');
    if (!focusable.length) {
      event.preventDefault();
      elements.dialog.focus();
      return;
    }
    const currentIndex = focusable.indexOf(document.activeElement);
    const nextIndex = event.shiftKey
      ? (currentIndex <= 0 ? focusable.length - 1 : currentIndex - 1)
      : (currentIndex < 0 || currentIndex === focusable.length - 1 ? 0 : currentIndex + 1);
    event.preventDefault();
    focusable[nextIndex].focus();
  }

  function applyStatus(rawStatus) {
    const next = normalizeStatus(rawStatus);
    if (next.state === 'error' && !next.error) next.error = 'Ocorreu um erro ao consultar ou instalar a atualização.';
    if (next.state === 'available' && !isNonEmptyVersion(next.availableVersion)) {
      next.state = 'error';
      next.error = 'O servidor não informou a versão da atualização. Verifique novamente.';
    }

    if (next.state === 'upToDate' || next.state === 'idle') {
      offerVersion = null;
    } else if (isNonEmptyVersion(next.availableVersion)) {
      offerVersion = next.availableVersion;
    }

    status = next;
    if (next.state === 'error' && installLocked) {
      installErrorObserved = true;
      installLocked = false;
    } else if (next.state === 'downloading' || next.state === 'installing') {
      installLocked = true;
    }

    if (next.state === 'available' && isNonEmptyVersion(offerVersion)) {
      const version = offerVersion;
      if (dismissedWhileChecking) {
        deferredVersion = version;
        dismissedWhileChecking = false;
      } else if (!modalOpen && deferredVersion !== version && !promptVersions.has(version)) {
        openModal('automatic');
      }
    } else if (next.state !== 'checking' && next.state !== 'available') {
      dismissedWhileChecking = false;
    }
    render();
    return status;
  }

  function localError(error, fallback) {
    const message = errorText(error, fallback);
    const failed = Object.assign({}, status, {
      state: 'error',
      availableVersion: offerVersion || status.availableVersion,
      error: message,
    });
    if (installLocked || installPromise) {
      installErrorObserved = true;
      installLocked = false;
    }
    return applyStatus(failed);
  }

  function requestCheck(manual) {
    if (manual) manualCheckPending = true;
    if (checkPromise) return checkPromise;
    if (installLocked || status.state === 'downloading' || status.state === 'installing') return Promise.resolve(status);

    const shouldQueryStatus = manualCheckPending;
    manualCheckPending = false;
    const previousStatus = status;
    const previousOffer = offerVersion;
    checkPromise = (async () => {
      if (shouldQueryStatus) {
        try {
          applyStatus(await callNative('app_update_status'));
        } catch (error) {
          localError(error, 'Não foi possível consultar o estado da atualização.');
        }
      }

      if (installLocked) return status;
      status = normalizeStatus(Object.assign({}, status, { state: 'checking', error: '' }));
      if (!status.availableVersion && previousOffer) status.availableVersion = previousOffer;
      render();
      try {
        return applyStatus(await callNative('app_update_check'));
      } catch (error) {
        const failed = localError(error, 'Não foi possível verificar atualizações.');
        if (!failed.currentVersion && previousStatus.currentVersion) failed.currentVersion = previousStatus.currentVersion;
        if (!offerVersion && previousOffer) offerVersion = previousOffer;
        render();
        return failed;
      }
    })().finally(() => {
      checkPromise = null;
      manualCheckPending = false;
      render();
    });
    return checkPromise;
  }

  function onTrayOpen() {
    if (installLocked) {
      openModal('manual');
      return;
    }
    if (!openModal('manual')) {
      queuedTrayOpen = true;
      return;
    }
    void requestCheck(true);
  }

  function onStatusEvent(event) {
    if (!event || !event.payload || typeof event.payload !== 'object') return;
    applyStatus(event.payload);
  }

  function installUpdate() {
    const expectedVersion = offerVersion;
    if (!isNonEmptyVersion(expectedVersion) || installLocked || installPromise || checkPromise) return Promise.resolve(false);
    if (status.state !== 'available' && status.state !== 'error') return Promise.resolve(false);

    installLocked = true;
    installErrorObserved = false;
    const attemptId = ++installAttemptId;
    status = normalizeStatus(Object.assign({}, status, { state: 'installing', error: '' }));
    render();

    installPromise = (async () => {
      try {
        const result = await callNative('app_update_install', { expectedVersion });
        if (attemptId === installAttemptId && !installErrorObserved) applyStatus(result);
        return true;
      } catch (error) {
        if (attemptId === installAttemptId) {
          localError(error, 'Não foi possível instalar a atualização. Você pode tentar novamente.');
        }
        return false;
      } finally {
        if (attemptId === installAttemptId) {
          installPromise = null;
          render();
        }
      }
    })();
    return installPromise;
  }

  function start() {
    if (queuedTrayOpen) {
      queuedTrayOpen = false;
      onTrayOpen();
    }
    initialTimer = global.setTimeout(() => { void requestCheck(false); }, INITIAL_CHECK_DELAY_MS);
    periodicTimer = global.setInterval(() => { void requestCheck(false); }, CHECK_INTERVAL_MS);
  }

  if (tauri.event && typeof tauri.event.listen === 'function') {
    Promise.resolve(tauri.event.listen('app-update-status', onStatusEvent)).catch(() => {});
    Promise.resolve(tauri.event.listen('app-update-open', onTrayOpen)).catch(() => {});
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start, { once: true });
  } else {
    Promise.resolve().then(start);
  }
})(typeof window !== 'undefined' ? window : globalThis);
