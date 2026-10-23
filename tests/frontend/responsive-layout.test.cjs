const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');
const { after, before, test } = require('node:test');

const frontendRoot = path.resolve(__dirname, '../../apps/desktop/web');
const screenshotsDir = path.resolve(__dirname, '../../evidence/improvements_2026-10-22');
const playwrightModule = process.env.PLAYWRIGHT_MODULE || 'playwright';
const { chromium } = require(playwrightModule);

const viewports = [
  { name: 'minimum-640x360', width: 640, height: 360, deviceScaleFactor: 1, detailed: true },
  { name: 'compact-800x560', width: 800, height: 560, deviceScaleFactor: 1 },
  { name: '1024x600-at-125-percent', width: 819, height: 480, deviceScaleFactor: 1.25, detailed: true },
  { name: '1024x600-at-150-percent', width: 683, height: 373, deviceScaleFactor: 1.5 },
  { name: '1366x768-at-125-percent', width: 1093, height: 614, deviceScaleFactor: 1.25 },
  { name: '1366x768-at-150-percent', width: 911, height: 512, deviceScaleFactor: 1.5, detailed: true },
  { name: 'standard-1024x600', width: 1024, height: 600, deviceScaleFactor: 1 },
  { name: 'standard-1366x768', width: 1366, height: 768, deviceScaleFactor: 1 },
  { name: 'standard-1440x840', width: 1440, height: 840, deviceScaleFactor: 1 },
];

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

