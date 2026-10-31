// 渲染进程主逻辑：导航、状态轮询、SSE 事件、各面板 CRUD。
// 对话流：用户气泡 / 助手气泡（内含工具调用卡片）+ 底部输入卡。
// 语音链路为纯 Rust 后端采集（cpal→KWS→STT→LLM→TTS），前端只做 UI 展示。
'use strict';

const $ = (sel) => document.querySelector(sel);
// 不依赖 preload：window.star 注入成功则用之，否则回退默认本地端口
const VOICE = (window.star && window.star.voiceUrl) || 'http://127.0.0.1:8420';
const CORE_URL = (window.star && window.star.coreUrl) || 'http://127.0.0.1:8410';
console.log(`[app] VOICE=${VOICE} core=${CORE_URL}`);

// 当前会话 id（切换会话时更新；null = 跟随后端 active）
let currentSessionId = null;
// 由 sendText 乐观渲染、尚未被 SSE user_text 消费的用户文本计数
// （同一文本连续发送多次时逐个消费，避免重复渲染用户气泡）
let pendingLocalUser = null;
const pendingLocalCount = new Map();
// 语音双发防护：记录最近到达的用户文本时间戳（5 秒窗口去重，仅对非打字路径生效）
const lastUserTextAt = new Map();
// 唤醒日志显隐开关（本地 UI 偏好，默认关闭）
function getKwsLogEnabled() {
  return localStorage.getItem('kws_log_enabled') === '1';
}

// 页面样式主题（lcars / classic）：本地 UI 偏好，加载即生效
function getUiTheme() {
  return localStorage.getItem('st_ui_theme') === 'lcars' ? 'lcars' : 'classic';
}
function applyUiTheme(theme) {
  const lcars = theme === 'lcars';
  document.body.classList.toggle('theme-lcars', lcars);
  localStorage.setItem('st_ui_theme', lcars ? 'lcars' : 'classic');
  const link = document.getElementById('lcars-css');
  if (link) link.disabled = !lcars;      // 原版 LCARS CSS 仅在 LCARS 主题下启用
  if (lcars) buildLcarsNav();
  relocateShell(lcars);
}

// 在两套外壳之间搬迁本项目视图/会话条（节点不重建，事件监听不丢）
function relocateShell(lcars) {
  const slot = document.getElementById('lcars-slot');
  const content = document.getElementById('content');
  const sidebar = document.getElementById('sidebar');
  const sessions = document.getElementById('sidebar-sessions');
  if (!slot || !content) return;
  const views = Array.from(content.children).filter((el) => el.classList.contains('view'));
  if (lcars) {
    const navc = document.getElementById('nav__container');
    if (sessions && navc) navc.appendChild(sessions);   // 会话列表放左侧栏下半段
    views.forEach((v) => slot.appendChild(v));           // 黑区只放各视图
  } else {
    views.forEach((v) => content.appendChild(v));
    if (sessions && sidebar) sidebar.appendChild(sessions);
  }
}
applyUiTheme(getUiTheme());

// 对话流事件仅渲染当前会话；session_id 为空 = 全局事件（语音），不拦截
function evForCurrentSession(ev) {
  if (!ev.session_id) return true;
  if (!currentSessionId) return true;
  return ev.session_id === currentSessionId;
}

// ---------------- 导航 ----------------
// 视图切换：经典侧栏(.nav-item) 与 LCARS 导航(.nav__link) 共用
function switchView(view, activeEl) {
  document.querySelectorAll('.nav-item, .nav__link').forEach((b) => b.classList.remove('active'));
  if (activeEl) activeEl.classList.add('active');
  document.querySelectorAll('.view').forEach((v) => v.classList.remove('active'));
  const target = document.getElementById('view-' + view);
  if (target) target.classList.add('active');
}
document.addEventListener('click', (e) => {
  const el = e.target.closest('.nav-item[data-view], .nav__link[data-view]');
  if (!el) return;
  if (el.tagName === 'A') e.preventDefault();
  switchView(el.dataset.view, el);
});

// LCARS 外壳导航：按原版结构生成编号胶囊（data-num），仅改文字
function buildLcarsNav() {
  const list = document.getElementById('nav-list');
  if (!list) return;
  const items = [
    { view: 'chat', label: '对话', num: '01' },
    { view: 'memory', label: '记忆', num: '02' },
    { view: 'skills', label: 'SKILLS', num: '03' },
    { view: 'schedules', label: '定时任务', num: '04' },
    { view: 'settings', label: '设置', num: '05' },
  ];
  list.innerHTML = items.map((b, i) => (
    '<li class="nav__entry"><a class="nav__link lc-' + (i + 1) + '" data-num="' + b.num +
    '" data-view="' + b.view + '" href="#">' + b.label + '</a></li>'
  )).join('');
  const active = document.querySelector('.view.active');
  const v = active ? active.id.replace(/^view-/, '') : 'chat';
  const link = list.querySelector('.nav__link[data-view="' + v + '"]');
  if (link) link.classList.add('active');
}

// ---------------- 状态轮询 ----------------
let connected = false;
let statusTimer = null;

// 发送按钮双态：空闲=箭头（发送），有任务=暂停（点击打断 AI）
let sendBusy = false;
let mainBusy = false;      // 后端 main_busy（轮询兜底）
let turnSessionId = null;  // 当前正在回复的会话 id（用于切换会话时判断）
let backendTurnSession = null; // 后端权威：当前主任务所属会话（/api/status turn_session）
let interruptPending = false; // 已请求打断、等待后端确认停止（避免按钮闪烁回"发送"）
let lastTurnUserText = ''; // 上一轮用户指令（失败节点"重试本轮"用）
let showReasoning = localStorage.getItem('st_show_reasoning') !== '0'; // 设置→显示 AI 思考过程
// 每个会话一份实时 turn 快照（per-session）：切走再切回时用它重建
// "AI 工作中"的思考卡/工具卡/已出文本（尚未持久化，历史里没有）；
// done 后保留最终文本，作为"切回时历史尚未刷新"的兜底补齐来源。
const turnSnapshots = new Map(); // sessionId -> { text, tools: Map(name->{name,inputs,ok,summary}), reasoning: [], done }

function snapFor(sessionId) {
  let s = turnSnapshots.get(sessionId);
  if (!s) { s = { text: '', tools: new Map(), reasoning: [], done: false, elapsedMs: 0 }; turnSnapshots.set(sessionId, s); }
  return s;
}
function cacheTurnReset(sessionId) {
  turnSnapshots.set(sessionId, { text: '', tools: new Map(), reasoning: [], done: false, elapsedMs: 0 });
}
function cacheTurnText(sessionId, t) { snapFor(sessionId).text = t; }
function cacheTurnReasoning(sessionId, t) { snapFor(sessionId).reasoning.push(t); }
function cacheTurnToolUse(sessionId, name, input) {
  const e = snapFor(sessionId).tools.get(name) || { name, inputs: [], ok: null, summary: '', dur: '', startTs: Date.now() };
  e.inputs.push(input);
  e.startTs = Date.now();
  snapFor(sessionId).tools.set(name, e);
}
function cacheTurnToolResult(sessionId, name, ok, summary) {
  const e = snapFor(sessionId).tools.get(name);
  if (e) {
    e.ok = ok;
    e.summary = summary;
    if (e.startTs) e.dur = ((Date.now() - e.startTs) / 1000).toFixed(1) + 's';
  }
}
function cacheTurnDone(sessionId, elapsedMs) { const s = snapFor(sessionId); s.done = true; if (elapsedMs) s.elapsedMs = elapsedMs; }

function setSendBtn(busy) {
  sendBusy = !!busy;
  const btn = $('#btn-send');
  if (!btn) return;
  const sendIco = btn.querySelector('.ico-send');
  const stopIco = btn.querySelector('.ico-stop');
  if (sendIco) sendIco.hidden = sendBusy;
  if (stopIco) stopIco.hidden = !sendBusy;
  btn.classList.toggle('busy', sendBusy);
  btn.classList.toggle('stopping', interruptPending && sendBusy);
  btn.title = sendBusy ? (interruptPending ? '正在停止…' : '暂停（打断 AI）') : '发送';
  btn.setAttribute('aria-label', btn.title);
  // 主流 AI 交互：空闲且输入为空时禁用；忙碌时必须保持可点击（用于打断）
  const empty = !$('#chat-input') || !$('#chat-input').value.trim();
  btn.disabled = !sendBusy && empty;
  btn.classList.toggle('empty', btn.disabled);
}

async function pollStatus() {
  try {
    const s = await apiGet('/api/status');
    connected = true;
    window.__backendState = s; // 后端/语音状态数据留存（顶栏胶囊已删，供后期加 UI 直接读取）
    $('#conn-banner').hidden = true;
    renderVoiceDiag(s);
    syncMicDot(s); // 顶栏"语音"胶囊以后端为权威，避免与真实监听状态脱节
    syncContinuousBtn(s); // 多轮对话按钮：仅监听开启时出现，状态以后端为权威
    mainBusy = s.main_status === 'working';
    backendTurnSession = s.turn_session || null;
    // 后端是权威状态：无条件跟随，避免 assistant_done 丢失/会话切换等边界
    // 导致暂停图标与后端真实状态不一致。暂停按钮仅当"任务属于当前会话"时亮起：
    // 切到其它会话时按钮回到发送态（不影响后端正在跑的任务）。
    if (mainBusy) {
      if (backendTurnSession) turnSessionId = backendTurnSession;
      else if (!turnSessionId) turnSessionId = currentSessionId;
      setSendBtn(turnSessionId === currentSessionId);
    } else {
      setSendBtn(false);
      turnSessionId = null;
      interruptPending = false;
    }
    // 唤醒/麦克风按钮指示灯：播报(蓝) > 主Agent工作(LCARS紫) > 唤醒后录音(黄) > 监听(绿) > 关(灰)
    const vd = s.voice_diag || {};
    if (s.speaking) setMicBtn('mic-speaking', '语音播报中（蓝色）');
    else if (mainBusy) setMicBtn('mic-working', '主 Agent 处理中（紫色）');
    else if (s.voice_active) setMicBtn('mic-active', '已唤醒·录音处理中（黄色）');
    else if (vd.listening) setMicBtn('mic-standby', '监听中（绿色）· 点击关闭');
    else setMicBtn('mic-off', '开启语音监听');
  } catch {
    connected = false;
    $('#conn-banner').hidden = false;
  }
}

// 语音链路诊断条：后端在线 + KWS 模型 + 常驻采集 + 麦克风权限状态。
// 设置里关闭语音（voice_enabled=false）时整条隐藏，不残留输入框上方。
// 输入框上方的诊断条已按要求移除；诊断状态改由顶栏「语音」胶囊展示
function renderVoiceDiag() { /* no-op */ }

// ---------------- 麦克风（后端常驻采集，前端仅控制监听开关） ----------------
function setMicState(cls, title) {
  const el = $('#mic-dot');
  if (!el) return;
  el.className = 'dot ' + cls;
  el.title = title || '';
}

// 顶栏"语音"胶囊已删；状态数据仍留存到 window.__voiceState 供后期调用。
function syncMicDot(s) {
  const d = s.voice_diag || {};
  // 后端启动时已自动开启监听：同步本地开关，避免首击按钮方向与实际相反
  if (window.voiceEnabled && s.voice_connected) micOn = !!d.listening;
  window.__voiceState = {
    voice_enabled: !!window.voiceEnabled,
    voice_connected: !!s.voice_connected,
    listening: !!d.listening,
    mic_alive: !!d.mic_alive,
    kws_ok: !!d.kws_ok,
  };
  const el = $('#mic-dot');
  if (!el) return;
  if (!window.voiceEnabled) { el.className = 'dot'; el.title = '语音已禁用（设置里开启）'; return; }
  if (!s.voice_connected) { el.className = 'dot off'; el.title = '语音服务未连接'; return; }
  if (d.listening && d.mic_alive) { el.className = 'dot on'; el.title = '语音监听中'; }
  else if (!d.mic_alive) { el.className = 'dot off'; el.title = '麦克风不可用'; }
  else if (!d.kws_ok) { el.className = 'dot off'; el.title = '唤醒模型未加载'; }
  else { el.className = 'dot warn'; el.title = '监听未开启'; }
}

// 监听按钮颜色状态指示灯：off(灰) / standby(绿) / err(红)
function setMicBtn(mode, title) {
  const btn = $('#btn-mic');
  if (!btn) return;
  btn.className = 'icon-btn ' + mode;
  btn.title = title || (mode === 'mic-off' ? '开启语音监听' : '关闭语音监听');
}

// 多轮对话模式按钮：仅当语音监听开启时才显示（hidden 时不占位，避免影响布局）。
// 是否点亮以后端 continuous_active 为权威，避免与真实模式状态脱节。
function syncContinuousBtn(s) {
  const btn = $('#btn-continuous');
  if (!btn) return;
  const visible = !!window.voiceEnabled && !!s.voice_connected && micOn;
  const on = visible && !!s.continuous_active;
  btn.hidden = !visible;
  btn.classList.toggle('on', on);
  btn.setAttribute('aria-pressed', on ? 'true' : 'false');
  btn.title = on
    ? '多轮对话模式已开启 · 点击结束'
    : '开启多轮对话模式（连续对话，无需重复唤醒）';
}

