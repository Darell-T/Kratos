import { spawn } from 'node:child_process';
import { mkdirSync, openSync } from 'node:fs';
import { join } from 'node:path';
import { app, BrowserWindow, shell } from 'electron';

const devUrlFlag = '--dev-url=';
const devUrl = process.argv.find((argument) => argument.startsWith(devUrlFlag))?.slice(devUrlFlag.length);
const engineUrl = devUrl ?? 'http://localhost:4200';
const enginePath = join(__dirname, '../../engine/dist/main.js');
const retryDelayMs = 2000;
const probeTimeoutMs = 1500;
const abortedLoadCode = -3;

const escapeHtml = (text: string) => text.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');

const unreachablePage = `data:text/html;charset=utf-8,${encodeURIComponent(`<!doctype html>
<meta charset="utf-8" />
<body style="margin:0;display:grid;place-items:center;height:100vh;background:#0f1115;color:#8b92a5;font:14px system-ui,sans-serif">
  <p>The Kratos engine is not reachable at ${escapeHtml(engineUrl)}. Retrying every ${retryDelayMs / 1000} seconds.</p>
</body>`)}`;

async function engineIsReachable(): Promise<boolean> {
  try {
    const response = await fetch(new URL('/api/health', engineUrl), { signal: AbortSignal.timeout(probeTimeoutMs) });
    return response.ok;
  } catch {
    return false;
  }
}

// The engine outlives the window so agents keep working after it closes. Electron's own Node runs it, so users need
// no separate Node install.
function startEngine(): void {
  const logs = app.getPath('logs');
  mkdirSync(logs, { recursive: true });
  const log = openSync(join(logs, 'engine.log'), 'a');
  spawn(process.execPath, [enginePath], {
    detached: true,
    windowsHide: true,
    stdio: ['ignore', log, log],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: '1' },
  }).unref();
}

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

function createWindow(): void {
  const win = new BrowserWindow({
    width: 1400,
    height: 900,
    backgroundColor: '#0f1115',
    title: 'Kratos',
    autoHideMenuBar: true,
    webPreferences: { contextIsolation: true, sandbox: true, nodeIntegration: false },
  });

  win.webContents.setWindowOpenHandler(({ url }) => {
    if (/^https?:/i.test(url)) void shell.openExternal(url);
    return { action: 'deny' };
  });

  let waiting = false;
  const loadWhenReachable = async () => {
    if (waiting) return;
    waiting = true;
    let shownUnreachable = false;
    while (!win.isDestroyed()) {
      if (await engineIsReachable()) {
        waiting = false;
        await win.loadURL(engineUrl).catch(() => undefined);
        return;
      }
      if (!shownUnreachable) {
        shownUnreachable = true;
        await win.loadURL(unreachablePage);
      }
      await sleep(retryDelayMs);
    }
  };

  // Covers the engine dying after the page loaded: the failed reload lands here and starts probing again.
  win.webContents.on('did-fail-load', (_event, errorCode, _description, _url, isMainFrame) => {
    if (isMainFrame && errorCode !== abortedLoadCode) void loadWhenReachable();
  });
  void loadWhenReachable();
}

app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') app.quit();
});

app.on('activate', () => {
  if (BrowserWindow.getAllWindows().length === 0) createWindow();
});

void app.whenReady().then(async () => {
  if (!devUrl && !(await engineIsReachable())) startEngine();
  createWindow();
});
