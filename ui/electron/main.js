// star-trek-assistant Electron 主进程
// 职责：创建纯白窗口 → 拉起 star-trek-core 后端（其内部再拉起 voice-serve）
//       → 后端就绪后加载渲染层；窗口/应用退出时杀掉整个后端进程组。
// 语音架构：麦克风采集 / 唤醒词 / STT / TTS / 播放全部由纯 Rust 后端完成
// （voice-serve 用 cpal 常驻采集，core 负责编排），渲染进程仅做 UI 展示，
// 不采集音频。preload 只负责注入后端地址，前端对其做了回退兜底（不强制依赖）。
'use strict';

const { app, BrowserWindow, session, Tray, Menu, screen, nativeImage } = require('electron');
const { spawn } = require('child_process');
const path = require('path');
const fs = require('fs');

const CORE_PORT = parseInt(process.env.CORE_PORT || '8410', 10);
const DEBUG_PORT = parseInt(process.env.DEBUG_PORT || '5889', 10);
const VOICE_PORT = parseInt(process.env.VOICE_PORT || '8420', 10);
const CORE_URL = `http://127.0.0.1:${CORE_PORT}`;
// 路径适配：开发态基于项目根（target/... 与 resources/ 均在项目内）；
// 打包态 asar 不可写，资源/后端全部在 process.resourcesPath 下。
const PROJECT_ROOT = app.isPackaged
  ? process.resourcesPath
  : path.resolve(__dirname, '..', '..');
// data 与日志必须落在用户可写目录：打包态用 Electron userData（~Library/Application Support/...），
// 开发态沿用项目根 data/。
const DATA_DIR = process.env.STAR_DATA_DIR || (app.isPackaged
  ? path.join(app.getPath('userData'), 'data')
  : path.join(PROJECT_ROOT, 'data'));
const LOG_FILE = process.env.STAR_LOG_FILE || path.join(
  app.isPackaged ? app.getPath('userData') : PROJECT_ROOT,
  'star-trek-ui.log'
);
// core 后端日志（tracing + eprintln 全部走 stderr）：落盘便于诊断唤醒/静音/提示音问题
const CORE_LOG = process.env.STAR_CORE_LOG || path.join(
  app.isPackaged ? app.getPath('userData') : PROJECT_ROOT,
  'star-trek-core.log'
);

let mainWindow = null;
let backend = null; // ChildProcess
let tray = null;          // 菜单栏图标（常驻后台）
let overlayWindow = null; // 唤醒时屏幕上方浮现的大图标（类 Siri）
let overlayTimer = null;
let isQuitting = false;   // 托盘「Quit」/真正退出时置 true：区分「关闭窗口=隐藏」与「彻底退出」

function resolveBin(name, envKey) {
  const candidates = [];
  if (process.env[envKey]) candidates.push(process.env[envKey]);
  if (app.isPackaged) {
    // 打包态：后端二进制位于 Contents/Resources/bin/
    candidates.push(path.join(process.resourcesPath, 'bin', name));
  } else {
    candidates.push(path.join(PROJECT_ROOT, 'target', 'debug', name));
    candidates.push(path.join(PROJECT_ROOT, 'target', 'release', name));
  }
  for (const c of candidates) {
    if (c && fs.existsSync(c)) return c;
  }
  return null;
}

function cleanupStaleBackends() {
  // App 崩溃/强退可能遗留 core/voice-serve 孤儿进程占用固定端口，
  // 启动前按端口定位并核验进程名后清理，避免新实例绑定失败或双后端并存。
  const { execFileSync } = require('child_process');
  for (const port of [CORE_PORT, VOICE_PORT]) {
    let pids = [];
    try {
      const out = execFileSync('/usr/sbin/lsof', ['-ti', `tcp:${port}`], { encoding: 'utf8' });
      pids = out.split('\n').map((x) => x.trim()).filter(Boolean);
    } catch { /* 无进程占用该端口 */ }
    for (const pid of pids) {
      try {
        const comm = execFileSync('/bin/ps', ['-p', pid, '-o', 'comm='], { encoding: 'utf8' }).trim();
        if (/star-trek-core|voice-serve/.test(comm)) {
          console.log(`[main] 清理残留后端: pid=${pid} (${comm})`);
          process.kill(Number(pid), 'SIGKILL');
        }
      } catch { /* 进程已不存在 */ }
    }
  }
}

function startBackend() {
  const coreBin = resolveBin('star-trek-core', 'STAR_CORE_BIN');
  if (!coreBin) {
    console.error(`[main] 未找到 star-trek-core 二进制（${app.isPackaged ? 'resources/bin' : 'target/debug|release'}）`);
    return;
  }
  cleanupStaleBackends();
  console.log(`[main] spawn core: ${coreBin} (data=${DATA_DIR})`);
  // detached: 独立进程组，退出时 kill(-pid) 可连带终止 voice-serve
  backend = spawn(coreBin, ['--data-dir', DATA_DIR], {
    env: {
      ...process.env,
      CORE_PORT: String(CORE_PORT),
      // 注入项目根：后端资源（resources/ 音效、searchpin、模型）统一相对此目录解析
      STAR_TREK_ROOT: PROJECT_ROOT,
    },
    // core 的 tracing/错误输出全部落盘，避免打包态 stderr 被丢弃导致无法诊断
    stdio: ['ignore', fs.openSync(CORE_LOG, 'a'), fs.openSync(CORE_LOG, 'a')],
    detached: true,
  });
  backend.on('error', (e) => console.error('[main] core 启动错误:', e.message));
  backend.unref?.();
}