const mic = window.micEngine;
let micOn = false;

// 开启/关闭监听：后端（voice-serve 冷启动/瞬时抖动）失败自动重试，最多 ~5s，
// 保证按钮颜色与后端实际监听状态严格一致；全部失败才报错。
async function listeningWithRetry(enabled, payload, tries = 6) {
  for (let i = 0; i < tries; i++) {
    const resp = await apiPost('/api/voice/listening', payload).catch(() => null);
    if (resp && resp.ok) return true;
    await new Promise((r) => setTimeout(r, 800));
  }
  return false;
}

$('#btn-mic').addEventListener('click', async () => {
  if (!micOn) {
    // ①授权麦克风 + 复用/重建后端采集（失败则中止，不会虚报监听）
    await mic.start();
    if (mic.getState() === 'err') return;
    // ②开启后端监听（带重试）
    const ok = await listeningWithRetry(true, { enabled: true, session_id: currentSessionId });
    if (!ok) {
      mic.stop();
      setMicBtn('mic-err', '监听开启失败（语音服务不可用）');
      setMicState('off', '监听开启失败');
      return;
    }
    micOn = true;
    setMicState('on', '监听中：等待唤醒词');
    setMicBtn('mic-standby', '监听中（绿色）· 点击关闭');
  } else {
    const ok = await listeningWithRetry(false, { enabled: false });
    mic.stop();
    micOn = false;
    // 关闭以后端执行为准：关闭失败说明后端仍在监听，按钮保持绿色
    if (ok) {
      setMicBtn('mic-off', '开启语音监听');
      setMicState('off', '监听已停止');
    } else {
      setMicBtn('mic-standby', '关闭失败，后端仍在监听（绿色）');
      setMicState('on', '后端仍在监听');
    }
    voiceLine('');
  }
});

// 多轮对话模式开关：开启后无需再喊唤醒词，可连续对话；再次点击结束。
// 仅监听开启时按钮可见（后端也会校验），失败时按钮闪红并提示原因。
const convBtn = $('#btn-continuous');
if (convBtn) {
  convBtn.addEventListener('click', async () => {
    if (convBtn.hidden) return;
    const turningOn = !convBtn.classList.contains('on');
    convBtn.classList.remove('err');
    const resp = await apiPost('/api/voice/continuous', { enabled: turningOn }).catch(() => null);
    if (!resp || resp.ok !== true) {
      convBtn.classList.remove('on');
      convBtn.classList.add('err');
      convBtn.title = (resp && resp.error) ? resp.error : '操作失败：语音服务不可用';
      setTimeout(() => convBtn.classList.remove('err'), 1400);
      return;
    }
    const on = !!resp.continuous_active;
    convBtn.classList.toggle('on', on);
    convBtn.setAttribute('aria-pressed', on ? 'true' : 'false');
    convBtn.title = on
      ? '多轮对话模式已开启 · 点击结束'
      : '开启多轮对话模式（连续对话，无需重复唤醒）';
  });
}

mic.on('state', ({ state }) => {
  if (state === 'err') {
    setMicState('off', '麦克风不可用');
    setMicBtn('mic-err', '麦克风不可用');
  } else if (state === 'idle') {
    setMicBtn('mic-off', '开启语音监听');
  }
  // standby 状态由按钮 handler 直接设置，避免重复
});

mic.on('mic_error', ({ message }) => {
  setMicState('off', '麦克风不可用');
  setMicBtn('mic-err', '麦克风不可用');
  // 顶部「语音」胶囊 title 给出原因，不再显示输入框上方诊断条
  const dot = $('#mic-dot');
  if (dot) dot.title = message || '麦克风不可用';
});

// ---------------- 会话系统（切换 / 新建 / 删除对话） ----------------
const sessionList = $('#session-list');

async function loadSessions() {
  try {
    const j = await apiGet('/api/sessions');
    const activeId = j.active;
    const list = j.sessions || [];
    if (list.length === 0) {
      // 无任何会话则新建一个
      const n = await apiPost('/api/sessions', {});
      currentSessionId = n.id;
      renderSessionList([n], n.id);
      renderChat([]);
      return;
    }
    // 前后端完全绑定：后端 active 是权威，前端始终跟随，
    // 避免切换/新建/删除后前端保留过期 id 与后端脱节。
    // 仅在 active 缺失或不在列表时，才回退到前端当前值/列表第一个。
    if (activeId && list.some((s) => s.id === activeId)) {
      currentSessionId = activeId;
    } else if (!currentSessionId || !list.some((s) => s.id === currentSessionId)) {
      currentSessionId = list[0].id;
    }
    renderSessionList(list, currentSessionId);
    loadMessages(currentSessionId);
  } catch {
    sessionList.innerHTML = '<div class="session-empty">会话服务不可用</div>';
  }
}

function renderSessionList(list, activeId) {
  if (!list.length) {
    sessionList.innerHTML = '<div class="session-empty">暂无对话，点击右上角新建</div>';
    return;
  }
  sessionList.innerHTML = '';
  list.forEach((s) => {
    const item = document.createElement('div');
    item.className = 'session-item' + (s.id === activeId ? ' active' : '');
    item.innerHTML = `
      <span class="s-ico"></span>
      <span class="s-name" title="${esc(s.title)}">${esc(s.title)}</span>
      <button class="s-menu-btn" title="会话菜单" aria-label="会话菜单">⋮</button>
      <div class="session-pop" hidden>
        <button data-act="switch">切换到此对话</button>
        <button data-act="del" class="danger">删除对话</button>
      </div>`;
    // 点击条目 = 切换会话
    item.addEventListener('click', (e) => {
      if (e.target.closest('.s-menu-btn') || e.target.closest('.session-pop')) return;
      closeAllPops();
      switchSession(s.id);
    });
    // ⋮ 弹出菜单（切换 / 删除）
    const menuBtn = item.querySelector('.s-menu-btn');
    const pop = item.querySelector('.session-pop');
    menuBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      const willOpen = pop.hidden;
      closeAllPops();
      pop.hidden = !willOpen;
    });
    pop.addEventListener('click', (e) => {
      e.stopPropagation();
      const act = e.target.dataset.act;
      pop.hidden = true;
      if (act === 'switch') switchSession(s.id);
      else if (act === 'del') deleteSession(s.id);
    });
    sessionList.appendChild(item);
  });
}

function closeAllPops() {
  sessionList.querySelectorAll('.session-pop').forEach((p) => { p.hidden = true; });
}
document.addEventListener('click', closeAllPops);

async function deleteSession(id) {
  if (!confirm('删除该对话？')) return;
  try {
    await apiDelete(`/api/sessions/${encodeURIComponent(id)}`);
    turnSnapshots.delete(id);
    if (currentSessionId === id) currentSessionId = null;
    await loadSessions();
  } catch (e) { alert('删除失败: ' + e.message); }
}

// 监听开启时，把语音对话目标同步到当前会话（防止语音对话落到别的会话）
function syncVoiceSession() {
  if (micOn && currentSessionId) {
    apiPost('/api/voice/listening', { enabled: true, session_id: currentSessionId }).catch(() => {});
  }
}

async function switchSession(id) {
  try {
    // 后端确认后才更新前端状态：切换失败（如会话不存在/后端异常）
    // 必须保留原会话并明确提示，避免前端显示与后端 active 脱节。
    const j = await apiPost(`/api/sessions/${id}/switch`, {});
    if (!j || j.ok === false) {
      addSystem((j && j.error) || '切换会话失败：后端未确认（会话可能已被删除）');
      return;
    }
    currentSessionId = id;
    pendingLocalUser = null;
    await loadMessages(id);
    await loadSessions(); // 刷新 active 高亮（以后端 active 为准）
    syncVoiceSession();
    // 切回正在进行回复的会话：以 /api/status 的 turn_session + main_status
    // 为权威恢复暂停按钮（renderChat 已用 per-session 快照重建转圈/工具卡）。
    // 先同步后端状态再判定，避免依赖过期的前端缓存。
    await pollStatus();
  } catch (e) {
    addSystem('切换会话失败: ' + e.message);
  }
}

async function newSession() {
  try {
    await apiPost('/api/sessions', {});
    currentSessionId = null; // 强制 loadSessions 取后端 active（新建会话）
    pendingLocalUser = null;
    await loadSessions();
    $('#chat-input').value = '';
    $('#chat-input').focus();
    syncVoiceSession();
    setSendBtn(false); // 新会话无任务
  } catch (e) {
    addSystem('新建会话失败: ' + e.message);
  }
}

async function loadMessages(id) {
  try {
    const j = await apiGet(`/api/sessions/${id}/messages`);
    if (!j || !j.ok) throw new Error((j && j.error) || '消息加载失败');
    renderChat(j.messages || []);
  } catch (e) {
    // 加载失败时给出可见反馈，而不是残留上一个会话的内容造成误导
    renderChat(null, '会话消息加载失败：' + e.message);
  }
}

// 渲染整个对话流（会话历史 / 切换时重建）
function renderChat(msgs, errText) {
  const chat = $('#chat-scroll');
  chat.innerHTML = '';
  currentTurn = null;
  resetReasoning();
  resetDock();
  processingEl = null; // innerHTML 已清空占位节点，同步重置引用
  if (errText) {
    const empty = document.createElement('div');
    empty.className = 'msg system';
    empty.textContent = '⚠ ' + errText;
    chat.appendChild(empty);
    return;
  }
  if (!msgs || msgs.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'msg system';
    empty.textContent = '新的对话。输入指令，或开启监听后说唤醒词「computer」开始语音交互。';
    chat.appendChild(empty);
    return;
  }
  msgs.forEach((m) => {
    if (m.role === 'user') {
      const el = createMsg(m.text);
      el.classList.add('user');
      chat.appendChild(el);
    } else if (m.role === 'assistant') {
      const div = document.createElement('div');
      div.className = 'msg assistant';
      const records = m.tools || [];
      // 历史同样走时间线：工具节点 → 回答节点 → 交付物节点
      const t = buildAssistantTurn();
      records.forEach((r) => {
        const refs = makeToolCard(r);
        t.tl.insertBefore(refs.node, t.answerNode);
      });
      if (records.length) t.tl.classList.add('has-line');
      else t.tl.classList.add('is-plain');
      if (m.text) {
        t.answerNode.hidden = false;
        t.bubble.innerHTML = renderMd(m.text);
        tlSetState(t.answerNode, 'completed');
      }
      const dv = deliverFromRecords(records);
      if (dv) {
        const card = buildDeliverCard(dv);
        if (card) { t.reportNode.hidden = false; t.deliverBox.appendChild(card); }
      }
      t.block.statusRow.hidden = !records.length;
      if (records.length) t.block.statusRow.textContent = '已完成 · 用时 ' + fmtDuration(m.elapsed_ms);
      div.appendChild(t.tl);
      chat.appendChild(div);
    }
  });
  // 切回会话时用 per-session 快照重建"AI 工作中"实时状态
  const snap = turnSnapshots.get(currentSessionId);
  if (snap) {
    if (!snap.done) {
      // 仍在进行：重建转圈/工具卡/已出文本（尚未持久化，历史里没有）
      renderLiveTurnBlock(snap);
    } else if (snap.text) {
      // 已完成：历史应包含最终文本（后端先落盘再广播 done）。
      // 兜底：若历史缺该条（切换竞态），用快照补齐，避免"做完却没显示"。
      const lastAsst = [...msgs].reverse().find((m) => m.role === 'assistant');
      if (!lastAsst || (lastAsst.text || '').trim() !== snap.text.trim()) {
        const div = document.createElement('div');
        div.className = 'msg assistant';
        const t = buildAssistantTurn();
        snap.tools.forEach((e) => {
          const refs = makeToolNode(toolTitleFromInputs(e.name, e.inputs), inputHtmlOf(e.name, e.inputs));
          t.tl.insertBefore(refs.node, t.answerNode);
          if (e.ok !== null) refs.finish(e.ok, e.summary || '', e.dur || null);
        });
        if (snap.tools.size) t.tl.classList.add('has-line');
        else t.tl.classList.add('is-plain');
        t.answerNode.hidden = false;
        t.bubble.innerHTML = renderMd(snap.text);
        tlSetState(t.answerNode, 'completed');
        div.appendChild(t.tl);
        chat.appendChild(div);
      }
    }
  }
  chat.scrollTop = chat.scrollHeight;
}

