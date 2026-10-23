// Shows a small, read-only receiver and mouse communication diagnostic.
(function installDeviceDiagnostics(global) {
  'use strict';

  const tauri = global.__TAURI__;
  const invoke = tauri && tauri.core && tauri.core.invoke;
  if (typeof invoke !== 'function') return;

  let latestState = null;
  let requestError = null;
  let retryPromise = null;
  let isChecking = false;
  let host = null;

  function invokeNative(command, payload) {
    return payload === undefined
      ? invoke.call(tauri.core, command)
      : invoke.call(tauri.core, command, payload);
  }

  function hasEnumeratedReceiver(state) {
    if (!state) return false;
    if (typeof state.receiverEnumerated === 'boolean') return state.receiverEnumerated;
    return Array.isArray(state.candidates) && state.candidates.length > 0;
  }

  function selectedDeviceKey(state) {
    const selected = state && state.selected;
    if (!selected || state.connected !== true || state.ambiguous === true) return null;
    const key = selected.deviceKey;
    if (typeof key !== 'string' || key.length === 0) return null;
    const candidates = Array.isArray(state.candidates) ? state.candidates : [];
    const matches = candidates.filter((candidate) => candidate && candidate.deviceKey === key);
    return matches.length === 1 ? key : null;
  }

  function formattedLastCheck(state) {
    const timestamp = state && state.lastCheckAtMs;
    if (!Number.isFinite(timestamp) || timestamp <= 0) return 'Última verificação: ainda não realizada.';
    const date = new Date(timestamp);
    if (Number.isNaN(date.getTime())) return 'Última verificação: horário indisponível.';
    return `Última verificação: ${date.toLocaleString('pt-BR')}.`;
  }

  function rejectionMessage(error, fallback) {
    if (typeof error === 'string' && error.trim()) return error;
    if (error && typeof error.message === 'string' && error.message.trim()) return error.message;
    return fallback;
  }

  function viewModel() {
    const state = latestState;
    const enumerated = hasEnumeratedReceiver(state);
    const selectedKey = selectedDeviceKey(state);
    const status = state && state.diagnosticStatus;
    const error = (state && state.communicationError) || requestError;
    let title;
    let message;

    if (!enumerated) {
      title = 'Nenhum receptor ATK detectado';
      message = 'Conecte o receptor USB para verificar o mouse.';
    } else if (error && (status === 'failed' || requestError)) {
      title = status === 'failed' ? 'Falha de comunicação' : 'Não foi possível iniciar a verificação';
      message = error;
    } else if (state && state.identityResponded === true && state.transport === 'usb') {
      title = state.charging === true ? 'Mouse USB conectado · carregando' : 'Mouse USB conectado · mouse respondeu';
      message = 'Transporte nativo: USB 3554:F58F.';
    } else if (state && state.identityResponded === true) {
      title = 'Receptor detectado · mouse respondeu';
      message = state.transport === 'receiver'
        ? 'Transporte nativo: receptor 2,4 GHz 373B:1085.'
        : 'A identidade do mouse foi confirmada nesta verificação.';
    } else if (!selectedKey) {
      title = 'Receptor detectado';
      message = 'Selecione um receptor no ATK HUB para verificar o mouse.';
    } else if (status === 'disconnected') {
      title = 'Receptor detectado · verificação anterior invalidada';
      message = 'O receptor desconectou depois da última verificação. Tente novamente após reconectar.';
    } else if (state && state.transport === 'usb') {
      title = 'Mouse conectado por USB';
      message = 'Verificando a identidade e o carregamento do mouse.';
    } else {
      title = 'Receptor detectado · mouse ainda não verificado';
      message = 'A identidade do mouse ainda não foi confirmada.';
    }

    return {
      title,
      message,
      lastCheck: formattedLastCheck(state),
      canRetry: !!selectedKey && !isChecking,
      isChecking,
      diagnosticStatus: status || 'not_checked',
    };
  }

  function setText(node, value) {
    if (node) node.textContent = value;
  }

  function ensureHost() {
    if (host || !global.document || !global.document.body) return host;
    host = global.document.getElementById('atk-device-diagnostics');
    const hostWasProvided = !!host;
    if (!host) {
      host = global.document.createElement('aside');
      host.id = 'atk-device-diagnostics';
      const app = global.document.getElementById('app');
      if (app && app.parentNode) app.parentNode.insertBefore(host, app.nextSibling);
      else global.document.body.appendChild(host);
    }
    host.setAttribute('aria-label', 'Diagnóstico do mouse');
    host.className = `${host.className || ''} atk-device-diagnostics`.trim();
    if (!hostWasProvided) {
      host.style.cssText = [
        'position:fixed',
        'left:16px',
        'bottom:16px',
        'z-index:10000',
        'width:min(330px, calc(100vw - 32px))',
        'box-sizing:border-box',
        'padding:12px 14px',
        'border:1px solid rgba(255,255,255,.16)',
        'border-radius:10px',
        'background:rgba(24,26,30,.94)',
        'color:#f5f6f7',
        'font:12px/1.45 system-ui, sans-serif',
        'box-shadow:0 6px 24px rgba(0,0,0,.28)',
      ].join(';');
    }

    const title = global.document.createElement('div');
    title.setAttribute('data-diagnostic-title', '');
    title.style.cssText = 'font-weight:600;margin-bottom:4px';
    const message = global.document.createElement('div');
    message.setAttribute('data-diagnostic-message', '');
    message.style.cssText = 'color:rgba(245,246,247,.78);overflow-wrap:anywhere';
    const lastCheck = global.document.createElement('div');
    lastCheck.setAttribute('data-diagnostic-last-check', '');
    lastCheck.style.cssText = 'color:rgba(245,246,247,.58);font-size:11px;margin-top:6px';
    const button = global.document.createElement('button');
    button.type = 'button';
    button.textContent = 'Tentar novamente';
    button.style.cssText = 'margin-top:9px;padding:5px 9px;border-radius:6px;border:1px solid rgba(255,255,255,.2);background:rgba(255,255,255,.08);color:inherit;cursor:pointer';
    button.addEventListener('click', () => { void retry(); });
    host.replaceChildren(title, message, lastCheck, button);
    return host;
  }

  function render() {
    const container = ensureHost();
    if (!container) return viewModel();
    const model = viewModel();
    setText(container.querySelector('[data-diagnostic-title]'), model.title);
    setText(container.querySelector('[data-diagnostic-message]'), model.message);
    setText(container.querySelector('[data-diagnostic-last-check]'), model.lastCheck);
    const button = container.querySelector('button');
    if (button) {
      button.disabled = !model.canRetry;
      button.textContent = model.isChecking ? 'Verificando…' : 'Tentar novamente';
      button.setAttribute('aria-busy', String(model.isChecking));
      button.style.opacity = model.canRetry ? '1' : '.55';
      button.style.cursor = model.canRetry ? 'pointer' : 'not-allowed';
    }
    container.setAttribute('role', 'status');
    container.setAttribute('aria-live', 'polite');
    return model;
  }

  async function refreshState() {
    latestState = await invokeNative('device_memory_state');
    render();
    return latestState;
  }

  function retry() {
    if (retryPromise) return retryPromise;
    retryPromise = (async () => {
      requestError = null;
      let key = null;
      try {
        const state = await refreshState();
        key = selectedDeviceKey(state);
        if (!key) return false;

        isChecking = true;
        render();
        await invokeNative('device_info', { deviceKey: key });
        return true;
      } catch (error) {
        requestError = rejectionMessage(error, 'Não foi possível iniciar a verificação do mouse.');
        return false;
      } finally {
        try {
          await refreshState();
        } catch (error) {
          if (!requestError) {
            requestError = rejectionMessage(error, 'Não foi possível atualizar o diagnóstico do mouse.');
          }
        }
        isChecking = false;
        render();
        retryPromise = null;
      }
    })();
    return retryPromise;
  }

  function onStateEvent(event) {
    if (event && event.payload) {
      latestState = event.payload;
      requestError = null;
      render();
    }
  }

  const api = Object.freeze({
    refreshState,
    retry,
    view: viewModel,
  });
  global.__atkDeviceDiagnostics = api;

  if (tauri.event && typeof tauri.event.listen === 'function') {
    Promise.resolve(tauri.event.listen('device-memory-state', onStateEvent)).catch(() => {});
  }

  function start() {
    render();
    void refreshState().catch((error) => {
      requestError = rejectionMessage(error, 'Não foi possível consultar o estado do receptor.');
      render();
    });
  }

  if (global.document) {
    if (global.document.readyState === 'loading') {
      global.document.addEventListener('DOMContentLoaded', start, { once: true });
    } else {
      Promise.resolve().then(start);
    }
  }
})(typeof window !== 'undefined' ? window : globalThis);