function killBackend() {
  const child = backend;
  backend = null;
  if (!child) return;
  const term = () => {
    try {
      // 终止整个进程组（core + voice-serve）
      process.kill(-child.pid, 'SIGTERM');
    } catch {
      try { child.kill('SIGTERM'); } catch { /* ignore */ }
    }
  };
  term();
  // SIGTERM 后 1.5s 未退出则 SIGKILL 兜底，确保 App 关闭后端一定消亡
  const t = setTimeout(() => {
    try { process.kill(-child.pid, 'SIGKILL'); }
    catch { try { child.kill('SIGKILL'); } catch { /* ignore */ } }
  }, 1500);
  t.unref?.();
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
  try {
    fs.appendFileSync(LOG_FILE, '[main] window loaded\n');
  } catch { /* 日志非关键 */ }
  // 关闭窗口 = 隐藏到后台（菜单栏图标继续常驻）；只有「Quit」/真正退出才销毁
  mainWindow.on('close', (e) => {
    if (isQuitting) return;
    e.preventDefault();
    mainWindow.hide();
    hideDock();
  });
  mainWindow.on('closed', () => { mainWindow = null; });
}

// 隐藏 Dock 图标：窗口关闭后只留菜单栏图标，表现为纯后台常驻
function hideDock() {
  if (process.platform === 'darwin' && app.dock) app.dock.hide();
}

function showMainWindow() {
  if (!mainWindow) createWindow();
  if (process.platform === 'darwin' && app.dock) app.dock.show();
  mainWindow.show();
  mainWindow.focus();
}

// 菜单栏图标：左键=唤醒（并浮现大图标），右键=菜单（Open Window / Quit）
function createTray() {
  const iconPath = path.join(__dirname, '..', 'src', 'assets', 'tray.png');
  let img = nativeImage.createFromPath(iconPath);
  if (img.isEmpty()) {
    console.error('[main] 托盘图标加载失败:', iconPath);
    img = nativeImage.createEmpty();
  }
  // 保持彩色（文件名不含 Template）：Template 会被系统渲染成单色剪影，丢失徽章配色
  tray = new Tray(img);
  tray.setToolTip('Star Trek Computer');
  const menu = Menu.buildFromTemplate([
    { label: 'Open Window', click: () => showMainWindow() },
    { type: 'separator' },
    { label: 'Quit', click: () => { isQuitting = true; app.quit(); } },
  ]);
  tray.on('click', () => wakeByTray());
  // 不调用 setContextMenu：那样左键也会弹菜单；改为右键时手动弹出
  tray.on('right-click', () => tray.popUpContextMenu(menu));
}

// 右上角的类 Siri 小图标（贴近菜单栏右侧）
function positionOverlay() {
  if (!overlayWindow) return;
  const wa = screen.getPrimaryDisplay().workArea;
  const W = 120;
  const H = 120;
  overlayWindow.setBounds({
    x: wa.x + wa.width - W - 16,
    y: wa.y + 6,
    width: W,
    height: H,
  });
}

function createOverlayWindow() {
  overlayWindow = new BrowserWindow({
    width: 120,
    height: 120,
    frame: false,
    transparent: true,
    backgroundColor: '#00000000',
    resizable: false,
    movable: false,
    minimizable: false,
    maximizable: false,
    fullscreenable: false,
    skipTaskbar: true,
    alwaysOnTop: true,
    focusable: false,
    hasShadow: false,
    show: false,
    webPreferences: { contextIsolation: true, nodeIntegration: false },
  });
  // 浮于全屏与其他 App 之上；不抢焦点（配合 showInactive）
  overlayWindow.setAlwaysOnTop(true, 'screen-saver');
  overlayWindow.setIgnoreMouseEvents(true);
  overlayWindow.setVisibleOnAllWorkspaces(true, { visibleOnFullScreen: true });
  overlayWindow.loadFile(path.join(__dirname, '..', 'src', 'overlay.html'));
  positionOverlay();
}

function showOverlay(durationMs = 6000) {
  if (!overlayWindow) createOverlayWindow();
  positionOverlay();
  overlayWindow.showInactive();
  if (overlayTimer) clearTimeout(overlayTimer);
  overlayTimer = setTimeout(() => {
    if (overlayWindow) overlayWindow.hide();
  }, durationMs);
}

// 托盘左键：等价于喊 "computer" —— 触发 core 唤醒流程并浮现大图标
async function wakeByTray() {
  showOverlay();
  try {
    await fetch(`${CORE_URL}/api/voice/wakeword`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ keyword: 'computer' }),
    });
  } catch (e) {
    console.error('[main] 托盘唤醒失败:', e.message);
  }
}

try {
  fs.writeFileSync(LOG_FILE, '');
} catch { /* 日志非关键 */ }
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
  createTray(); // 启动即在菜单栏常驻图标
  app.on('activate', () => showMainWindow());
});

// 窗口关闭不再退出：窗口仅隐藏，菜单栏图标继续常驻后台运行
// （真正退出走托盘「Quit」→ isQuitting=true → before-quit 清理后端）
app.on('window-all-closed', () => { /* 保持后台常驻，不退出 */ });
app.on('before-quit', () => { isQuitting = true; killBackend(); });
process.on('exit', killBackend);
process.on('SIGINT', () => { killBackend(); process.exit(0); });