// ════════ AI 执行流节点（thought / action / observation / answer / report） ════════
// 风格：白底直排灰字 + 折叠箭头；运行中主色 #2563EB，完成后绿点；不套卡片不画竖线。
const TL_IC = {
  chev: '<svg class="tl-chev" viewBox="0 0 12 12" aria-hidden="true"><path d="M4 2.5 8 6 4 9.5"/></svg>',
  spin: '<svg class="tl-ic tl-ic-spin" viewBox="0 0 14 14" aria-hidden="true"><circle cx="7" cy="7" r="5.2" stroke="#E5E7EB" stroke-width="1.6" fill="none"/><path d="M12.2 7a5.2 5.2 0 0 0-5.2-5.2" stroke="#2563EB" stroke-width="1.6" stroke-linecap="round" fill="none"/></svg>',
  check: '<svg class="tl-ic tl-ic-check" viewBox="0 0 14 14" aria-hidden="true"><path d="M3 7.2 6 10l5.5-5.8"/></svg>',
  fail: '<svg class="tl-ic tl-ic-fail" viewBox="0 0 14 14" aria-hidden="true"><path d="M3.5 3.5 10.5 10.5M10.5 3.5 3.5 10.5"/></svg>',
  dotOk: '<span class="tl-dotok"></span>',
  dotFail: '<span class="tl-dotfail"></span>',
};

// 节点骨架：单块内容（无左侧点/竖线）
function tlNodeEl(kind, state) {
  const node = document.createElement('div');
  node.className = 'tl-node tl-' + kind + ' is-' + (state || 'pending');
  const body = document.createElement('div');
  body.className = 'tl-body';
  node.appendChild(body);
  return { node, body };
}

// 状态切换：running（主色 + spinner）/ completed（绿点）/ failed（红）
function tlSetState(node, state) {
  if (!node) return;
  node.classList.remove('is-pending', 'is-running', 'is-completed', 'is-failed');
  node.classList.add('is-' + state);
}

// 思考块：运行中显示 spinner + "AI 正在思考"；完成后自动收起为"已完成思考 · N 步推理"
function makeThoughtNode() {
  const { node, body } = tlNodeEl('thought', 'running');
  node.hidden = true;
  const head = document.createElement('button');
  head.type = 'button';
  head.className = 'tl-head tl-thought-head';
  head.setAttribute('aria-expanded', 'false');
  const ico = document.createElement('span');
  ico.innerHTML = TL_IC.spin;
  const title = document.createElement('span');
  title.className = 'tl-title';
  title.textContent = 'AI 正在思考';
  const chev = document.createElement('span');
  chev.innerHTML = TL_IC.chev;
  head.appendChild(ico);
  head.appendChild(title);
  head.appendChild(chev);
  const fold = document.createElement('div');
  fold.className = 'tl-fold';
  const textEl = document.createElement('div');
  textEl.className = 'tl-thought-text';
  fold.appendChild(textEl);
  body.appendChild(head);
  body.appendChild(fold);
  let open = false;
  let steps = 0;
  let finished = false;
  head.addEventListener('click', () => {
    if (!finished) return; // 思考中不可手动切换（由自动展开控制）
    open = !open;
    node.classList.toggle('is-open', open);
    head.setAttribute('aria-expanded', String(open));
  });
  return {
    node, body,
    get steps() { return steps; },
    add(chunk) {
      if (!chunk) return;
      node.hidden = false;
      if (finished) {
        finished = false;
        tlSetState(node, 'running');
        ico.innerHTML = TL_IC.spin;
        title.textContent = 'AI 正在思考';
      }
      steps += 1;
      // 思考到来时自动展开，让用户看到流式推理过程
      if (!open) {
        open = true;
        node.classList.add('is-open');
        head.setAttribute('aria-expanded', 'true');
      }
    },
    flush(text) {
      textEl.textContent = text;
      // 运行中保持展开并自动滚到底
      if (open) {
        requestAnimationFrame(() => {
          fold.scrollTop = fold.scrollHeight;
        });
      }
    },
    finish() {
      if (node.hidden) return;
      finished = true;
      title.textContent = steps > 0 ? '已完成思考 · ' + steps + ' 步推理' : '已完成思考';
      ico.innerHTML = TL_IC.check;
      tlSetState(node, 'completed');
      // 完成后自动折叠思考内容
      open = false;
      node.classList.remove('is-open');
      head.setAttribute('aria-expanded', 'false');
    },
  };
}

function inputHtmlOf(name, inputs) {
  const arr = Array.isArray(inputs) ? inputs : (inputs ? [inputs] : []);
  return arr.map((i) => (name === 'SpeakToUser' ? esc((i && i.text) || jsonHtml(i)) : jsonHtml(i))).join('<hr/>');
}

// 工具调用条目：一行灰字(箭头 + 名称 + 状态)，点击展开输入/结果，完成后出 ↳ 摘要
function makeToolNode(title, inputHtml) {
  const { node, body } = tlNodeEl('tool', 'running');
  const head = document.createElement('button');
  head.type = 'button';
  head.className = 'tl-head tl-tool-head';
  const chev = document.createElement('span');
  chev.innerHTML = TL_IC.chev;
  const nameEl = document.createElement('span');
  nameEl.className = 'tl-tool-name';
  nameEl.textContent = title;
  const spacer = document.createElement('span');
  spacer.className = 'tl-tool-spacer';
  const stateEl = document.createElement('span');
  stateEl.className = 'tl-tool-state';
  stateEl.innerHTML = TL_IC.spin + '执行中';
  head.appendChild(chev);
  head.appendChild(nameEl);
  head.appendChild(spacer);
  head.appendChild(stateEl);

  const fold = document.createElement('div');
  fold.className = 'tl-fold';
  const detail = document.createElement('div');
  detail.className = 'tl-detail';

  const argsRow = document.createElement('div');
  argsRow.className = 'tl-detail__row';
  const argsLabel = document.createElement('span');
  argsLabel.className = 'tl-detail__label';
  argsLabel.textContent = '输入参数';
  const argsEl = document.createElement('pre');
  argsEl.className = 'tl-json';
  argsEl.innerHTML = inputHtml || '';
  if (!inputHtml) argsRow.hidden = true;
  argsRow.appendChild(argsLabel);
  argsRow.appendChild(argsEl);

  const resRow = document.createElement('div');
  resRow.className = 'tl-detail__row';
  const resLabel = document.createElement('span');
  resLabel.className = 'tl-detail__label';
  resLabel.textContent = '返回结果';
  const resultEl = document.createElement('p');
  resultEl.className = 'tl-result';
  resultEl.innerHTML = '<span class="tl-pending">执行中…</span>';
  resRow.appendChild(resLabel);
  resRow.appendChild(resultEl);

  const errRow = document.createElement('div');
  errRow.className = 'tl-detail__row';
  errRow.hidden = true;
  const errEl = document.createElement('p');
  errEl.className = 'tl-error';
  errRow.appendChild(errEl);

  const meta = document.createElement('div');
  meta.className = 'tl-meta';
  const metaDur = document.createElement('span');
  metaDur.textContent = '耗时 —';
  const copyBtn = document.createElement('button');
  copyBtn.type = 'button';
  copyBtn.className = 'tl-meta__btn';
  copyBtn.textContent = '复制结果';
  copyBtn.addEventListener('click', () => {
    const text = title + '\n' + (argsEl.textContent || '') + '\n→ ' + (resultEl.textContent || '');
    copyPlain(text, copyBtn);
  });
  const retryBtn = document.createElement('button');
  retryBtn.type = 'button';
  retryBtn.className = 'tl-meta__btn tl-meta__retry';
  retryBtn.textContent = '重试本轮';
  retryBtn.hidden = true;
  retryBtn.addEventListener('click', () => retryLastTurn());
  meta.appendChild(metaDur);
  meta.appendChild(copyBtn);
  meta.appendChild(retryBtn);

  detail.appendChild(argsRow);
  detail.appendChild(resRow);
  detail.appendChild(errRow);
  detail.appendChild(meta);
  fold.appendChild(detail);

  // observation：工具返回后的一行 ↳ 摘要（完成才出现）
  const obs = document.createElement('div');
  obs.className = 'tl-obs';
  obs.hidden = true;
  obs.innerHTML = '<span class="ar">↳</span><span class="tx"></span>';
  const obsTx = obs.querySelector('.tx');

  body.appendChild(head);
  body.appendChild(fold);
  body.appendChild(obs);

  head.addEventListener('click', () => node.classList.toggle('is-open'));

  return {
    node, head, nameEl, stateEl, argsEl, resultEl, errRow, errEl, metaDur, retryBtn,
    obs, obsTx,
    inputs: [],
    startTs: Date.now(),
    finish(ok, summaryText, durText) {
      tlSetState(node, ok ? 'completed' : 'failed');
      const s = (summaryText || '').trim();
      if (ok) {
        stateEl.innerHTML = TL_IC.dotOk + '已完成 ' + (durText || '');
      } else {
        stateEl.innerHTML = TL_IC.dotFail + '失败 ' + (durText || '');
      }
      resultEl.innerHTML = s ? esc(s) : '<span class="tl-pending">（无结果）</span>';
      errRow.hidden = !!ok;
      errEl.textContent = ok ? '' : (s || '工具执行失败');
      if (ok && s) {
        obs.hidden = false;
        obsTx.textContent = s;
      } else {
        obs.hidden = true;
      }
      metaDur.textContent = '耗时 ' + (durText || '—');
      retryBtn.hidden = !!ok;
    },
    setRunning() {
      tlSetState(node, 'running');
      stateEl.innerHTML = TL_IC.spin + '执行中';
      resultEl.innerHTML = '<span class="tl-pending">执行中…</span>';
      errRow.hidden = true;
      obs.hidden = true;
      retryBtn.hidden = true;
    },
  };
}

// 复制纯文本（工具结果 / 最终回答共用的轻量动作）
function copyPlain(text, btn) {
  if (!text) return;
  if (navigator.clipboard) navigator.clipboard.writeText(text).catch(() => {});
  const old = btn.textContent;
  btn.textContent = '已复制';
  setTimeout(() => { btn.textContent = old; }, 1400);
}

// 失败节点的重试入口：当前后端没有工具级重试接口，重发上一轮用户指令是唯一真实能力
function retryLastTurn() {
  if (sendBusy || !lastTurnUserText) return;
  sendText(lastTurnUserText);
}

// 一轮助手回复的时间线骨架：思考 → 工具… → 回答 → 交付物 → 完成摘要
function buildAssistantTurn() {
  const tl = document.createElement('div');
  tl.className = 'tl';
  const thought = makeThoughtNode();
  tl.appendChild(thought.node);

  const ans = tlNodeEl('answer', 'running');
  ans.node.hidden = true;
  const bubble = document.createElement('div');
  bubble.className = 'bubble';
  const actions = document.createElement('div');
  actions.className = 'tl-answer-actions';
  const copyBtn = document.createElement('button');
  copyBtn.type = 'button';
  copyBtn.className = 'tl-action';
  copyBtn.textContent = '复制';
  copyBtn.addEventListener('click', () => copyPlain(bubble.textContent || '', copyBtn));
  actions.appendChild(copyBtn);
  ans.body.appendChild(bubble);
  ans.body.appendChild(actions);
  tl.appendChild(ans.node);

  const report = tlNodeEl('report', 'completed');
  report.node.hidden = true;
  const reportLabel = document.createElement('div');
  reportLabel.className = 'tl-report-label';
  reportLabel.innerHTML = '<span class="ln"></span>' +
    '<svg viewBox="0 0 14 14" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M8.2 1.5H4.5a1.3 1.3 0 0 0-1.3 1.3v8.4A1.3 1.3 0 0 0 4.5 12.5h5a1.3 1.3 0 0 0 1.3-1.3V5z"/><path d="M8.2 1.5V5h3.5"/></svg>' +
    '<span>已生成产物</span>';
  report.body.appendChild(reportLabel);
  const deliverBox = document.createElement('div');
  deliverBox.className = 'db-deliver-wrap';
  report.body.appendChild(deliverBox);
  tl.appendChild(report.node);

  const foot = document.createElement('div');
  foot.className = 'tl-foot';
  foot.hidden = true;
  tl.appendChild(foot);

  const block = { stack: tl, statusRow: foot, thought };
  return {
    tl, block, thought,
    answerNode: ans.node, answerBody: ans.body, bubble, answerActions: actions,
    reportNode: report.node, deliverBox,
  };
}

