// star-trek-assistant Electron 主进程
// 职责：创建纯白窗口 → 拉起 star-trek-core 后端（其内部再拉起 voice-serve）
//       → 后端就绪后加载渲染层；窗口/应用退出时杀掉整个后端进程组。
// 语音架构：麦克风采集 / 唤醒词 / STT / TTS / 播放全部由纯 Rust 后端完成
// （voice-serve 用 cpal 常驻采集，core 负责编排），渲染进程仅做 UI 展示，
// 不采集音频。preload 只负责注入后端地址，前端对其做了回退兜底（不强制依赖）。
'use strict';

const { app, BrowserWindow, session, Tray, Menu, screen, nativeImage, ipcMain, dialog } = require('electron');
const { spawn } = require('child_process');
const path = require('path');
const fs = require('fs');

const CORE_PORT = parseInt(process.env.CORE_PORT || '8410', 10);
// 调试端口默认关闭：常驻开放 remote-debugging 等于给本机任意进程一个
// 完全控制渲染层的入口。只有显式设置 DEBUG_PORT 环境变量才开启。
const DEBUG_PORT = process.env.DEBUG_PORT ? parseInt(process.env.DEBUG_PORT, 10) : null;
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
let backendRestarts = 0;  // 崩溃自动重启计数（成功运行一段时间后清零）
let backendStartedAt = 0;

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

// 语音/重排模型放在用户目录（跨版本升级稳定）：App 升级替换 Contents/Resources
// 不会影响它，避免每次重装后模型丢失导致唤醒与播报静默失效。
const MODELS_DIR = process.env.STAR_MODELS_DIR || (app.isPackaged
  ? path.join(app.getPath('userData'), 'models')
  : path.join(PROJECT_ROOT, 'resources', 'models'));

// 首次启动/升级时把随包模型引导到用户目录；已存在则完全不碰（避免覆盖用户模型）。
function ensureModels() {
  try {
    const required = [
      path.join(MODELS_DIR, 'kws', 'encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx'),
      path.join(MODELS_DIR, 'paraformer-zh', 'model.int8.onnx'),
    ];
    if (required.every((p) => fs.existsSync(p))) return;
    if (!app.isPackaged) return;
    const bundled = path.join(PROJECT_ROOT, 'resources', 'models');
    if (!fs.existsSync(bundled)) {
      console.warn(`[main] 模型缺失且无随包模型可引导: ${MODELS_DIR}`);
      return;
    }
    console.log(`[main] 首次引导模型: ${bundled} -> ${MODELS_DIR}`);
    fs.mkdirSync(MODELS_DIR, { recursive: true });
    fs.cpSync(bundled, MODELS_DIR, { recursive: true, force: false, errorOnExist: false });
  } catch (e) {
    console.warn(`[main] 引导模型失败: ${e.message}`);
  }
}

function startBackend() {
  const coreBin = resolveBin('star-trek-core', 'STAR_CORE_BIN');
  if (!coreBin) {
    console.error(`[main] 未找到 star-trek-core 二进制（${app.isPackaged ? 'resources/bin' : 'target/debug|release'}）`);
    notifyRendererBackendDown('未找到后端程序，请重新安装应用');
    return;
  }
  cleanupStaleBackends();
  console.log(`[main] spawn core: ${coreBin} (data=${DATA_DIR})`);
  backendStartedAt = Date.now();
  // detached: 独立进程组，退出时 kill(-pid) 可连带终止 voice-serve
  backend = spawn(coreBin, ['--data-dir', DATA_DIR], {
    env: {
      ...process.env,
      CORE_PORT: String(CORE_PORT),
      // 注入项目根：后端资源（resources/ 音效、模型）统一相对此目录解析
      STAR_TREK_ROOT: PROJECT_ROOT,
      // 语音模型固定指向用户目录，升级替换 App 不影响
      KWS_DIR: path.join(MODELS_DIR, 'kws'),
      MODEL_DIR: path.join(MODELS_DIR, 'paraformer-zh'),
      VAD_MODEL: path.join(MODELS_DIR, 'silero-vad', 'silero_vad.onnx'),
      // 本地 Kokoro TTS：打包态随 App 附带；开发态指向本机项目
      KOKORO_DIR: process.env.KOKORO_DIR || (app.isPackaged
        ? path.join(process.resourcesPath, 'kokoro')
        : path.resolve(__dirname, '..', '..', '..', 'develop', 'kokoro-v1.1-zh-rust')),
      KOKORO_BIN: process.env.KOKORO_BIN || (app.isPackaged
        ? path.join(process.resourcesPath, 'kokoro', 'kokoro-tts')
        : path.resolve(__dirname, '..', '..', '..', 'develop', 'kokoro-v1.1-zh-rust', 'target', 'release', 'kokoro-tts')),
    },
    // core 的 tracing/错误输出全部落盘，避免打包态 stderr 被丢弃导致无法诊断
    stdio: ['ignore', fs.openSync(CORE_LOG, 'a'), fs.openSync(CORE_LOG, 'a')],
    detached: true,
  });
  backend.on('error', (e) => console.error('[main] core 启动错误:', e.message));
  // 崩溃自动重启：后端异常退出（OOM/panic/被杀）时窗口不再永久失联。
  // 稳定运行超过 60s 视为一次成功启动，重启计数清零；连续快速崩溃最多重试 5 次。
  backend.on('exit', (code, signal) => {
    if (isQuitting) return;
    console.warn(`[main] core 意外退出 code=${code} signal=${signal}`);
    if (Date.now() - backendStartedAt > 60000) backendRestarts = 0;
    backendRestarts += 1;
    if (backendRestarts > 5) {
      notifyRendererBackendDown('后端连续崩溃，已停止自动重启。请查看日志或重启应用。');
      return;
    }
    const delay = Math.min(1000 * backendRestarts, 5000);
    notifyRendererBackendDown(`后端意外退出，${Math.round(delay / 1000)}s 后自动重启…`);
    setTimeout(() => { if (!isQuitting) startBackend(); }, delay);
  });
  backend.unref?.();
}

