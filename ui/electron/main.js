// star-trek-assistant Electron 主进程
// 职责：创建纯白窗口 → 拉起 star-trek-core 后端（其内部再拉起 voice-serve）
//       → 后端就绪后加载渲染层；窗口/应用退出时杀掉整个后端进程组。
// 语音架构：麦克风采集 / 唤醒词 / STT / TTS / 播放全部由纯 Rust 后端完成
// （voice-serve 用 cpal 常驻采集，core 负责编排），渲染进程仅做 UI 展示，
// 不采集音频。preload 只负责注入后端地址，前端对其做了回退兜底（不强制依赖）。
'use strict';

const { app, BrowserWindow, session } = require('electron');
const { spawn } = require('child_process');
const path = require('path');
const fs = require('fs');

const CORE_PORT = parseInt(process.env.CORE_PORT || '8410', 10);
const DEBUG_PORT = parseInt(process.env.DEBUG_PORT || '5889', 10);
const VOICE_PORT = parseInt(process.env.VOICE_PORT || '8420', 10);
const CORE_URL = `http://127.0.0.1:${CORE_PORT}`;
const PROJECT_ROOT = path.resolve(__dirname, '..', '..');
const DATA_DIR = process.env.STAR_DATA_DIR || path.join(PROJECT_ROOT, 'data');

let mainWindow = null;
let backend = null; // ChildProcess

function resolveBin(name, envKey) {
  const candidates = [];
  if (process.env[envKey]) candidates.push(process.env[envKey]);
  candidates.push(path.join(PROJECT_ROOT, 'target', 'debug', name));
  candidates.push(path.join(PROJECT_ROOT, 'target', 'release', name));
  for (const c of candidates) {
    if (c && fs.existsSync(c)) return c;
  }
  return null;
}

function startBackend() {
  const coreBin = resolveBin('star-trek-core', 'STAR_CORE_BIN');
  if (!coreBin) {
    console.error('[main] 未找到 star-trek-core 二进制（target/debug 或 target/release）');
    return;
  }
  console.log(`[main] spawn core: ${coreBin} (data=${DATA_DIR})`);
  // detached: 独立进程组，退出时 kill(-pid) 可连带终止 voice-serve
  backend = spawn(coreBin, ['--data-dir', DATA_DIR], {
    env: { ...process.env, CORE_PORT: String(CORE_PORT) },
    stdio: 'ignore',
    detached: true,
  });
  backend.on('error', (e) => console.error('[main] core 启动错误:', e.message));
  backend.unref?.();
}

function killBackend() {
  if (backend) {
    try {
      // 终止整个进程组（core + voice-serve）
      process.kill(-backend.pid, 'SIGTERM');
    } catch {
      try { backend.kill('SIGTERM'); } catch { /* ignore */ }
    }
    backend = null;
  }
}

async function waitForBackend(timeoutMs = 20000) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      const r = await fetch(`${CORE_URL}/health`);
      if (r.ok) return true;
    } catch { /* not ready yet */ }
    await new Promise((res) => setTimeout(res, 300));
  }
  return false;
}

function createWindow() {
  mainWindow = new BrowserWindow({
    width: 1240,
    height: 860,
    minWidth: 980,
    minHeight: 640,
    backgroundColor: '#ffffff',
    title: 'Star Trek Assistant',
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
    },
  });
  mainWindow.setMenuBarVisibility(false);
  mainWindow.loadFile(path.join(__dirname, '..', 'src', 'index.html'));
  console.log('[main] window loaded');
  require('fs').appendFileSync('/tmp/star-trek-main.log', '[main] window loaded\n');
  mainWindow.on('closed', () => { mainWindow = null; });
}

require('fs').writeFileSync('/tmp/star-trek-main.log', '');
app.whenReady().then(async () => {
  app.commandLine.appendSwitch('remote-debugging-port', String(DEBUG_PORT));
  // 放行媒体权限：唤醒/语音链路由渲染进程 Web Audio（getUserMedia）采集，
  // 必须在这里放行 media 权限请求，否则 getUserMedia 会静默失败、唤醒不触发
  // （照抄 goose 桌面版：音频采集在 Electron 渲染层，权限归属 GUI 应用本体）。
  session.defaultSession.setPermissionRequestHandler((_wc, permission, callback) => {
    callback(permission === 'media' || permission === 'mediaKeySystem');
  });
  startBackend();
  await waitForBackend();
  createWindow();
  app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

// 窗口全部关闭 / 应用退出 / 进程退出 → 终止后端
app.on('window-all-closed', () => {
  killBackend();
  app.quit();
});
app.on('before-quit', killBackend);
process.on('exit', killBackend);
process.on('SIGINT', () => { killBackend(); process.exit(0); });
