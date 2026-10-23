const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');
const { after, before, test } = require('node:test');

const frontendRoot = path.resolve(__dirname, '../../apps/desktop/web');
const evidenceDirectory = path.resolve(__dirname, '../../evidence/installer_2026-10-23');
const playwrightModule = process.env.PLAYWRIGHT_MODULE || 'playwright';
const { chromium } = require(playwrightModule);

const viewports = [
  { name: '640x360', width: 640, height: 360, deviceScaleFactor: 1 },
  { name: '819x480-dpr1_25', width: 819, height: 480, deviceScaleFactor: 1.25 },
  { name: '911x512-dpr1_5', width: 911, height: 512, deviceScaleFactor: 1.5 },
  { name: '1200x760', width: 1200, height: 760, deviceScaleFactor: 1 },
];

const maliciousPrefix = '<img src=x onerror="window.__xss=true"><script>window.__xss=true</script>\n';
const longNotes = maliciousPrefix + 'Correção de estabilidade e controles atualizados.\n'.repeat(240);

let browser;
let server;
let baseUrl;
let baseOrigin;

function contentType(filePath) {
  const extension = path.extname(filePath).toLowerCase();
  return ({
    '.css': 'text/css; charset=utf-8',
    '.html': 'text/html; charset=utf-8',
    '.js': 'text/javascript; charset=utf-8',
    '.json': 'application/json; charset=utf-8',
    '.png': 'image/png',
    '.svg': 'image/svg+xml',
    '.webp': 'image/webp',
  })[extension] || 'application/octet-stream';
}