// 后端不可用时给渲染层一个可见提示（前端也有轮询兜底，这里只是更快）
function notifyRendererBackendDown(message) {
  try {
    if (mainWindow && !mainWindow.isDestroyed()) {
      mainWindow.webContents.send('backend-down', message);
    }
  } catch { /* 窗口未就绪时忽略 */ }
}

function killBackend() {
  isQuitting = true; // 主动终止不触发崩溃自动重启
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

function createWindow() {
  mainWindow = new BrowserWindow({
    width: 1240,
    height: 860,
    minWidth: 980,
    minHeight: 640,
    backgroundColor: '#ffffff',
    title: 'Star Trek Assistant',
    // 去掉系统标题栏，红黄绿按钮悬浮在内容上；窗口顶部由页面自行布局
    titleBarStyle: 'hiddenInset',
    trafficLightPosition: { x: 18, y: 13 },
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

// 单实例锁：双击图标/重复启动不再产生第二套窗口+后端（端口互抢、状态互踩）。
// 第二实例直接把已有窗口唤到前台。
if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on('second-instance', () => showMainWindow());
}

if (DEBUG_PORT) {
  app.commandLine.appendSwitch('remote-debugging-port', String(DEBUG_PORT));
}

app.whenReady().then(async () => {
  // 选择 Skill 文件夹：给渲染层提供原生目录选择器（不再让用户手敲绝对路径）
  ipcMain.handle('pick-skill-dir', async () => {
    const r = await dialog.showOpenDialog(mainWindow, {
      title: '选择 Skill 文件夹',
      buttonLabel: '选择此文件夹',
      properties: ['openDirectory'],
      message: '选择一个包含 SKILL.md / README.md 的 skill 文件夹',
    });
    return r.canceled ? null : (r.filePaths[0] || null);
  });
  // 放行媒体权限：唤醒/语音链路由渲染进程 Web Audio（getUserMedia）采集，
  // 必须在这里放行 media 权限请求，否则 getUserMedia 会静默失败、唤醒不触发
  // （音频采集在 Electron 渲染层，权限归属 GUI 应用本体）。
  session.defaultSession.setPermissionRequestHandler((_wc, permission, callback) => {
    callback(permission === 'media' || permission === 'mediaKeySystem');
  });
  // 先开窗再拉后端：窗口立即出现（前端自带"连接后端中"状态条），
  // 不再让用户面对最长 20s 的无窗口空等。
  createWindow();
  createTray(); // 启动即在菜单栏常驻图标
  ensureModels();
  startBackend();
  app.on('activate', () => showMainWindow());
});

// 窗口关闭不再退出：窗口仅隐藏，菜单栏图标继续常驻后台运行
// （真正退出走托盘「Quit」→ isQuitting=true → before-quit 清理后端）
app.on('window-all-closed', () => { /* 保持后台常驻，不退出 */ });
app.on('before-quit', () => { isQuitting = true; killBackend(); });
process.on('exit', killBackend);
process.on('SIGINT', () => { killBackend(); process.exit(0); });