// 用缓存重建进行中的助手消息块（切会话回来时调用）
function renderLiveTurnBlock(cache) {
  const chat = $('#chat-scroll');
  // 尚未产出任何内容（思考/文本/工具都没有）：沿用"正在思考"占位
  if (!cache.text && !cache.reasoning.length && cache.tools.size === 0) {
    showProcessing();
    return;
  }
  const div = document.createElement('div');
  div.className = 'msg assistant';
  const t = buildAssistantTurn();
  div.appendChild(t.tl);
  const reasoningText = cache.reasoning.join('');
  if (showReasoning && reasoningText) {
    cache.reasoning.forEach((r) => t.thought.add(r));
    t.thought.flush(reasoningText);
  }
  const toolMap = new Map();
  let lastLabel = '';
  cache.tools.forEach((e) => {
    const refs = makeToolNode(toolTitleFromInputs(e.name, e.inputs), inputHtmlOf(e.name, e.inputs));
    refs.inputs = e.inputs.slice();
    t.tl.insertBefore(refs.node, t.answerNode);
    if (e.ok === null) {
      lastLabel = toolTitleFromInputs(e.name, e.inputs);
      runningTools.add(refs);
      startToolTicker();
    } else {
      refs.finish(e.ok, e.summary || '', e.dur || null);
    }
    toolMap.set(e.name, refs);
  });
  if (reasoningText || cache.tools.size) t.tl.classList.add('has-line');
  if (cache.text) {
    t.answerNode.hidden = false;
    t.bubble.innerHTML = renderMd(cache.text);
  }
  chat.appendChild(div);
  currentTurn = {
    msg: div, text: t.bubble, tl: t.tl, block: t.block, detail: t.tl, toolMap,
    deliverBox: t.deliverBox, deliver: deliverFromRecords([...cache.tools.values()]),
    startTs: Date.now(), answerNode: t.answerNode,
    thought: t.thought, reportNode: t.reportNode,
  };
  if (cache.done) {
    finalizeTurn(currentTurn, cache.elapsedMs);
  } else {
    if (cache.tools.size > 0) t.thought.finish();
    setBlockRunning(t.block, lastLabel || '处理');
  }
}

// ---------------- 对话（用户/助手气泡 + 工具调用卡片） ----------------
const chatScroll = $('#chat-scroll');

// 当前助手消息容器：{ msg, text, tools, toolMap }
let currentTurn = null;

function scrollChat() { chatScroll.scrollTop = chatScroll.scrollHeight; }

function addUserBubble(text) {
  const div = document.createElement('div');
  div.className = 'msg user';
  const inner = document.createElement('div');
  inner.className = 'bubble';
  inner.textContent = text;
  div.appendChild(inner);
  chatScroll.appendChild(div);
  scrollChat();
}

function addSystem(text) {
  const div = document.createElement('div');
  div.className = 'msg system';
  div.textContent = text;
  chatScroll.appendChild(div);
  scrollChat();
}

// ---------------- AI 运行中：紧凑思考 dock（只占一条，展开仅显示思考过程） ----------------
const aiDock = $('#ai-dock');
const aiDockHead = $('#ai-dock-head');
const aiDockLabel = $('#ai-dock-label');
const aiDockHint = $('#ai-dock-hint');
const aiDockBody = $('#ai-dock-reasoning');
let dockReasoningBuf = '';
let dockTimer = null;
let dockExpanded = false;

function resetDock() {
  clearTimeout(dockTimer);
  dockReasoningBuf = '';
  dockExpanded = false;
  if (aiDock) {
    aiDock.hidden = true;
    aiDock.classList.remove('is-open');
  }
  if (aiDockBody) aiDockBody.textContent = '';
  if (aiDockHint) aiDockHint.textContent = '';
  if (aiDockHead) aiDockHead.setAttribute('aria-expanded', 'false');
}

function showDock(label) {
  if (!aiDock) return;
  if (aiDock.hidden) {
    dockReasoningBuf = '';
    if (aiDockBody) aiDockBody.textContent = '';
    if (aiDockHint) aiDockHint.textContent = '';
  }
  aiDock.hidden = false;
  if (label && aiDockLabel) aiDockLabel.textContent = label;
}

function setDockLabel(label) {
  if (aiDock && !aiDock.hidden && aiDockLabel && label) aiDockLabel.textContent = label;
}

function appendDockReasoning(text) {
  if (!aiDock) return;
  showDock(aiDockLabel ? aiDockLabel.textContent : '正在思考');
  dockReasoningBuf += text;
  clearTimeout(dockTimer);
  dockTimer = setTimeout(() => {
    if (aiDock.hidden) return;
    if (aiDockBody) {
      aiDockBody.textContent = dockReasoningBuf;
      aiDockBody.scrollTop = aiDockBody.scrollHeight;
    }
    const tail = dockReasoningBuf.replace(/\s+/g, ' ').trim().slice(-70);
    if (aiDockHint) aiDockHint.textContent = tail;
  }, 120);
}

if (aiDockHead) {
  aiDockHead.addEventListener('click', () => {
    dockExpanded = !dockExpanded;
    aiDock.classList.toggle('is-open', dockExpanded);
    aiDockHead.setAttribute('aria-expanded', String(dockExpanded));
    if (dockExpanded && aiDockBody) {
      aiDockBody.textContent = dockReasoningBuf;
      aiDockBody.scrollTop = aiDockBody.scrollHeight;
    }
  });
}

// "处理中"占位（豆包式：一行"正在思考" + 三点跳动动画；首个事件到达即替换）
let processingEl = null;
function showProcessing() {
  hideProcessing();
  const div = document.createElement('div');
  div.className = 'msg assistant processing';
  const inner = document.createElement('div');
  inner.className = 'processing-line';
  inner.innerHTML = '<span class="processing-text">正在思考</span><span class="db-dots"><i></i><i></i><i></i></span>';
  div.appendChild(inner);
  chatScroll.appendChild(div);
  processingEl = div;
  scrollChat();
}
function hideProcessing() {
  if (processingEl) {
    processingEl.remove();
    processingEl = null;
  }
}

// 创建或复用当前助手时间线：思考 → 工具 → 回答 → 交付物
function ensureTurn() {
  if (currentTurn) return currentTurn;
  hideProcessing();
  const msg = document.createElement('div');
  msg.className = 'msg assistant';
  const t = buildAssistantTurn();
  msg.appendChild(t.tl);
  chatScroll.appendChild(msg);
  currentTurn = {
    msg,
    text: t.bubble,
    tl: t.tl,
    block: t.block,
    detail: t.tl,
    toolMap: new Map(),
    deliverBox: t.deliverBox,
    deliver: null,
    startTs: Date.now(),
    answerNode: t.answerNode,
    thought: t.thought,
    reportNode: t.reportNode,
  };
  scrollChat();
  return currentTurn;
}

function finishTurn(elapsedMs) {
  if (currentTurn) finalizeTurn(currentTurn, elapsedMs);
  currentTurn = null;
  resetReasoning();
  resetDock();
}

// 本轮收尾：状态行变"已完成，用时 X 分 X 秒"，明细保持收起，渲染交付卡片。
// 无论实时路径还是历史重建路径都走这里，保证"切走再回来"这行不会消失。
function finalizeTurn(turn, elapsedMs) {
  if (!turn || !turn.block) return;
  const thinking = !!(turn.thought && !turn.thought.node.hidden);
  const ms = (elapsedMs && elapsedMs > 0)
    ? elapsedMs
    : (turn.startTs ? Date.now() - turn.startTs : 0);
  const hasActivity = (turn.toolMap && turn.toolMap.size > 0) || thinking;
  if (turn.thought) turn.thought.finish();
  // 任务完成后：所有工具调用卡片自动折叠，只留标题行 + ↳ 摘要
  if (turn.toolMap) {
    for (const tn of turn.toolMap.values()) {
      if (tn && tn.node) tn.node.classList.remove('is-open');
    }
  }
  if (!hasActivity) turn.tl.classList.add('is-plain');
  finalizeBlock(turn.block, ms, hasActivity);
  if (turn.answerNode && !turn.answerNode.hidden) {
    tlSetState(turn.answerNode, 'completed');
  }
  turn.done = true;
  renderDeliver(turn);
}

function renderDeliver(turn) {
  if (!turn || !turn.deliverBox || !turn.deliver) return;
  if (turn.deliverBox.firstChild) return;
  const card = buildDeliverCard(turn.deliver);
  if (!card) return;
  turn.deliverBox.appendChild(card);
  if (turn.reportNode) turn.reportNode.hidden = false;
}

// ---------------- AI 中间思考过程（时间线内的思考节点，同轮累积） ----------------
let reasoningBuf = '';
let reasoningTimer = null;

function addReasoningCard(text) {
  const turn = ensureTurn();
  if (!turn || !turn.thought) return;
  reasoningBuf += text;
  turn.thought.add(text);
  turn.tl.classList.remove('is-plain');
  turn.tl.classList.add('has-line');
  clearTimeout(reasoningTimer);
  reasoningTimer = setTimeout(() => {
    if (turn.thought) turn.thought.flush(reasoningBuf);
    scrollChat();
  }, 40);
  scrollChat();
}

function resetReasoning() {
  clearTimeout(reasoningTimer);
  reasoningBuf = '';
}

// ════════ 豆包式工具调用行（严格复刻 doubao-work-html：db-step 非卡片结构、
// 纯文字标题 + 尾部折角箭头、执行中"光线波动"、展开 = 输入参数 + 结果 + 元信息） ════════

// JSON 着色：键=深灰，字符串=浅灰（与 doubao db-json__k/db-json__s 一致）
function jsonHtml(data) {
  let text;
  try { text = typeof data === 'string' ? data : JSON.stringify(data, null, 2); }
  catch { text = String(data); }
  const re = /"((?:\\.|[^"\\])*)"(\s*:)?/g;
  let out = '';
  let last = 0;
  let m;
  while ((m = re.exec(text))) {
    if (m.index > last) out += esc(text.slice(last, m.index));
    out += m[2]
      ? '<span class="tl-json__k">"' + esc(m[1]) + '"' + m[2] + '</span>'
      : '<span class="tl-json__s">"' + esc(m[1]) + '"</span>';
    last = re.lastIndex;
  }
  if (last < text.length) out += esc(text.slice(last));
  return out;
}

// 工具用途映射：常见内置工具 → 中文用途（豆包式标题：不显示工具名，显示"它在干嘛"）
const TOOL_LABELS = {
  'WebSearch': '搜索网页',
  'WebFetch': '抓取网页内容',
  'Bash': '执行终端命令',
  'bash': '执行终端命令',
  'Read': '读取文件',
  'read': '读取文件',
  'Write': '写入文件',
  'write': '写入文件',
  'Edit': '修改文件',
  'Glob': '查找文件',
  'Grep': '搜索文件内容',
  'Python': '运行 Python 代码',
  'TextEditor': '编辑文本',
  'SearchPin': '联网搜索',
  'searchpin': '联网搜索',
  'AskUserQuestion': 'ask user question',
  'DeliverFiles': '登记交付物',
};

// 工具卡标题优先级：AskUserQuestion 保留原名 → 内置用途映射 →
// 输入参数里的 description 说明 → 工具名回退。
// （工具调用时 input 常自带 "description" 字段，如 "List netease-music skill directory"）
function toolTitleFromInputs(name, inputs) {
  if (name === 'SpeakToUser') return '语音播报';
  if (name === 'AskUserQuestion') return 'ask user question';
  if (TOOL_LABELS[name]) return TOOL_LABELS[name];
  const arr = Array.isArray(inputs) ? inputs : (inputs ? [inputs] : []);
  for (const i of arr) {
    if (i && typeof i === 'object' && typeof i.description === 'string' && i.description.trim()) {
      return i.description.trim();
    }
  }
  return name;
}

// ════════ 工具区：时间线节点 + 可展开明细 + 交付卡片 ════════

// 用时格式化：<60s → "N 秒"，否则 "M 分 K 秒"
function fmtDuration(ms) {
  const sec = Math.max(1, Math.round((ms || 0) / 1000));
  if (sec < 60) return sec + ' 秒';
  const m = Math.floor(sec / 60);
  const r = sec % 60;
  return r ? (m + ' 分 ' + r + ' 秒') : (m + ' 分');
}

// 执行中：不额外占用一行，运行态由节点自身的蓝点脉冲表达
function setBlockRunning(block, label) {
  if (!block || !block.statusRow) return;
  block.statusRow.hidden = true;
  block.runningLabel = label || '';
}

// 结束：时间线底部留一行很浅的完成摘要；本轮无任何活动则整行隐藏
function finalizeBlock(block, elapsedMs, hasActivity) {
  if (!block || !block.statusRow) return;
  block.statusRow.hidden = !hasActivity;
  if (hasActivity) block.statusRow.textContent = '已完成 · 用时 ' + fmtDuration(elapsedMs);
}

// 从工具记录里取 DeliverFiles 的输入（AI 主动交付清单）
function deliverFromRecords(records) {
  if (!records) return null;
  for (const t of records) {
    if (!t || t.name !== 'DeliverFiles') continue;
    if (t.input && typeof t.input === 'object') return t.input;
    if (Array.isArray(t.inputs)) {
      for (let i = t.inputs.length - 1; i >= 0; i--) {
        if (t.inputs[i] && typeof t.inputs[i] === 'object') return t.inputs[i];
      }
    }
  }
  return null;
}

// 交付物文件图标：用扩展名徽标代替 emoji（白底 + 浅灰底 + 小字号扩展名）
function fileIcon(name) {
  const m = /\.([a-z0-9]+)$/i.exec(name || '');
  const ext = (m ? m[1] : 'FILE').toUpperCase().slice(0, 4);
  return '<span class="db-file-ext">' + ext + '</span>';
}