async function installTauriQaMock(page) {
  await page.addInitScript(() => {
    const state = {
      receiverEnumerated: false,
      identityResponded: false,
      diagnosticStatus: 'not_checked',
      lastCheckAtMs: Date.now(),
      communicationError: null,
      connected: false,
      ambiguous: false,
      remembered: false,
      selected: null,
      candidates: [],
    };
    const calls = [];
    const listeners = new Map();

    const tauriMock = {
      core: {
        async invoke(command, payload) {
          calls.push({ command, payload: payload || null });
          if (command === 'device_memory_state') return { ...state, selected: state.selected && { ...state.selected }, candidates: state.candidates.map((item) => ({ ...item })) };
          if (command === 'device_info') return { queries: [] };
          if (command === 'lua_status') return { running: false, state: 'stopped', message: null };
          if (command === 'lua_logs_since') return { cursor: String(payload && payload.cursor || '0'), reset: false, entries: [] };
          if (command === 'lua_stop_shortcut_get') return { shortcut: 'Ctrl+Alt+F11', registered: true, error: null };
          if (command === 'lua_stop_shortcut_set') return { shortcut: payload && payload.shortcut || 'Ctrl+Alt+F11', registered: true, error: null };
          if (command === 'lua_clear_log') return null;
          return null;
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
    let enabled = false;
    window.__ATK_QA = { calls, listeners, enableTauriMock: () => { enabled = true; } };
    Object.defineProperty(window, '__TAURI__', {
      configurable: true,
      get: () => enabled ? tauriMock : undefined,
    });
  });
}

async function clickMenuLink(page, text) {
  const item = page.locator('.menu-link').filter({ hasText: text });
  assert.equal(await item.count(), 1, `expected one collapsed-sidebar item matching ${text}`);
  const menuItem = item.locator('xpath=..');
  assert.equal(await menuItem.getAttribute('role'), 'menuitem', `expected a menu item for ${text}`);
  await menuItem.click();
}

async function waitForGlobalComponent(page, component) {
  await page.waitForFunction((expected) => {
    const app = window.__vue_app__ || document.querySelector('#app')?.__vue_app__;
    const pinia = app?.config?.globalProperties?.$pinia;
    return pinia?.state?.value?.global?.component === expected;
  }, component, { timeout: 20_000 });
}

async function wheelTargetIntoScroller(page, scroller, target) {
  await target.waitFor({ state: 'attached', timeout: 8_000 });
  await page.waitForTimeout(350);
  const scrollerBox = await scroller.boundingBox();
  assert.ok(scrollerBox && scrollerBox.width > 0 && scrollerBox.height > 0, 'scrollable content pane must be visible');
  await page.mouse.move(scrollerBox.x + scrollerBox.width / 2, scrollerBox.y + scrollerBox.height / 2);

  let result;
  for (let attempt = 0; attempt < 12; attempt += 1) {
    const pane = await scroller.boundingBox();
    const targetBox = await target.boundingBox();
    assert.ok(pane && targetBox, 'scroll pane and target must remain mounted');
    const paneState = await scroller.evaluate((element) => ({
      scrollTop: element.scrollTop,
      clientHeight: element.clientHeight,
      scrollHeight: element.scrollHeight,
    }));
    result = {
      ...paneState,
      target: { top: targetBox.y, bottom: targetBox.y + targetBox.height, left: targetBox.x, right: targetBox.x + targetBox.width },
      pane: { top: pane.y, bottom: pane.y + pane.height, left: pane.x, right: pane.x + pane.width },
    };
    const targetHeight = result.target.bottom - result.target.top;
    const paneHeight = result.pane.bottom - result.pane.top;
    const topAligned = Math.abs(result.target.top - result.pane.top) <= 2;
    const fullyVisible = result.target.top >= result.pane.top - 1 && result.target.bottom <= result.pane.bottom + 1;
    if (fullyVisible || (targetHeight > paneHeight && topAligned)) return result;
    const distanceToPaneTop = result.target.top - result.pane.top;
    const delta = Math.max(-260, Math.min(260, distanceToPaneTop));
    await page.mouse.wheel(0, delta);
    await page.waitForTimeout(100);
  }
  return result;
}

function screenshotPath(viewport, suffix) {
  return path.join(screenshotsDir, `${viewport.name}-${suffix}.png`);
}

function contrastRatio(foreground, background, backdrop) {
  const parse = (value) => {
    const match = value.match(/^rgba?\(([^)]+)\)$/);
    assert.ok(match, `expected a computed RGB color, got ${value}`);
    const channels = match[1].split(/[,/\s]+/).filter(Boolean).map(Number);
    return { rgb: channels.slice(0, 3), alpha: channels[3] ?? 1 };
  };
  const fg = parse(foreground);
  const bg = parse(background);
  const base = parse(backdrop || 'rgb(255, 255, 255)');
  const effectiveBackground = bg.rgb.map((channel, index) => channel * bg.alpha + base.rgb[index] * (1 - bg.alpha));
  const composite = fg.rgb.map((channel, index) => channel * fg.alpha + effectiveBackground[index] * (1 - fg.alpha));
  const luminance = composite.map((channel) => {
    const srgb = channel / 255;
    return srgb <= 0.04045 ? srgb / 12.92 : ((srgb + 0.055) / 1.055) ** 2.4;
  }).reduce((sum, channel, index) => sum + channel * [0.2126, 0.7152, 0.0722][index], 0);
  const backgroundLuminance = effectiveBackground.map((channel) => {
    const srgb = channel / 255;
    return srgb <= 0.04045 ? srgb / 12.92 : ((srgb + 0.055) / 1.055) ** 2.4;
  }).reduce((sum, channel, index) => sum + channel * [0.2126, 0.7152, 0.0722][index], 0);
  const lighter = Math.max(luminance, backgroundLuminance);
  const darker = Math.min(luminance, backgroundLuminance);
  return (lighter + 0.05) / (darker + 0.05);
}

before(async () => {
  await fs.promises.mkdir(screenshotsDir, { recursive: true });
  server = http.createServer(async (request, response) => {
    try {
      const requestPath = decodeURIComponent(new URL(request.url, 'http://127.0.0.1').pathname);
      const relativePath = requestPath === '/' ? 'index.html' : requestPath.replace(/^\/+/, '');
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
  if (server?.listening) {
    await new Promise((resolve) => server.close(resolve));
  }
});

for (const viewport of viewports) {
  test(`${viewport.name}: active pages, footer diagnostics, and responsive scrolling remain reachable`, async () => {
    const context = await browser.newContext({
      viewport: { width: viewport.width, height: viewport.height },
      deviceScaleFactor: viewport.deviceScaleFactor,
    });
    const page = await context.newPage();
    const pageErrors = [];
    page.on('pageerror', (error) => pageErrors.push(error.message));
    try {
      await installTauriQaMock(page);
      await page.route('**/*', (route) => {
        const requestUrl = new URL(route.request().url());
        if ((requestUrl.protocol === 'http:' || requestUrl.protocol === 'https:') && requestUrl.origin !== baseOrigin) {
          return route.abort();
        }
        return route.continue();
      });
      await page.goto(baseUrl, { waitUntil: 'load', timeout: 30_000 });

      const demoAction = page.getByText('Enter Demo Mode', { exact: true }).first();
      await demoAction.click({ timeout: 10_000 });
      await page.getByText('ATK F1 Ultimate 2.0', { exact: true }).first().click({ timeout: 10_000 });
      await page.locator('#layout-sidebar-nav').waitFor({ state: 'visible', timeout: 15_000 });
      await page.getByText('Data Loading', { exact: true }).waitFor({ state: 'hidden', timeout: 15_000 });
      await waitForGlobalComponent(page, 'ChangeKeySetting');
      await page.evaluate(() => window.__ATK_QA.enableTauriMock());
      await page.addScriptTag({ url: `${baseUrl}device_diagnostics.js` });

      const shell = await page.evaluate(() => {
        const box = (element) => {
          if (!element) return null;
          const rect = element.getBoundingClientRect();
          return { x: rect.x, y: rect.y, width: rect.width, height: rect.height, right: rect.right, bottom: rect.bottom };
        };
        const app = document.querySelector('#app');
        const diagnostics = document.querySelector('#atk-device-diagnostics');
        const nav = document.querySelector('#layout-sidebar-nav');
        return {
          viewport: { width: innerWidth, height: innerHeight, dpr: devicePixelRatio },
          document: { clientWidth: document.documentElement.clientWidth, scrollWidth: document.documentElement.scrollWidth },
          body: { clientWidth: document.body.clientWidth, scrollWidth: document.body.scrollWidth },
          app: { ...box(app), scrollWidth: app.scrollWidth, clientWidth: app.clientWidth },
          nav: box(nav),
          diagnostics: box(diagnostics),
          diagnosticTitle: diagnostics && diagnostics.querySelector('[data-diagnostic-title]')?.textContent,
          diagnosticMessageColor: diagnostics && getComputedStyle(diagnostics.querySelector('[data-diagnostic-message]')).color,
          diagnosticLastCheckColor: diagnostics && getComputedStyle(diagnostics.querySelector('[data-diagnostic-last-check]')).color,
          diagnosticBackgroundColor: diagnostics && getComputedStyle(diagnostics).backgroundColor,
          diagnosticBackdropColor: (() => {
            const bodyBackground = getComputedStyle(document.body).backgroundColor;
            const htmlBackground = getComputedStyle(document.documentElement).backgroundColor;
            if (!bodyBackground.endsWith(', 0)') && bodyBackground !== 'transparent') return bodyBackground;
            if (!htmlBackground.endsWith(', 0)') && htmlBackground !== 'transparent') return htmlBackground;
            return 'rgb(255, 255, 255)';
          })(),
          diagnosticButton: box(diagnostics && diagnostics.querySelector('button')),
          diagnosticButtonDisabled: diagnostics && diagnostics.querySelector('button')?.disabled,
          exitDemo: box(document.querySelector('#layout-header .demo-capsule > span')),
          exitDemoWhiteSpace: getComputedStyle(document.querySelector('#layout-header .demo-capsule > span')).whiteSpace,
        };
      });

      assert.ok(shell.nav && shell.nav.width >= 40 && shell.nav.height > 0, `collapsed navigation must remain reachable: ${JSON.stringify(shell)}`);
      assert.ok(shell.document.scrollWidth <= shell.document.clientWidth + 1, `document horizontal overflow: ${JSON.stringify(shell)}`);
      assert.ok(shell.body.scrollWidth <= shell.body.clientWidth + 1, `body horizontal overflow: ${JSON.stringify(shell)}`);
      assert.ok(shell.app.scrollWidth <= shell.app.clientWidth + 1, `app horizontal overflow: ${JSON.stringify(shell)}`);
      assert.match(shell.diagnosticTitle || '', /receptor/i, `diagnostic status should be rendered: ${JSON.stringify(shell)}`);
      assert.ok(shell.diagnostics && shell.diagnostics.height > 0, `diagnostic footer is not laid out: ${JSON.stringify(shell)}`);
      assert.ok(contrastRatio(shell.diagnosticMessageColor, shell.diagnosticBackgroundColor, shell.diagnosticBackdropColor) >= 4.5, `diagnostic message should remain readable against the light footer: ${JSON.stringify(shell)}`);
      assert.ok(contrastRatio(shell.diagnosticLastCheckColor, shell.diagnosticBackgroundColor, shell.diagnosticBackdropColor) >= 4.5, `diagnostic timestamp should remain readable against the light footer: ${JSON.stringify(shell)}`);
      assert.ok(shell.diagnosticButton && shell.diagnosticButton.height > 0, `diagnostic retry button is not visible: ${JSON.stringify(shell)}`);
      assert.equal(shell.diagnosticButtonDisabled, true, 'retry stays disabled without an enumerated receiver');
      assert.ok(shell.diagnostics.y >= shell.app.bottom - 1, `diagnostic footer overlaps the app: ${JSON.stringify(shell)}`);
      assert.ok(shell.diagnosticButton.y >= 0 && shell.diagnosticButton.bottom <= viewport.height + 1, `diagnostic button is outside the viewport: ${JSON.stringify(shell)}`);
      assert.equal(shell.exitDemoWhiteSpace, 'nowrap', `demo label must stay on one line: ${JSON.stringify(shell)}`);
      assert.ok(shell.exitDemo && shell.exitDemo.y >= 0 && shell.exitDemo.bottom <= 48, `demo label should fit the title bar: ${JSON.stringify(shell)}`);

      const diagnosticButton = page.locator('#atk-device-diagnostics button');
      assert.equal(await diagnosticButton.isDisabled(), true, 'diagnostic action must not touch hardware when the test has no receiver');
      assert.equal(await page.evaluate(() => window.__atkDeviceDiagnostics.retry()), false, 'retry API should safely stop without a selected receiver');
      assert.equal(await page.evaluate(() => window.__ATK_QA.calls.some((call) => call.command === 'device_info')), false, 'no device identity query should run in the hardware-free harness');

      await clickMenuLink(page, 'Performance Settings');
      await waitForGlobalComponent(page, 'PerformanceSetting');
      const performancePane = page.locator('.hub-route-transition-host > :not(.side-layout)');
      await page.locator('#performance-setting-polling-rate').waitFor({ state: 'visible', timeout: 8_000 });
      const pollingCard = page.locator('#performance-setting-polling-rate');
      assert.match(await pollingCard.innerText(), /125[\s\S]*250[\s\S]*500/i, 'polling-rate options should be present');
      if (viewport.detailed) {
        await page.screenshot({ path: screenshotPath(viewport, 'performance') });
        const sensorRateCard = page.locator('#performance-setting-sensor-rate');
        const performanceScroll = await wheelTargetIntoScroller(page, performancePane, sensorRateCard);
        assert.match(await sensorRateCard.innerText(), /Basic Mode[\s\S]*ATK Shard Competitive Firmware[\s\S]*ATK Shard Competitive Firmware MAX/i, 'sensor sampling choices should be present');
        assert.ok(performanceScroll.scrollHeight > performanceScroll.clientHeight, `performance page should scroll vertically: ${JSON.stringify(performanceScroll)}`);
        assert.ok(performanceScroll.scrollTop > 0, `sensor controls must be reachable by scrolling: ${JSON.stringify(performanceScroll)}`);
        assert.ok(performanceScroll.target.top <= performanceScroll.pane.bottom && performanceScroll.target.bottom >= performanceScroll.pane.top, `sensor controls should intersect the visible pane after scrolling: ${JSON.stringify(performanceScroll)}`);
        assert.ok(performanceScroll.target.left >= performanceScroll.pane.left - 1 && performanceScroll.target.right <= performanceScroll.pane.right + 1, `sensor controls should not be clipped horizontally: ${JSON.stringify(performanceScroll)}`);
        const maxSensorOption = sensorRateCard.getByText('ATK Shard Competitive Firmware MAX', { exact: true });
        const maxOptionBox = await maxSensorOption.boundingBox();
        assert.ok(maxOptionBox && maxOptionBox.y >= performanceScroll.pane.top - 1 && maxOptionBox.y + maxOptionBox.height <= performanceScroll.pane.bottom + 1, `sensor option should remain visible after scrolling: ${JSON.stringify({ maxOptionBox, pane: performanceScroll.pane })}`);
        await page.screenshot({ path: screenshotPath(viewport, 'performance-scroll') });
      }

      await clickMenuLink(page, 'DPI Settings');
      await waitForGlobalComponent(page, 'DpiSetting');
      await page.locator('#dpi-setting-add-dpi').waitFor({ state: 'visible', timeout: 8_000 });
      const dpiScroller = page.locator('#app .hub-route-transition-host > :not(.side-layout) [class*="overflow-y-auto"]').first();
      const lastDpi = page.locator('#dpi-setting-dpi-item-3');
      if (viewport.detailed) {
        await page.screenshot({ path: screenshotPath(viewport, 'dpi') });
        const dpiScroll = await wheelTargetIntoScroller(page, dpiScroller, lastDpi);
        if (dpiScroll.scrollHeight > dpiScroll.clientHeight) {
          assert.ok(dpiScroll.scrollTop > 0, `last DPI controls must be reachable by scrolling: ${JSON.stringify(dpiScroll)}`);
        } else {
          assert.ok(dpiScroll.target.top >= dpiScroll.pane.top - 1 && dpiScroll.target.bottom <= dpiScroll.pane.bottom + 1, `last DPI controls should remain visible when no scroll is needed: ${JSON.stringify(dpiScroll)}`);
        }
        assert.ok(dpiScroll.target.top >= dpiScroll.pane.top - 1 && dpiScroll.target.bottom <= dpiScroll.pane.bottom + 1, `last DPI controls should fit the visible pane: ${JSON.stringify(dpiScroll)}`);
        await page.screenshot({ path: screenshotPath(viewport, 'dpi-scroll') });
      }

      await clickMenuLink(page, 'Lua script');
      await waitForGlobalComponent(page, 'LuaSetting');
      const luaPage = page.locator('.atk-lua-page');
      await luaPage.waitFor({ state: 'visible', timeout: 8_000 });
      const editor = luaPage.locator('textarea').first();
      assert.ok(await editor.count(), 'Lua route must mount its script editor');
      assert.ok(await luaPage.getByRole('button', { name: 'Executar', exact: true }).count(), 'Lua editor must expose the Run button');
      assert.ok(await luaPage.locator('.atk-lua-console-toolbar').count(), 'Lua route must mount the live console toolbar');

      if (viewport.detailed) {
        const scriptSelect = page.locator('#atk-lua-script-select');
        const scriptSelectReachability = await wheelTargetIntoScroller(page, luaPage, scriptSelect);
        assert.ok(scriptSelectReachability.target.left >= scriptSelectReachability.pane.left - 1 && scriptSelectReachability.target.right <= scriptSelectReachability.pane.right + 1, `Lua library selector must fit the page width: ${JSON.stringify(scriptSelectReachability)}`);
        await page.screenshot({ path: screenshotPath(viewport, 'lua-library') });

        const libraryButtonSelectors = [
          '#atk-lua-new-script-btn',
          '#atk-lua-rename-script-btn',
          '#atk-lua-save-script-btn',
          '#atk-lua-checkpoint-script-btn',
          '#atk-lua-restore-version-btn',
        ];
        for (const selector of libraryButtonSelectors) {
          const button = page.locator(selector);
          const reachability = await wheelTargetIntoScroller(page, luaPage, button);
          assert.ok(reachability.target.top >= reachability.pane.top - 1 && reachability.target.bottom <= reachability.pane.bottom + 1, `Lua library control ${selector} must fit the visible page: ${JSON.stringify(reachability)}`);
          assert.ok(reachability.target.left >= reachability.pane.left - 1 && reachability.target.right <= reachability.pane.right + 1, `Lua library control ${selector} must fit the page width: ${JSON.stringify(reachability)}`);
          const style = await button.evaluate((element) => ({
            color: getComputedStyle(element).color,
            background: getComputedStyle(element).backgroundColor,
            width: element.getBoundingClientRect().width,
            height: element.getBoundingClientRect().height,
          }));
          assert.ok(style.width > 0 && style.height > 0, `Lua library control ${selector} must have a visible hit area: ${JSON.stringify(style)}`);
          assert.ok(contrastRatio(style.color, style.background) >= 4.5, `Lua library control ${selector} must have legible text: ${JSON.stringify(style)}`);
        }

        const newScriptButton = page.locator('#atk-lua-new-script-btn');
        await wheelTargetIntoScroller(page, luaPage, newScriptButton);
        await newScriptButton.click();
        const scriptNameInput = page.locator('#atk-lua-script-name-input');
        await scriptNameInput.waitFor({ state: 'visible' });
        const nameInputStyle = await scriptNameInput.evaluate((element) => ({ color: getComputedStyle(element).color, background: getComputedStyle(element).backgroundColor }));
        assert.ok(contrastRatio(nameInputStyle.color, nameInputStyle.background) >= 4.5, `Lua library name field must have legible text: ${JSON.stringify(nameInputStyle)}`);
        const submitScriptButton = page.locator('#atk-lua-script-name-submit');
        const cancelScriptButton = page.locator('#atk-lua-script-name-cancel');
        const submitStyle = await submitScriptButton.evaluate((element) => ({ color: getComputedStyle(element).color, background: getComputedStyle(element).backgroundColor }));
        assert.ok(contrastRatio(submitStyle.color, submitStyle.background) >= 4.5, `Lua library primary action must retain its accessible blue surface: ${JSON.stringify(submitStyle)}`);
        const cancelReachability = await wheelTargetIntoScroller(page, luaPage, cancelScriptButton);
        assert.ok(cancelReachability.target.top >= cancelReachability.pane.top - 1 && cancelReachability.target.bottom <= cancelReachability.pane.bottom + 1, `Lua library cancel action must fit the visible page: ${JSON.stringify(cancelReachability)}`);
        const cancelStyle = await cancelScriptButton.evaluate((element) => ({ color: getComputedStyle(element).color, background: getComputedStyle(element).backgroundColor }));
        assert.ok(contrastRatio(cancelStyle.color, cancelStyle.background) >= 4.5, `Lua library cancel action must have legible text: ${JSON.stringify(cancelStyle)}`);
        await page.screenshot({ path: screenshotPath(viewport, 'lua-library-form') });
        await cancelScriptButton.click();
        await scriptNameInput.waitFor({ state: 'hidden' });

        const editorReachability = await wheelTargetIntoScroller(page, luaPage, editor);
        assert.ok(editorReachability.scrollHeight > editorReachability.clientHeight, `Lua editor page should scroll vertically: ${JSON.stringify(editorReachability)}`);
        assert.ok(editorReachability.scrollTop > 0, `Lua editor must be reachable by scrolling: ${JSON.stringify(editorReachability)}`);
        assert.ok(editorReachability.target.top >= editorReachability.pane.top - 1 && editorReachability.target.bottom <= editorReachability.pane.bottom + 1, `Lua editor must fit the visible page: ${JSON.stringify(editorReachability)}`);
        assert.ok(editorReachability.target.left >= editorReachability.pane.left - 1 && editorReachability.target.right <= editorReachability.pane.right + 1, `Lua editor must not be clipped horizontally: ${JSON.stringify(editorReachability)}`);
        await page.screenshot({ path: screenshotPath(viewport, 'lua-editor') });

        const applyShortcut = luaPage.getByRole('button', { name: 'Aplicar atalho', exact: true });
        const buttonReachability = await wheelTargetIntoScroller(page, luaPage, applyShortcut);
        assert.ok(buttonReachability.scrollTop > 0, `Lua shortcut action must be reachable by scrolling: ${JSON.stringify(buttonReachability)}`);
        assert.ok(buttonReachability.target.top >= buttonReachability.pane.top - 1 && buttonReachability.target.bottom <= buttonReachability.pane.bottom + 1, `Lua shortcut action must fit the visible page: ${JSON.stringify(buttonReachability)}`);
        const shortcutInput = page.locator('#atk-lua-stop-shortcut-input');
        const shortcutInputStyle = await shortcutInput.evaluate((element) => ({ color: getComputedStyle(element).color, background: getComputedStyle(element).backgroundColor }));
        assert.ok(contrastRatio(shortcutInputStyle.color, shortcutInputStyle.background) >= 4.5, `Lua stop shortcut field must have legible text: ${JSON.stringify(shortcutInputStyle)}`);
        const shortcutButtonStyle = await applyShortcut.evaluate((element) => ({ color: getComputedStyle(element).color, background: getComputedStyle(element).backgroundColor }));
        assert.ok(contrastRatio(shortcutButtonStyle.color, shortcutButtonStyle.background) >= 4.5, `Lua stop shortcut action must retain legible white text on blue: ${JSON.stringify(shortcutButtonStyle)}`);
        await page.screenshot({ path: screenshotPath(viewport, 'lua-shortcut') });

        const consoleToolbar = luaPage.locator('.atk-lua-console-toolbar');
        const toolbarReachability = await wheelTargetIntoScroller(page, luaPage, consoleToolbar);
        const toolbarState = await consoleToolbar.evaluate((toolbar) => ({
          flexWrap: getComputedStyle(toolbar).flexWrap,
          clientWidth: toolbar.clientWidth,
          scrollWidth: toolbar.scrollWidth,
        }));
        Object.assign(toolbarReachability, toolbarState);
        assert.ok(toolbarReachability.scrollTop > 0, `Lua console toolbar must be reachable by scrolling: ${JSON.stringify(toolbarReachability)}`);
        assert.equal(toolbarReachability.flexWrap, 'wrap', `Lua console controls should wrap at narrow widths: ${JSON.stringify(toolbarReachability)}`);
        assert.ok(toolbarReachability.clientWidth > 0 && toolbarReachability.scrollWidth <= toolbarReachability.clientWidth + 1, `Lua console controls should not overflow horizontally: ${JSON.stringify(toolbarReachability)}`);
        assert.ok(toolbarReachability.target.top >= toolbarReachability.pane.top - 1 && toolbarReachability.target.bottom <= toolbarReachability.pane.bottom + 1, `Lua console toolbar must fit the visible page: ${JSON.stringify(toolbarReachability)}`);
        const pauseLogButton = page.locator('#atk-lua-log-pause');
        const pauseReachability = await wheelTargetIntoScroller(page, luaPage, pauseLogButton);
        assert.ok(pauseReachability.scrollTop > 0, `Lua console action button must be reachable by scrolling: ${JSON.stringify(pauseReachability)}`);
        assert.ok(pauseReachability.target.top >= pauseReachability.pane.top - 1 && pauseReachability.target.bottom <= pauseReachability.pane.bottom + 1, `Lua console action button must fit the visible page: ${JSON.stringify(pauseReachability)}`);
        assert.ok(pauseReachability.target.left >= pauseReachability.pane.left - 1 && pauseReachability.target.right <= pauseReachability.pane.right + 1, `Lua console action button must not be clipped horizontally: ${JSON.stringify(pauseReachability)}`);
        const logFilter = page.locator('#atk-lua-log-filter');
        const filterReachability = await wheelTargetIntoScroller(page, luaPage, logFilter);
        assert.ok(filterReachability.target.top >= filterReachability.pane.top - 1 && filterReachability.target.bottom <= filterReachability.pane.bottom + 1, `Lua console filter must fit the visible page: ${JSON.stringify(filterReachability)}`);
        assert.ok(filterReachability.target.left >= filterReachability.pane.left - 1 && filterReachability.target.right <= filterReachability.pane.right + 1, `Lua console filter must not be clipped horizontally: ${JSON.stringify(filterReachability)}`);
        for (const [controlName, control] of [['filter', logFilter], ['pause', pauseLogButton]]) {
          const style = await control.evaluate((element) => ({
            color: getComputedStyle(element).color,
            background: getComputedStyle(element).backgroundColor,
            width: element.getBoundingClientRect().width,
            height: element.getBoundingClientRect().height,
          }));
          assert.ok(style.width > 0 && style.height > 0, `Lua console ${controlName} control must have a visible hit area: ${JSON.stringify(style)}`);
          assert.ok(contrastRatio(style.color, style.background) >= 4.5, `Lua console ${controlName} control text must be legible on its light surface: ${JSON.stringify(style)}`);
        }
        await page.screenshot({ path: screenshotPath(viewport, 'lua-console') });
      }

      assert.deepEqual(pageErrors, [], `frontend raised page errors: ${JSON.stringify(pageErrors)}`);
      const luaConsoleControls = viewport.detailed ? await page.evaluate(() => ['#atk-lua-log-filter', '#atk-lua-log-pause'].map((selector) => {
        const element = document.querySelector(selector);
        const rect = element?.getBoundingClientRect();
        const style = element && getComputedStyle(element);
        return { selector, text: element?.textContent, rect: rect && [rect.x, rect.y, rect.width, rect.height], display: style?.display, visibility: style?.visibility, opacity: style?.opacity, color: style?.color, background: style?.backgroundColor };
      })) : undefined;
      console.log(JSON.stringify({ viewport: viewport.name, shell, pageErrors, luaConsoleControls, diagnosticInvokeCalls: await page.evaluate(() => window.__ATK_QA.calls.map((call) => call.command)) }));
    } catch (error) {
      const failedPath = screenshotPath(viewport, 'failure');
      await page.screenshot({ path: failedPath }).catch(() => {});
      console.error(JSON.stringify({ viewport: viewport.name, pageErrors, failedPath }));
      throw error;
    } finally {
      await context.close();
    }
  });
}