function dialogHarnessHtml() {
  const encodedNotes = JSON.stringify(longNotes).replace(/<\//g, '<\\/');
  return `<!doctype html>
<html lang="pt-BR">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>ATK HUB update dialog layout test</title>
  <link rel="stylesheet" href="/static/index-g2FekTHX.css">
  <link rel="stylesheet" href="/responsive.css">
  <link rel="stylesheet" href="/app_updates.css">
  <script>
    (() => {
      const calls = [];
      const listeners = new Map();
      const offer = {
        currentVersion: '1.0.0',
        state: 'available',
        availableVersion: '1.2.0',
        notes: ${encodedNotes},
        downloadedBytes: 0,
        totalBytes: null,
      };
      const idle = { currentVersion: '1.0.0', state: 'idle', downloadedBytes: 0, totalBytes: null };
      const emit = (name, payload) => {
        for (const handler of listeners.get(name) || []) handler({ event: name, id: 1, payload });
      };
      window.__UPDATE_DIALOG_QA = { calls, listeners, emit, offer };
      window.__TAURI__ = {
        core: {
          async invoke(command, payload) {
            calls.push({ command, payload });
            if (command === 'app_update_status') return { ...idle };
            if (command === 'app_update_check') return { ...offer };
            if (command === 'app_update_install') {
              emit('app-update-status', { ...offer, state: 'downloading', downloadedBytes: 512, totalBytes: 2048 });
              await new Promise((resolve) => window.setTimeout(resolve, 500));
              const failed = { ...offer, state: 'error', error: 'Falha simulada no download; tente novamente.' };
              emit('app-update-status', failed);
              return failed;
            }
            throw new Error('Comando não permitido no teste: ' + command);
          },
        },
        event: {
          async listen(name, handler) {
            const handlers = listeners.get(name) || [];
            handlers.push(handler);
            listeners.set(name, handlers);
            return () => {};
          },
        },
      };
    })();
  </script>
</head>
<body>
  <div id="app"></div>
  <aside id="atk-device-diagnostics" aria-live="polite"></aside>
  <script src="/app_updates.js"></script>
</body>
</html>`;
}

function contrastRatio(foreground, background) {
  function luminance(rgb) {
    const channels = rgb.match(/\d+(?:\.\d+)?/g).slice(0, 3).map((value) => Number(value) / 255);
    const linear = channels.map((value) => value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4);
    return linear[0] * 0.2126 + linear[1] * 0.7152 + linear[2] * 0.0722;
  }
  const fg = luminance(foreground);
  const bg = luminance(background);
  return (Math.max(fg, bg) + 0.05) / (Math.min(fg, bg) + 0.05);
}

function screenshotPath(viewport) {
  return path.join(evidenceDirectory, `update-dialog-${viewport.name}.png`);
}

before(async () => {
  await fs.promises.mkdir(evidenceDirectory, { recursive: true });
  server = http.createServer(async (request, response) => {
    try {
      const requestPath = decodeURIComponent(new URL(request.url, 'http://127.0.0.1').pathname);
      if (requestPath === '/update-dialog-test.html') {
        response.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
        response.end(dialogHarnessHtml());
        return;
      }
      const relativePath = requestPath.replace(/^\/+/, '');
      const filePath = path.resolve(frontendRoot, relativePath);
      if (filePath !== frontendRoot && !filePath.startsWith(`${frontendRoot}${path.sep}`)) {
        response.writeHead(403).end('Forbidden');
        return;
      }
      const data = await fs.promises.readFile(filePath);
      response.writeHead(200, { 'content-type': contentType(filePath) });
      response.end(data);
    } catch {
      response.writeHead(404).end('Not found');
    }
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const { port } = server.address();
  baseUrl = `http://127.0.0.1:${port}/`;
  baseOrigin = new URL(baseUrl).origin;
  const launchOptions = { headless: true };
  if (process.env.BROWSER_EXECUTABLE) launchOptions.executablePath = process.env.BROWSER_EXECUTABLE;
  browser = await chromium.launch(launchOptions);
});

after(async () => {
  await browser?.close();
  if (server?.listening) await new Promise((resolve) => server.close(resolve));
});

for (const viewport of viewports) {
  test(`${viewport.name}: updater dialog stays usable and safe at this size and scale`, async () => {
    const context = await browser.newContext({
      viewport: { width: viewport.width, height: viewport.height },
      deviceScaleFactor: viewport.deviceScaleFactor,
    });
    const page = await context.newPage();
    const pageErrors = [];
    page.on('pageerror', (error) => pageErrors.push(error.message));

    try {
      await page.route('**/*', (route) => {
        const requestUrl = new URL(route.request().url());
        if ((requestUrl.protocol === 'http:' || requestUrl.protocol === 'https:') && requestUrl.origin !== baseOrigin) {
          return route.abort();
        }
        return route.continue();
      });
      await page.goto(`${baseUrl}update-dialog-test.html`, { waitUntil: 'load', timeout: 20_000 });
      await page.waitForFunction(() => window.__UPDATE_DIALOG_QA.listeners.get('app-update-open')?.length === 1);

      const nativeCalls = () => page.evaluate(() => window.__UPDATE_DIALOG_QA.calls.map((call) => ({
        command: call.command,
        expectedVersion: call.payload && call.payload.expectedVersion,
      })));
      assert.equal((await nativeCalls()).filter((call) => call.command === 'app_update_install').length, 0);

      await page.evaluate(() => window.__UPDATE_DIALOG_QA.emit('app-update-open'));
      const modal = page.locator('[data-app-update-modal]');
      await modal.waitFor({ state: 'visible', timeout: 5_000 });
      const offer = page.locator('[data-app-update-available-version]');
      await page.waitForFunction(() => document.querySelector('[data-app-update-available-version]')?.textContent === '1.2.0');
      assert.equal(await page.locator('[data-app-update-current-version]').textContent(), '1.0.0');
      assert.equal(await offer.textContent(), '1.2.0');

      const notes = page.locator('[data-app-update-notes]');
      const expectedNotes = longNotes.slice(0, 6000);
      assert.equal(await notes.textContent(), expectedNotes);
      assert.equal(await notes.locator('img, script').count(), 0, 'release notes must remain literal text');
      const notesScroll = await notes.evaluate((node) => ({
        clientHeight: node.clientHeight,
        scrollHeight: node.scrollHeight,
        scrollTop: node.scrollTop,
      }));
      assert.ok(notesScroll.scrollHeight > notesScroll.clientHeight, 'long release notes should have their own scroll area');
      await notes.evaluate((node) => { node.scrollTop = node.scrollHeight; });
      assert.ok(await notes.evaluate((node) => node.scrollTop > 0), 'release notes must be scrollable');

      const actions = page.locator('.atk-updates-actions button:visible');
      const actionLayout = await actions.evaluateAll((nodes) => nodes.map((node) => {
        const rect = node.getBoundingClientRect();
        const style = getComputedStyle(node);
        return {
          label: node.textContent,
          left: rect.left,
          right: rect.right,
          top: rect.top,
          bottom: rect.bottom,
          color: style.color,
          backgroundColor: style.backgroundColor,
        };
      }));
      assert.equal(actionLayout.length, 2, 'available updates expose only Later and Install actions');
      for (const action of actionLayout) {
        assert.ok(action.left >= 0 && action.right <= viewport.width, `${action.label} should fit horizontally`);
        assert.ok(action.top >= 0 && action.bottom <= viewport.height, `${action.label} should fit vertically`);
      }
      const primary = actionLayout.find((action) => action.label === 'Atualizar agora');
      assert.ok(primary, 'the explicit installation action should be visible');
      assert.ok(contrastRatio(primary.color, primary.backgroundColor) >= 4.5, 'primary button text must meet 4.5:1 contrast');

      const modalBounds = await modal.evaluate((node) => {
        const rect = node.getBoundingClientRect();
        return { left: rect.left, right: rect.right, top: rect.top, bottom: rect.bottom, height: rect.height };
      });
      assert.ok(modalBounds.left >= 0 && modalBounds.right <= viewport.width, 'dialog must fit the viewport width');
      assert.ok(modalBounds.top >= 0 && modalBounds.bottom <= viewport.height, 'dialog must fit the viewport height');
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, 'the page must not gain horizontal overflow');
      await page.screenshot({ path: screenshotPath(viewport), fullPage: false });

      await page.locator('[data-app-update-action="later"]').click();
      await modal.waitFor({ state: 'hidden', timeout: 2_000 });
      assert.equal((await nativeCalls()).filter((call) => call.command === 'app_update_install').length, 0);

      await page.evaluate(() => window.__UPDATE_DIALOG_QA.emit('app-update-open'));
      await modal.waitFor({ state: 'visible', timeout: 5_000 });
      await page.waitForFunction(() => document.querySelector('[data-app-update-available-version]')?.textContent === '1.2.0');

      const close = page.locator('[data-app-update-action="close"]');
      const later = page.locator('[data-app-update-action="later"]');
      const install = page.locator('[data-app-update-action="install"]');
      await notes.focus();
      await page.keyboard.press('Tab');
      assert.equal(await page.evaluate(() => document.activeElement.getAttribute('data-app-update-action')), 'later');
      await page.keyboard.press('Tab');
      assert.equal(await page.evaluate(() => document.activeElement.getAttribute('data-app-update-action')), 'install');
      await page.keyboard.press('Tab');
      assert.equal(await page.evaluate(() => document.activeElement.getAttribute('data-app-update-action')), 'close', 'Tab from the last control should wrap to close');
      await page.keyboard.press('Shift+Tab');
      assert.equal(await page.evaluate(() => document.activeElement.getAttribute('data-app-update-action')), 'install', 'Shift+Tab from close should wrap to install');
      await notes.focus();
      await page.keyboard.press('Shift+Tab');
      assert.equal(await page.evaluate(() => document.activeElement.getAttribute('data-app-update-action')), 'close', 'Shift+Tab from notes should reach close');

      await install.click();
      await page.locator('.atk-updates-progress-wrap').waitFor({ state: 'visible', timeout: 2_000 });
      assert.equal((await nativeCalls()).filter((call) => call.command === 'app_update_install').length, 1, 'install may start only after an explicit click');
      await page.waitForFunction(() => document.querySelector('[data-app-update-progress-label]')?.textContent.includes('512 B de 2.0 KB'));
      await page.locator('[data-app-update-error]').waitFor({ state: 'visible', timeout: 4_000 });
      assert.match(await page.locator('[data-app-update-error]').textContent(), /Falha simulada no download/);
      assert.equal(await install.textContent(), 'Tentar instalar novamente');
      await page.waitForFunction(() => document.querySelector('[data-app-update-action="install"]')?.disabled === false);
      assert.equal(await install.isEnabled(), true, 'a failed update must permit a retry');
      assert.equal((await nativeCalls()).find((call) => call.command === 'app_update_install').expectedVersion, '1.2.0');
      assert.equal(await close.isEnabled(), true, 'an error state can be closed safely');
      assert.equal(await later.isEnabled(), true, 'an error state can be deferred safely');

      const hiddenBounds = await modal.boundingBox();
      assert.ok(hiddenBounds, 'modal remains available for the error/retry state');
      assert.deepEqual(pageErrors, []);
    } finally {
      await context.close();
    }
  });
}