// 在系统文件管理器中定位交付文件（macOS Finder / Windows 资源管理器）
async function revealPath(path, el) {
  try {
    const r = await fetch(CORE_URL + '/api/system/reveal', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ path }),
    });
    const d = await r.json();
    if (!d || !d.ok) throw new Error((d && d.error) || '打开失败');
    if (el) {
      el.classList.add('is-flash');
      setTimeout(() => el.classList.remove('is-flash'), 600);
    }
  } catch (e) {
    console.warn('[deliver] reveal failed:', e && e.message);
    if (el) {
      el.classList.add('is-error');
      setTimeout(() => el.classList.remove('is-error'), 1200);
    }
  }
}

// 交付卡片（AI 主动调用 DeliverFiles 后渲染）
function buildDeliverCard(input) {
  const files = Array.isArray(input && input.files) ? input.files : [];
  if (!files.length) return null;
  const card = document.createElement('div');
  card.className = 'db-deliver';
  const head = document.createElement('div');
  head.className = 'db-deliver__head';
  const ico = document.createElement('span');
  ico.className = 'db-deliver__ico';
  ico.innerHTML = '<svg viewBox="0 0 14 14" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M2 4.5 7 2l5 2.5v5L7 12 2 9.5z"/><path d="M2 4.5 7 7l5-2.5"/><path d="M7 7v5"/></svg>';
  const title = document.createElement('span');
  title.className = 'db-deliver__title';
  title.textContent = (input && input.title) || ('已交付 ' + files.length + ' 个文件');
  const count = document.createElement('span');
  count.className = 'db-deliver__count';
  count.textContent = files.length + ' 个文件';
  head.appendChild(ico);
  head.appendChild(title);
  head.appendChild(count);
  const list = document.createElement('div');
  list.className = 'db-deliver__files';
  files.forEach((f) => {
    if (!f || typeof f !== 'object') return;
    const fp = String(f.path || '');
    const name = String(f.title || fp.split('/').pop() || fp);
    const chip = document.createElement('div');
    chip.className = 'db-deliver__file';
    chip.title = fp ? ('点击在访达中显示：' + fp) : '';
    if (fp) {
      chip.classList.add('is-clickable');
      chip.addEventListener('click', () => revealPath(fp, chip));
    }
    const ci = document.createElement('span');
    ci.className = 'db-deliver__file-ico';
    ci.innerHTML = fileIcon(name);
    const cbody = document.createElement('div');
    cbody.className = 'db-deliver__file-body';
    const cn = document.createElement('div');
    cn.className = 'db-deliver__file-name';
    cn.textContent = name;
    cbody.appendChild(cn);
    if (f.desc) {
      const cd = document.createElement('div');
      cd.className = 'db-deliver__file-desc';
      cd.textContent = String(f.desc);
      cbody.appendChild(cd);
    }
    chip.appendChild(ci);
    chip.appendChild(cbody);
    list.appendChild(chip);
  });
  card.appendChild(head);
  card.appendChild(list);
  return card;
}

// 运行中的工具节点每秒刷新一次真实耗时（单个 ticker，结束后自动停）
const runningTools = new Set();
let toolTicker = null;
function startToolTicker() {
  if (toolTicker) return;
  toolTicker = setInterval(() => {
    const now = Date.now();
    runningTools.forEach((t) => {
      if (!t.node.isConnected) { runningTools.delete(t); return; }
      if (t.metaDur) t.metaDur.textContent = '耗时 ' + ((now - t.startTs) / 1000).toFixed(1) + 's';
    });
    if (!runningTools.size) { clearInterval(toolTicker); toolTicker = null; }
  }, 1000);
}

// 实时工具调用节点
function addToolCard(agent, name, input) {
  const isSpeak = name === 'SpeakToUser';
  const turn = ensureTurn();
  const prev = turn.toolMap.get(name);
  if (prev) {
    // 同 turn 同名工具再次调用：追加输入、回到执行态（后端按工具名归并，无独立 id）
    prev.inputs.push(input);
    prev.argsEl.innerHTML = inputHtmlOf(name, prev.inputs);
    prev.setRunning();
    prev.startTs = Date.now();
    runningTools.add(prev);
    startToolTicker();
    turn.tl.classList.remove('is-plain');
    turn.tl.classList.add('has-line');
    if (turn.thought) turn.thought.finish();
    if (name === 'DeliverFiles') turn.deliver = input;
    setBlockRunning(turn.block, toolTitleFromInputs(name, prev.inputs));
    scrollChat();
    return;
  }
  const title = isSpeak ? '语音播报' : toolTitleFromInputs(name, input);
  const refs = makeToolNode(title, inputHtmlOf(name, [input]));
  refs.inputs = [input];
  turn.tl.insertBefore(refs.node, turn.answerNode);
  turn.toolMap.set(name, refs);
  runningTools.add(refs);
  startToolTicker();
  turn.tl.classList.remove('is-plain');
  turn.tl.classList.add('has-line');
  if (turn.thought) turn.thought.finish();
  if (name === 'DeliverFiles') turn.deliver = input;
  setBlockRunning(turn.block, title);
  scrollChat();
}

function setToolResult(agent, name, ok, summary) {
  const turn = currentTurn;
  if (!turn) return;
  const entry = turn.toolMap.get(name);
  if (!entry) {
    // 结果先于工具行到达（异常情况）：补一个结果节点
    addToolCard(agent, name, {});
    setToolResult(agent, name, ok, summary);
    return;
  }
  runningTools.delete(entry);
  const secs = (Date.now() - entry.startTs) / 1000;
  entry.finish(!!ok, summary || '', secs >= 0.1 ? secs.toFixed(1) + 's' : null);
  scrollChat();
}

// 历史消息里的静态工具节点（已完成/已中断）
function makeToolCard(t) {
  const isSpeak = t.name === 'SpeakToUser';
  const inputs = Array.isArray(t.inputs) ? t.inputs : (t.input ? [t.input] : []);
  const refs = makeToolNode(isSpeak ? '语音播报' : toolTitleFromInputs(t.name, inputs), inputHtmlOf(t.name, inputs));
  refs.inputs = inputs.slice();
  refs.finish(!!t.ok, t.summary || '', null);
  refs.metaDur.textContent = '耗时 ' + (t.dur || '—');
  return refs;
}

// 极简 Markdown 渲染（先转义防 XSS，再处理代码块/行内码/粗斜体/链接/标题/列表/引用）
function renderMd(s) {
  if (!s) return '';
  const blocks = [];
  let out = String(s).replace(/```([\s\S]*?)```/g, (m, code) => {
    const key = '\u0000' + blocks.length + '\u0000';
    blocks.push('<pre class="md-code">' + esc(code.replace(/^\n/, '').replace(/\n$/, '')) + '</pre>');
    return key;
  });
  out = esc(out);
  out = out.replace(/`([^`\n]+)`/g, '<code class="md-inline">$1</code>');
  out = out.replace(/\*\*([^*\n]+)\*\*/g, '<strong>$1</strong>');
  out = out.replace(/\*([^*\n]+)\*/g, '<em>$1</em>');
  out = out.replace(/\[([^\]\n]+)\]\((https?:\/\/[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noopener noreferrer">$1</a>');
  let lines = out.split('\n');
  // 表格：连续以 | 开头/结尾的行合并为 <table>（第二行为 --- 分隔行）
  const merged = [];
  let i = 0;
  while (i < lines.length) {
    if (/^\s*\|.*\|\s*$/.test(lines[i])) {
      const tRows = [lines[i]];
      let j = i + 1;
      while (j < lines.length && /^\s*\|.*\|\s*$/.test(lines[j])) { tRows.push(lines[j]); j++; }
      const cells = tRows.map((r) => r.trim().replace(/^\||\|$/g, '').split('|').map((c) => c.trim()));
      if (cells.length >= 2 && cells[1].length && /^[-:]+$/.test(cells[1][0])) {
        const head = cells[0];
        const body = cells.slice(2).filter((r) => r.some((c) => c));
        let tb = '<table class="md-table"><thead><tr>' + head.map((c) => '<th>' + c + '</th>').join('') + '</tr></thead><tbody>';
        for (const row of body) tb += '<tr>' + row.map((c) => '<td>' + c + '</td>').join('') + '</tr>';
        tb += '</tbody></table>';
        merged.push(tb);
      } else {
        merged.push(...tRows);
      }
      i = j;
    } else {
      merged.push(lines[i]);
      i++;
    }
  }
  lines = merged;
  const html = [];
  for (const raw of lines) {
    const line = raw.replace(/\u0000(\d+)\u0000/g, (m, i) => blocks[Number(i)]);
    const h = line.match(/^(#{1,4})\s+(.*)/);
    if (h) { html.push('<div class="md-h md-h' + h[1].length + '">' + h[2] + '</div>'); continue; }
    const q = line.match(/^&gt;\s?(.*)/);
    if (q) { html.push('<div class="md-quote">' + q[1] + '</div>'); continue; }
    const ul = line.match(/^\s*[-*]\s+(.*)/);
    if (ul) { html.push('<div class="md-li">' + ul[1] + '</div>'); continue; }
    const ol = line.match(/^\s*\d+\.\s+(.*)/);
    if (ol) { html.push('<div class="md-li"><span class="md-ol">' + (ol[0].trim().split('.')[0]) + '.</span> ' + ol[1] + '</div>'); continue; }
    if (/^\s*(---+|\*\*\*+)\s*$/.test(line)) { html.push('<hr/>'); continue; }
    html.push(line === '' ? '<br/>' : line);
  }
  return html.join('');
}

function titleCase(s) {
  return String(s || '').replace(/([A-Z])/g, ' $1').replace(/^./, (c) => c.toUpperCase()).trim();
}

// 会话历史重建时创建一个静态消息气泡（返回后由调用方追加 class）
function createMsg(text) {
  const div = document.createElement('div');
  div.className = 'msg';
  const inner = document.createElement('div');
  inner.className = 'bubble';
  inner.textContent = text;
  div.appendChild(inner);
  return div;
}

function sendText(text) {
  text = (text || '').trim();
  if (!text) return;
  finishTurn();
  // 乐观渲染：立即显示用户气泡 + “正在处理…”占位，不等待 SSE 事件
  // （避免 SSE 迟到/丢失时界面“没反应”）；SSE user_text 到达时按文本去重。
  addUserBubble(text);
  lastTurnUserText = text;
  resetDock();
  showProcessing();
  pendingLocalUser = text;
  pendingLocalCount.set(text, (pendingLocalCount.get(text) || 0) + 1);
  turnSessionId = currentSessionId;
  cacheTurnReset(currentSessionId);
  setSendBtn(true);
  apiPost('/api/chat', { text, session_id: currentSessionId })
    .then((j) => {
      if (j && j.ok === false) {
        hideProcessing();
        addSystem(j.error || '发送失败');
        // 后端拒绝：本轮未真正占用任务，立即复位按钮（轮询会按 main_status 兜底纠正）
        setSendBtn(false);
        turnSessionId = null;
      }
    })
    .catch((e) => {
      hideProcessing();
      addSystem('发送失败: ' + e.message);
      setSendBtn(false);
      turnSessionId = null;
    });
}

// 打断当前正在进行的 AI 回复（点击暂停按钮）
// 不立即跳回"发送"：保持暂停图标 + 呼吸态"正在停止…"，
// 由 pollStatus / assistant_done 以后端 main_status=idle 为权威复位，
// 避免后端仍在收尾时按钮来回闪烁。
function interruptAI() {
  if (interruptPending) return; // 已请求停止，忽略重复点击
  interruptPending = true;
  setSendBtn(true);
  apiPost('/api/chat/interrupt', {})
    .catch(() => {
      // 请求本身失败：复位为发送态（轮询会按后端真实状态纠正）
      interruptPending = false;
      setSendBtn(false);
      turnSessionId = null;
    });
}

$('#btn-send').addEventListener('click', () => {
  if (sendBusy) {
    interruptAI();
    return;
  }
  const v = $('#chat-input').value;
  $('#chat-input').value = '';
  setSendBtn(false);
  sendText(v);
});
$('#chat-input').addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault();
    if (sendBusy) return; // 忙碌中 Enter 不重复发送
    const v = $('#chat-input').value;
    $('#chat-input').value = '';
    setSendBtn(false);
    sendText(v);
  }
});
$('#chat-input').addEventListener('input', () => setSendBtn(sendBusy));

// ---------------- SSE 事件 ----------------
function openEvents() {
  const es = new EventSource(CORE_URL + '/api/events');
  es.onmessage = (m) => {
    let ev;
    try { ev = JSON.parse(m.data); } catch { return; }
    handleEvent(ev);
  };
  es.onerror = () => {
    // 后端不可达：顶部横幅提示（EventSource 会自动重连）
    connected = false;
    $('#conn-banner').hidden = false;
  };
  es.onopen = () => {
    // 后端（重启/恢复）后自动重连成功：重新拉取权威状态与会话列表，
    // 保证前端与后端绑定一致（active 高亮、暂停按钮、监听灯全量刷新）
    $('#conn-banner').hidden = true;
    pollStatus();
    loadSessions();
  };
}

function handleEvent(ev) {
  switch (ev.type) {
    case 'user_text':
      finishTurn(); // 新轮开始：先清理上一轮 DOM 状态（无论是否当前会话）
      cacheTurnReset(ev.session_id || currentSessionId);
      if (!evForCurrentSession(ev)) break;
      // 打字路径：与 sendText 乐观渲染一一对应，逐个消费计数，
      // 同文本连发时不会重复渲染用户气泡（也不误伤语音去重）。
      if (pendingLocalUser === ev.text) {
        const left = (pendingLocalCount.get(ev.text) || 0) - 1;
        if (left > 0) pendingLocalCount.set(ev.text, left);
        else pendingLocalCount.delete(ev.text);
        pendingLocalUser = null;
        break;
      }
      pendingLocalUser = null;
      // 语音路径（非打字来源）双发防护：同一文本 5 秒内重复到达视为重放，丢弃
      const now = Date.now();
      const last = lastUserTextAt.get(ev.text);
      if (last && now - last < 5000) break;
      lastUserTextAt.set(ev.text, now);
      // 语音等外部输入同样进入忙碌态（按钮变暂停可打断 AI）
      turnSessionId = ev.session_id || currentSessionId;
      setSendBtn(true);
      addUserBubble(ev.text);
      resetDock();
      showProcessing();
      break;
    case 'assistant_text': {
      cacheTurnText(ev.session_id || currentSessionId, ev.text);
      if (!evForCurrentSession(ev)) break;
      hideProcessing();
      // 流式增量：后端现在发的是 token 片段，需要追加到缓冲区再渲染
      const textTurn = ensureTurn();
      textTurn.answerNode.hidden = false;
      if (textTurn._streamBuf == null) textTurn._streamBuf = '';
      textTurn._streamBuf += ev.text;
      textTurn.text.innerHTML = renderMd(textTurn._streamBuf);
      tlSetState(textTurn.answerNode, 'running');
      // 开始输出最终回答 = 思考阶段结束，思考块自动收起为一行摘要
      if (textTurn.thought) textTurn.thought.finish();
      // 纯文字回复保持旧版白底直排观感，不显示时间线圆点
      if (!textTurn.toolMap.size && (!textTurn.thought || textTurn.thought.node.hidden)) {
        textTurn.tl.classList.add('is-plain');
      }
      scrollChat();
      break;
    }
    case 'assistant_done':
      // 全局恢复按钮状态：同一时刻后端只有一个主任务在跑，
      // 任何会话的 done 都代表该任务结束，不因当前查看的会话不同而卡住按钮/占位
      setSendBtn(false);
      turnSessionId = null;
      interruptPending = false;
      cacheTurnDone(ev.session_id || currentSessionId, ev.elapsed_ms); // 标记快照完成 + 留存本轮耗时
      resetDock();
      if (!evForCurrentSession(ev)) break;
      hideProcessing();
      if (currentTurn) {
        // 工具调用后由 tool_use 创建的 currentTurn 回答节点为空，回填最终文本；
        // 若已由 assistant_text 覆盖过则保持不动
        if (!currentTurn.text.textContent && ev.text) {
          currentTurn.text.innerHTML = renderMd(ev.text);
          currentTurn.answerNode.hidden = false;
        }
        finishTurn(ev.elapsed_ms);
      } else if (ev.text) {
        // 无工具调用的纯文本回复（后端只发 assistant_done）：直接渲染最终文本
        const div = document.createElement('div');
        div.className = 'msg assistant';
        const t = buildAssistantTurn();
        t.tl.classList.add('is-plain');
        t.answerNode.hidden = false;
        t.bubble.innerHTML = renderMd(ev.text);
        tlSetState(t.answerNode, 'completed');
        div.appendChild(t.tl);
        chatScroll.appendChild(div);
      }
      scrollChat();
      break;
    case 'tool_use':
      cacheTurnToolUse(ev.session_id || currentSessionId, ev.name, ev.input);
      if (!evForCurrentSession(ev)) break;
      addToolCard(ev.agent || 'main', ev.name, ev.input);
      break;
    case 'tool_result':
      cacheTurnToolResult(ev.session_id || currentSessionId, ev.name, ev.ok, ev.summary);
      if (!evForCurrentSession(ev)) break;
      setToolResult(ev.agent || 'main', ev.name, ev.ok, ev.summary);
      break;
    case 'reasoning_text':
      // 思考内容进入时间线内的思考节点（默认收起，展开才显示全文）
      cacheTurnReasoning(ev.session_id || currentSessionId, ev.text);
      if (!evForCurrentSession(ev)) break;
      hideProcessing();
      if (showReasoning) addReasoningCard(ev.text);
      break;
    case 'voice':
      if (ev.kind === 'wakeword') { voiceLine(`唤醒词 "${ev.text}" 已触发`); }
      else if (ev.kind === 'stt') { voiceLine(`识别: ${ev.text}`); }
      else if (ev.kind === 'speak_start') { voiceLine('语音播报中…'); }
      else if (ev.kind === 'speak_end') { voiceLine(''); }
      break;
    case 'agent_status':
      // 主 Agent 状态事件即时同步暂停按钮（无需等 3s 轮询）
      if (ev.agent === 'main') {
        if (ev.status === 'working') {
          if (!turnSessionId) turnSessionId = currentSessionId;
          setSendBtn(turnSessionId === currentSessionId);
        } else if (ev.status === 'idle') {
          setSendBtn(false);
          turnSessionId = null;
          interruptPending = false;
          resetDock();
        }
      }
      break;
    case 'schedule_triggered': addSystem(`⏰ 定时任务触发: ${ev.title}`); break;
    case 'memory_updated': loadMemory(); break;
    case 'skills_updated': loadSkills(); break;
    case 'schedules_updated': loadSchedules(); break;
    case 'settings_updated': flashSaved(); pollStatus(); break;
    default: break;
  }
}

function voiceLine() { /* no-op：输入框上方状态条已移除 */ }

$('#btn-new-session').addEventListener('click', () => newSession());

// ---------------- 唤醒日志（排查唤醒问题：KWS 后端诊断） ----------------
const kwsLog = $('#kws-log');
let kwsLogTimer = null;

async function pollKwsLog() {
  if (!window.micEngine) return;
  if (!getKwsLogEnabled()) { kwsLog.hidden = true; return; }
  try {
    const r = await fetch(`${VOICE}/kws_diag`);
    const j = await r.json();
    const entries = Array.isArray(j) ? j : ((j && j.entries) || []);
    const body = $('#kws-log-body');
    if (entries.length === 0) {
      body.innerHTML = '<div class="kws-empty">暂无唤醒检测记录 —— 请开启监听并尝试说唤醒词，这里会显示每次检测的样本量 / 音量 / 耗时。</div>';
      kwsLog.hidden = false;
      return;
    }
    // 每次刷新重建最新 N 条（新在上）
    const rows = entries.slice(0, 30).map((e) => {
      const hit = e.hit
        ? `<span class="kws-hit">✔ 命中「${esc(e.keyword || '')}」</span>`
        : '未命中';
      const t = e.ts ? new Date(Number(e.ts)).toLocaleTimeString('zh-CN', { hour12: false }) : '--';
      return `<div>${esc(t)} · ${e.samples}样本(窗${e.win_len}) · RMS ${(e.rms || 0).toFixed(3)} · ${(e.elapsed_ms || 0).toFixed(1)}ms · ${hit}</div>`;
    });
    body.innerHTML = rows.join('');
    kwsLog.hidden = false;
  } catch { /* 后端不可用则忽略 */ }
}

// ---------------- 语音监听的真实电平反馈 ----------------
// 数据来源：voice-serve 的 /kws_diag（唤醒检测窗口的真实 RMS）。
// 只在监听开启时显示；没有新鲜音频窗口就回落到静止高度，不做假动画。
const voiceWave = $('#voice-wave');
const waveBars = voiceWave ? Array.from(voiceWave.querySelectorAll('i')) : [];
let waveTimer = null;

function levelFromRms(rms) {
  const v = (Math.log10(Math.max(rms, 1e-6)) + 3.2) / 1.8;
  return Math.max(0, Math.min(1, v));
}

async function pollVoiceLevel() {
  if (!voiceWave) return;
  if (!micOn) { voiceWave.hidden = true; return; }
  voiceWave.hidden = false;
  const now = Date.now();
  let levels = [];
  try {
    const j = await (await fetch(`${VOICE}/kws_diag`)).json();
    const entries = Array.isArray(j) ? j : ((j && j.entries) || []);
    levels = entries
      .filter((e) => e && e.ts && (now - Number(e.ts)) < 1200)
      .slice(0, waveBars.length)
      .reverse()
      .map((e) => levelFromRms(Number(e.rms) || 0));
  } catch { /* 后端未就绪时保持静止 */ }
  waveBars.forEach((bar, i) => {
    const lv = levels[i] || 0;
    bar.style.height = (18 + lv * 82) + '%';
    bar.style.backgroundColor = lv > 0.55 ? '#6b7280' : '#a9afb8';
  });
}

// ---------------- 记忆 ----------------
async function loadMemory() {
  try {
    const list = await apiGet('/api/memory');
    $('#memory-count').textContent = list.length;
    const box = $('#memory-list');
    box.innerHTML = '';
    list.forEach((m) => {
      const item = document.createElement('div');
      item.className = 'list-item';
      item.innerHTML = `
        <div class="row">
          <span class="tag">${esc(m.memory_type)}</span>
          <span class="meta">${esc(m.created_at)}</span>
        </div>
        <div class="title">${esc(m.title)}</div>
        <div class="body">${esc(m.content)}</div>
        <div class="row"><div></div><div class="acts">
          <button data-del="${esc(m.id)}">删除</button>
        </div></div>`;
      box.appendChild(item);
    });
    box.querySelectorAll('[data-del]').forEach((b) => b.addEventListener('click', async () => {
      try { await apiDelete('/api/memory/' + encodeURIComponent(b.dataset.del)); } catch (e) { alert(e.message); }
    }));
  } catch (e) { $('#memory-count').textContent = '错误'; }
}

$('#memory-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const body = {
    type: $('#memory-type').value,
    title: $('#memory-title').value.trim(),
    content: $('#memory-content').value.trim(),
  };
  if (!body.title || !body.content) return;
  try {
    await apiPost('/api/memory', body);
    $('#memory-title').value = ''; $('#memory-content').value = '';
  } catch (err) { alert('保存失败: ' + err.message); }
});

$('#btn-memory-clear').addEventListener('click', async () => {
  if (!confirm('确定清空全部记忆？')) return;
  try { await apiPost('/api/memory/clear'); } catch (e) { alert(e.message); }
});

// ---------------- Skills ----------------
async function loadSkills() {
  try {
    const list = await apiGet('/api/skills');
    $('#skill-count').textContent = list.length;
    const box = $('#skill-list');
    box.innerHTML = '';
    list.forEach((s) => {
      const item = document.createElement('div');
      item.className = 'list-item';
      item.innerHTML = `
        <div class="row">
          <span class="title">${esc(s.name)}</span>
          <span class="tag ${s.enabled ? 'ok' : 'off'}">${s.enabled ? '启用' : '停用'}</span>
        </div>
        <div class="meta">${esc(s.path)}</div>
        <div class="body">${esc(s.description || '（无说明）')}</div>
        <div class="row">
          <span class="meta">来源: ${esc(s.source)}</span>
          <div class="acts">
            <button data-toggle="${esc(s.name)}">${s.enabled ? '停用' : '启用'}</button>
            <button data-remove="${esc(s.name)}">移除</button>
          </div>
        </div>`;
      box.appendChild(item);
    });
    box.querySelectorAll('[data-toggle]').forEach((b) => b.addEventListener('click', async () => {
      try { await apiPost('/api/skills/' + encodeURIComponent(b.dataset.toggle) + '/toggle', { enabled: !(b.textContent === '停用') }); } catch (e) { alert(e.message); }
    }));
    box.querySelectorAll('[data-remove]').forEach((b) => b.addEventListener('click', async () => {
      if (!confirm(`移除 skill「${b.dataset.remove}」？（不删除磁盘文件）`)) return;
      try { await apiDelete('/api/skills/' + encodeURIComponent(b.dataset.remove)); } catch (e) { alert(e.message); }
    }));
  } catch (e) { /* ignore */ }
}

$('#skill-import-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const p = $('#skill-path').value.trim();
  if (!p) return;
  try {
    await apiPost('/api/skills/import', { path: p });
    $('#skill-path').value = '';
  } catch (err) { alert('导入失败: ' + err.message); }
});

// ---------------- 定时任务 ----------------
async function loadSchedules() {
  try {
    const list = await apiGet('/api/schedules');
    $('#schedule-count').textContent = list.length;
    const box = $('#schedule-list');
    box.innerHTML = '';
    list.forEach((t) => {
      const item = document.createElement('div');
      item.className = 'list-item';
      item.innerHTML = `
        <div class="row">
          <span class="title">${esc(t.title)}</span>
          <span class="tag ${t.enabled ? 'ok' : 'off'}">${t.enabled ? '启用' : '停用'}</span>
        </div>
        <div class="meta">cron: ${esc(t.cron_expr)} · 上次: ${esc(t.last_run || '从未')}</div>
        <div class="body">${esc(t.prompt)}</div>
        <div class="row"><div></div><div class="acts">
          <button data-toggle="${esc(t.id)}">${t.enabled ? '停用' : '启用'}</button>
          <button data-del="${esc(t.id)}">删除</button>
        </div></div>`;
      box.appendChild(item);
    });
    box.querySelectorAll('[data-toggle]').forEach((b) => b.addEventListener('click', async () => {
      try { await apiPost('/api/schedules/' + encodeURIComponent(b.dataset.toggle), { enabled: !(b.textContent === '停用') }); } catch (e) { alert(e.message); }
    }));
    box.querySelectorAll('[data-del]').forEach((b) => b.addEventListener('click', async () => {
      if (!confirm('删除该定时任务？')) return;
      try { await apiDelete('/api/schedules/' + encodeURIComponent(b.dataset.del)); } catch (e) { alert(e.message); }
    }));
  } catch (e) { /* ignore */ }
}

$('#schedule-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const body = {
    title: $('#sched-title').value.trim(),
    prompt: $('#sched-prompt').value.trim(),
    cron_expr: $('#sched-cron').value.trim(),
  };
  if (!body.title || !body.prompt || !body.cron_expr) return;
  try {
    await apiPost('/api/schedules', body);
    $('#sched-title').value = ''; $('#sched-prompt').value = ''; $('#sched-cron').value = '';
  } catch (err) { alert('创建失败: ' + err.message); }
});


// ---------------- 设置 ----------------
function keyRowHTML(k, idx) {
  return (
    '<div class="key-row">' +
    '<span class="k-badge">Key ' + (idx + 1) + '</span>' +
    '<input class="p-key" type="password" placeholder="同服务商 API Key" value="' + (k || '') + '" />' +
    '<button type="button" class="k-del" title="删除该 Key">✕</button>' +
    '</div>'
  );
}

function renumberKeys(card) {
  card.querySelectorAll('.key-row').forEach((row, i) => {
    const b = row.querySelector('.k-badge');
    if (b) b.textContent = 'Key ' + (i + 1);
  });
}

// ---- 两级切换：后端类型（内置 API / OpenCode） → 该后端下的模型 ----
let modelList = [];
let activeModelIdx = 0;
// 记住上一次内置 API 的选择，切回内置后端时恢复它（而不是跳到第一条）
let lastApiIdx = 0;
// OpenCode 后端可选的模型（来自 /api/opencode/models）；null = 尚未加载
let ocModels = null;
let ocLoading = false;

function isOcProvider(p) {
  const b = ((p && p.base_url) || '').trim().toLowerCase();
  return b === 'opencode' || b === 'opencode://' || b === 'opencode://cli';
}

// 当前生效的后端类型由「当前选中的条目」决定，保证设置与底部徽标永远一致
function activeProvider() {
  if (!modelList.length) return null;
  return modelList[Math.min(activeModelIdx, modelList.length - 1)] || null;
}

function backendKind() {
  return isOcProvider(activeProvider()) ? 'opencode' : 'api';
}

function apiProviderIndexes() {
  return modelList.map((_, i) => i).filter((i) => !isOcProvider(modelList[i]));
}

function ocProviderIndex() {
  return modelList.findIndex(isOcProvider);
}

function ocModelMeta(id) {
  return (ocModels || []).find((m) => m.id === id) || null;
}

async function loadOpenCodeModels() {
  try {
    const r = await apiGet('/api/opencode/models');
    ocModels = (r && r.models) || [];
    if (r && r.ok === false) console.warn('OpenCode 模型列表获取失败:', r.error);
  } catch (e) {
    ocModels = [];
    console.warn('OpenCode 模型列表获取失败:', e.message);
  }
  return ocModels;
}

function modelEditHTML(m, ordinal) {
  const keys = (m.api_keys && m.api_keys.length) ? m.api_keys : (m.api_key ? [m.api_key] : ['']);
  let ks = '<div class="key-list">';
  keys.forEach((k, i) => { ks += keyRowHTML(k, i); });
  ks += '<button type="button" class="key-add" title="添加一个同配置 API Key">＋ 添加一个 Key</button></div>';
  return (
    '<div class="me-head">' +
    '<span class="pc-badge">模型 ' + ordinal + '</span>' +
    '<label class="me-name-wrap">显示名称 <input class="me-name" placeholder="如 主 API / 备用 API / 本地 Ollama" /></label>' +
    '</div>' +
    '<div class="pc-grid">' +
    '<label class="pc-url-wrap">Base URL <input class="me-url" placeholder="OpenAI 兼容端点，如 https://api.openai.com/v1（带不带 /v1 均可）" /></label>' +
    '<label class="pc-model-wrap">模型名 <input class="me-model" placeholder="如 gpt-4o / qwen2.5 / deepseek-chat" /></label>' +
    '<label class="pc-rpm-wrap">每个 Key 限额 RPM <input class="me-rpm" type="number" min="1" step="1" placeholder="20" /></label>' +
    '</div>' + ks
  );
}

function wireKeyList(edit) {
  edit.addEventListener('click', (e) => {
    const kList = edit.querySelector('.key-list');
    if (!kList) return;
    if (e.target.classList.contains('k-del')) {
      e.target.closest('.key-row').remove();
      if (!kList.querySelector('.key-row')) kList.insertAdjacentHTML('beforeend', keyRowHTML('', 0));
      renumberKeys(edit);
    } else if (e.target.classList.contains('key-add')) {
      kList.insertAdjacentHTML('beforeend', keyRowHTML('', kList.querySelectorAll('.key-row').length));
      renumberKeys(edit);
      const inputs = kList.querySelectorAll('.p-key');
      if (inputs.length) inputs[inputs.length - 1].focus();
    }
  });
}

function renderModelSelector() {
  const sel = $('#model-select');
  const chatSel = $('#chat-model-select');
  const badge = $('#chat-backend-badge');
  const sw = $('#backend-switch');
  const kind = backendKind();
  if (sw) {
    sw.querySelectorAll('[data-backend]').forEach((b) => {
      b.classList.toggle('is-active', b.getAttribute('data-backend') === kind);
    });
  }
  const label = $('#model-select-label');
  const newBtn = $('#btn-model-new');
  const delBtn = $('#btn-model-del');
  sel.innerHTML = '';
  if (chatSel) chatSel.innerHTML = '';
  const optFor = (s, value, text) => {
    if (!s) return;
    const o = document.createElement('option');
    o.value = value;
    o.textContent = text;
    s.appendChild(o);
  };

  if (!modelList.length) {
    optFor(sel, '', '（还没有后端配置，点「＋ 新建」）');
    if (chatSel) optFor(chatSel, '', '（无模型）');
    if (badge) { badge.textContent = '—'; badge.classList.remove('is-oc'); }
    renderModelEdit();
    return;
  }

  if (kind === 'api') {
    if (label) label.textContent = '模型';
    if (newBtn) newBtn.hidden = false;
    if (delBtn) delBtn.hidden = false;
    if (badge) { badge.textContent = '内置'; badge.classList.remove('is-oc'); }
    const apis = apiProviderIndexes();
    if (!apis.length) {
      optFor(sel, '', '（还没有内置模型，点「＋ 新建」）');
    } else {
      apis.forEach((gi) => {
        const m = modelList[gi];
        const ord = apis.indexOf(gi) + 1;
        optFor(sel, String(gi), (m.name || ('模型 ' + ord)) + (m.model ? ' · ' + m.model : ''));
      });
      const cur = apis.includes(activeModelIdx) ? activeModelIdx : apis[0];
      activeModelIdx = cur;
      lastApiIdx = cur;
      sel.value = String(cur);
    }
    if (chatSel) {
      chatSel.title = '内置 API 后端：在已保存的模型配置之间切换（立即生效）';
      apis.forEach((gi) => {
        const m = modelList[gi];
        const ord = apis.indexOf(gi) + 1;
        optFor(chatSel, String(gi), (m.name || ('模型 ' + ord)) + (m.model ? ' · ' + m.model : ''));
      });
      if (apis.length) chatSel.value = String(activeModelIdx);
    }
  } else {
    if (label) label.textContent = 'OpenCode 模型';
    if (newBtn) newBtn.hidden = true;
    if (delBtn) delBtn.hidden = true;
    if (badge) { badge.textContent = 'OpenCode'; badge.classList.add('is-oc'); }
    if (chatSel) chatSel.title = 'OpenCode 后端：选择 CLI 可识别的模型（含免费模型），立即生效';
    const ocIdx = ocProviderIndex();
    const cur = ocIdx >= 0 ? (modelList[ocIdx].model || '').trim() : '';
    if (ocModels === null) {
      optFor(sel, '', '加载模型列表…');
      optFor(chatSel, '', '加载模型列表…');
      if (!ocLoading) {
        ocLoading = true;
        loadOpenCodeModels().finally(() => {
          ocLoading = false;
          renderModelSelector();
        });
      }
    } else if (!ocModels.length) {
      optFor(sel, cur ? 'oc:' + cur : '', cur || '（未获取到模型，检查 CLI 路径）');
      optFor(chatSel, cur ? 'oc:' + cur : '', cur || '（未获取到模型）');
    } else {
      if (cur && !ocModels.some((m) => m.id === cur)) {
        optFor(sel, 'oc:' + cur, cur + '（当前）');
        optFor(chatSel, 'oc:' + cur, cur + '（当前）');
      }
      ocModels.forEach((m) => {
        const text = m.free ? m.id + '（免费）' : m.id;
        optFor(sel, 'oc:' + m.id, text);
        optFor(chatSel, 'oc:' + m.id, text);
      });
      if (cur) {
        sel.value = 'oc:' + cur;
        if (chatSel) chatSel.value = 'oc:' + cur;
      } else {
        sel.selectedIndex = 0;
        if (chatSel) chatSel.selectedIndex = 0;
      }
    }
  }
  renderModelEdit();
}

// OpenCode 编辑区：只需展示当前模型与列表状态（模型本身在上方下拉里选）
function renderOcEdit() {
  const box = $('#model-edit');
  const i = ocProviderIndex();
  const p = i >= 0 ? modelList[i] : null;
  const id = (p && p.model) || '';
  const meta = ocModelMeta(id);
  box.innerHTML =
    '<div class="oc-panel">' +
      '<div class="oc-panel__row">' +
        '<span class="oc-panel__dot"></span>' +
        '<div class="oc-panel__main">' +
          '<div class="oc-panel__label">当前 OpenCode 模型</div>' +
          '<div class="oc-panel__model">' + (id ? esc(id) : '未选择（从上方下拉选择）') +
            (meta && meta.free ? '<span class="oc-free">免费</span>' : '') +
          '</div>' +
        '</div>' +
        '<button type="button" class="btn-mini" id="btn-oc-refresh">刷新列表</button>' +
      '</div>' +
      '<p class="form-hint">模型由本机 OpenCode CLI 实时返回（' + ((ocModels || []).length) + ' 个）。切换模型只影响本后端，立即生效；会话上下文按模型隔离。</p>' +
    '</div>';
  const btn = box.querySelector('#btn-oc-refresh');
  if (btn) {
    btn.addEventListener('click', () => {
      ocModels = null;
      renderModelSelector();
    });
  }
}

function renderModelEdit() {
  const box = $('#model-edit');
  if (!modelList.length) {
    box.innerHTML = '<p class="form-hint">还没有模型配置，点「＋ 新建」添加第一套。</p>';
    return;
  }
  if (backendKind() === 'opencode') {
    renderOcEdit();
    return;
  }
  const idx = Math.min(activeModelIdx, modelList.length - 1);
  const m = modelList[idx];
  const ordinal = apiProviderIndexes().indexOf(idx) + 1;
  box.innerHTML = modelEditHTML(m, ordinal);
  box.querySelector('.me-name').value = m.name || '';
  box.querySelector('.me-url').value = m.base_url || '';
  box.querySelector('.me-model').value = m.model || '';
  box.querySelector('.me-rpm').value = m.rpm_limit ?? '';
  wireKeyList(box);
}

// 把当前编辑区的内容写回内存（切模型/后端/保存前调用，避免未保存改动被覆盖）
function flushModelEdit() {
  if (backendKind() !== 'api' || !modelList.length) return;
  const edit = $('#model-edit');
  if (!edit || !edit.querySelector('.me-name')) return;
  const idx = Math.min(activeModelIdx, modelList.length - 1);
  const cur = modelList[idx];
  if (!cur) return;
  cur.name = (edit.querySelector('.me-name').value || '').trim() || cur.name || ('模型 ' + (idx + 1));
  cur.model = (edit.querySelector('.me-model').value || '').trim();
  cur.base_url = (edit.querySelector('.me-url').value || '').trim();
  cur.rpm_limit = edit.querySelector('.me-rpm').value ? parseInt(edit.querySelector('.me-rpm').value, 10) : null;
  const keys = Array.from(edit.querySelectorAll('.p-key')).map((el) => el.value.trim()).filter(Boolean);
  cur.api_keys = keys;
  cur.api_key = keys[0] || '';
}

// 只回写 providers / active_model（其余设置保持磁盘上的值），切换后立即生效
async function saveModels() {
  try {
    const cfg = await apiGet('/api/config');
    const rows = modelList.filter((p) => (p.base_url || '').trim());
    const active = activeProvider();
    cfg.providers = rows;
    if (active && rows.some((p) => p.name === active.name)) cfg.active_model = active.name;
    else if (rows.length) cfg.active_model = rows[0].name;
    cfg.main_base_url = (rows[0] || {}).base_url || '';
    cfg.main_api_key = (rows[0] || {}).api_key || '';
    cfg.main_model = (rows[0] || {}).model || '';
    cfg.sub_base_url = (rows[1] || {}).base_url || '';
    cfg.sub_api_key = (rows[1] || {}).api_key || '';
    cfg.sub_model = (rows[1] || {}).model || '';
    const r = await apiPost('/api/config', cfg);
    if (r && r.ok === false) throw new Error(r.error || '保存失败');
  } catch (e) {
    console.warn('保存模型选择失败:', e.message);
  }
}

// 切后端：内置 API ↔ OpenCode（没有对应条目时自动建一条默认项）
async function switchBackend(kind) {
  if (kind === backendKind()) return;
  flushModelEdit();
  if (kind === 'api') {
    const apis = apiProviderIndexes();
    let i = apis.includes(lastApiIdx) ? lastApiIdx : (apis[0] ?? -1);
    if (i < 0) {
      modelList.push({ name: '主 API', base_url: '', model: '', api_keys: [''], api_key: '', rpm_limit: null });
      i = modelList.length - 1;
    }
    activeModelIdx = i;
    renderModelSelector();
    saveModels();
    return;
  }
  let i = modelList.findIndex(isOcProvider);
  if (i < 0) {
    modelList.push({ name: 'OpenCode（本机 CLI）', base_url: 'opencode', model: '', api_keys: [], api_key: '', rpm_limit: null });
    i = modelList.length - 1;
  }
  activeModelIdx = i;
  renderModelSelector();
  if (ocModels === null) {
    loadOpenCodeModels().then(() => {
      const j = ocProviderIndex();
      if (j >= 0 && !modelList[j].model && ocModels.length) {
        modelList[j].model = ocModels[0].id;
        renderModelSelector();
      }
      saveModels();
    });
  } else {
    if (!modelList[i].model && ocModels.length) modelList[i].model = ocModels[0].id;
    renderModelSelector();
    saveModels();
  }
}

async function loadConfig() {
  try {
    const cfg = await apiGet('/api/config');
    const form = $('#settings-form');
    // 多 API 列表：优先 providers，缺省从旧 main/sub 字段生成
    const providers = cfg.providers && cfg.providers.length
      ? cfg.providers
      : [
          { name: '主 API', base_url: cfg.main_base_url || '', model: cfg.main_model || '', api_key: cfg.main_api_key || '', rpm_limit: null },
          { name: '备用 API', base_url: cfg.sub_base_url || '', model: cfg.sub_model || '', api_key: cfg.sub_api_key || '', rpm_limit: null },
        ];
    modelList = providers.map((p) => ({ ...p }));
    const ai = modelList.findIndex((p) => p.name && p.name === cfg.active_model);
    activeModelIdx = ai >= 0 ? ai : 0;
    renderModelSelector();
    form.main_system_prompt.value = cfg.main_system_prompt || '';
    form.voice_port.value = cfg.voice_port ?? 8420;
    form.wakeword.value = cfg.wakeword || 'computer';
    form.beep_file.value = cfg.beep_file || '';
    form.voice.value = cfg.voice || '';
    form.rate.value = String(Number(((cfg.rate ?? 1.0)).toFixed(2)));
    form.tts_backend.value = cfg.tts_backend || 'internal';
    form.interrupt_keywords.value = (cfg.interrupt_keywords || []).join(',');
    form.kws_threshold.value = String(Number(((cfg.kws_threshold ?? 0.15)).toFixed(2)));
    form.max_record_secs.value = cfg.max_record_secs ?? 120;
    form.silence_limit_secs.value = cfg.silence_limit_secs ?? 2.5;
    form.continuous_silence_secs.value = cfg.continuous_silence_secs ?? 1.5;
    form.continuous_max_record_secs.value = cfg.continuous_max_record_secs ?? 12;
    form.continuous_idle_rounds.value = cfg.continuous_idle_rounds ?? 3;
    form.max_turns.value = cfg.max_turns ?? 1000;
    form.rpm_limit.value = cfg.rpm_limit ?? 20;
    form.enable_thinking.checked = !!cfg.enable_thinking;
    form.voice_enabled.checked = !!cfg.voice_enabled;
    window.voiceEnabled = !!cfg.voice_enabled;
    form.ask_user_enabled.checked = cfg.ask_user_enabled !== false;
    form.output_language.value = cfg.output_language || 'zh';
    form.voice_en.value = cfg.voice_en || 'en-US-AriaNeural';
    if (form.opencode_cli_path) form.opencode_cli_path.value = cfg.opencode_cli_path || '';
    if (form.opencode_prompt) form.opencode_prompt.value = cfg.opencode_prompt || '';
    form.show_kws_log.checked = getKwsLogEnabled();
    form.show_reasoning.checked = showReasoning;
    const themeSel = $('#ui-theme');
    if (themeSel) themeSel.value = getUiTheme();
  } catch (e) { /* ignore */ }
}

function flashSaved() {
  const el = $('#settings-saved');
  el.hidden = false;
  setTimeout(() => { el.hidden = true; }, 2000);
}

// 设置分级 Tab 切换
(function () {
  const tabs = document.querySelector('#settings-tabs');
  if (!tabs) return;
  tabs.addEventListener('click', (e) => {
    const btn = e.target.closest('button[data-stab]');
    if (!btn) return;
    const name = btn.getAttribute('data-stab');
    tabs.querySelectorAll('button').forEach((b) => b.classList.toggle('active', b === btn));
    document.querySelectorAll('.settings-panel').forEach((p) => {
      p.classList.toggle('active', p.getAttribute('data-spanel') === name);
    });
  });
})();

// 后端类型（内置 / OpenCode）：独立的一级切换
$('#backend-switch').addEventListener('click', (e) => {
  const btn = e.target.closest('button[data-backend]');
  if (!btn) return;
  switchBackend(btn.getAttribute('data-backend'));
});

// 设置页模型下拉：只列出当前后端下的模型
$('#model-select').addEventListener('change', async (e) => {
  const val = e.target.value;
  if (!val) return;
  if (val.startsWith('oc:')) {
    const i = ocProviderIndex();
    if (i < 0) return;
    modelList[i].model = val.slice(3);
    activeModelIdx = i;
    renderModelSelector();
    await saveModels();
    return;
  }
  flushModelEdit();
  activeModelIdx = parseInt(val, 10) || 0;
  renderModelSelector();
  await saveModels();
});

// 对话界面底部直接切换模型：只切「当前后端下的模型」，并立即持久化
$('#chat-model-select').addEventListener('change', async (e) => {
  const val = e.target.value;
  if (!val) return;
  if (val.startsWith('oc:')) {
    const i = ocProviderIndex();
    if (i < 0) {
      alert('OpenCode 后端还没保存到设置，请先在「设置 → 模型」里保存');
      return;
    }
    modelList[i].model = val.slice(3);
    activeModelIdx = i;
    renderModelSelector();
    await saveModels();
    return;
  }
  const idx = parseInt(val, 10);
  if (!Number.isNaN(idx) && modelList[idx] && !isOcProvider(modelList[idx])) {
    activeModelIdx = idx;
    renderModelSelector();
    await saveModels();
  }
});

// 页面样式主题切换（本地 UI 偏好，立即生效，不入后端配置）
$('#ui-theme').addEventListener('change', (e) => applyUiTheme(e.target.value));

$('#btn-model-new').addEventListener('click', () => {
  flushModelEdit();
  const ord = apiProviderIndexes().length + 1;
  modelList.push({ name: '模型 ' + ord, base_url: '', model: '', api_keys: [''], api_key: '', rpm_limit: null });
  activeModelIdx = modelList.length - 1;
  renderModelSelector();
  const url = document.querySelector('#model-edit .me-url');
  if (url) url.focus();
});

$('#btn-model-del').addEventListener('click', () => {
  if (backendKind() !== 'api' || modelList.length <= 1) return;
  flushModelEdit();
  modelList.splice(activeModelIdx, 1);
  const apis = apiProviderIndexes();
  activeModelIdx = apis.length ? apis[0] : 0;
  renderModelSelector();
  saveModels();
});

$('#settings-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const f = e.target;
  // 唤醒日志显隐是本地 UI 偏好（默认关闭），不入后端配置
  localStorage.setItem('kws_log_enabled', f.show_kws_log.checked ? '1' : '0');
  if (!f.show_kws_log.checked) kwsLog.hidden = true;
  showReasoning = f.show_reasoning.checked;
  localStorage.setItem('st_show_reasoning', showReasoning ? '1' : '0');
  // 模型配置：把当前编辑区的修改写回内存列表，再整体提交
  flushModelEdit();
  const providerRows = modelList.filter((p) => p.base_url);
  const activeName = modelList.length
    ? (modelList[Math.min(activeModelIdx, modelList.length - 1)].name || '')
    : '';
  const activeModelName = providerRows.some((p) => p.name === activeName)
    ? activeName
    : ((providerRows[0] || {}).name || '');
  const cfg = {
    providers: providerRows,
    active_model: activeModelName,
    // 向后兼容：主端点取第一个 provider，备用取第二个
    main_base_url: (providerRows[0] || {}).base_url || '',
    main_api_key: (providerRows[0] || {}).api_key || '',
    main_model: (providerRows[0] || {}).model || '',
    sub_base_url: (providerRows[1] || {}).base_url || '',
    sub_api_key: (providerRows[1] || {}).api_key || '',
    sub_model: (providerRows[1] || {}).model || '',
    sub_system_prompt: '',
    main_system_prompt: f.main_system_prompt.value,
    voice_port: parseInt(f.voice_port.value, 10) || 8420,
    wakeword: f.wakeword.value.trim() || 'computer',
    beep_file: f.beep_file.value.trim(),
    voice: f.voice.value.trim(),
    rate: Math.round((parseFloat(f.rate.value) || 1.0) * 100) / 100,
    tts_backend: f.tts_backend.value,
    interrupt_keywords: f.interrupt_keywords.value.split(/[,，]/).map((s) => s.trim()).filter(Boolean),
    kws_threshold: Math.round((parseFloat(f.kws_threshold.value) || 0.15) * 100) / 100,
    max_record_secs: parseFloat(f.max_record_secs.value) || 120,
    silence_limit_secs: parseFloat(f.silence_limit_secs.value) || 2.5,
    continuous_silence_secs: parseFloat(f.continuous_silence_secs.value) || 1.5,
    continuous_max_record_secs: parseFloat(f.continuous_max_record_secs.value) || 12,
    continuous_idle_rounds: parseInt(f.continuous_idle_rounds.value, 10) || 3,
    max_turns: parseInt(f.max_turns.value, 10) || 1000,
    rpm_limit: parseInt(f.rpm_limit.value, 10) || 20,
    enable_thinking: f.enable_thinking.checked,
    voice_enabled: f.voice_enabled.checked,
    ask_user_enabled: f.ask_user_enabled.checked,
    output_language: f.output_language.value || 'zh',
    voice_en: f.voice_en.value.trim() || 'en-US-AriaNeural',
    opencode_cli_path: f.opencode_cli_path ? f.opencode_cli_path.value.trim() : '',
    opencode_prompt: f.opencode_prompt ? f.opencode_prompt.value : '',
  };
  window.voiceEnabled = cfg.voice_enabled;
  try {
    await apiPost('/api/config', cfg);
  } catch (err) { alert('保存失败: ' + err.message); }
});

// ---------------- 工具 ----------------
function esc(s) {
  return String(s ?? '').replace(/[&<>"']/g, (c) => (
    { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]
  ));
}

// ---------------- 启动 ----------------
pollStatus();
statusTimer = setInterval(pollStatus, 3000);
loadMemory();
loadSkills();
loadSchedules();
loadConfig();
loadSessions();
kwsLogTimer = setInterval(pollKwsLog, 3000);
waveTimer = setInterval(pollVoiceLevel, 320);
openEvents();
