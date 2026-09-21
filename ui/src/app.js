// 渲染进程主逻辑：导航、状态轮询、SSE 事件、各面板 CRUD。
// 对话流为 goose 风格：用户气泡 / 助手气泡（内含工具调用卡片）+ 底部输入卡。
// 语音链路为纯 Rust 后端采集（cpal→KWS→STT→LLM→TTS），前端只做 UI 展示。
'use strict';

const $ = (sel) => document.querySelector(sel);
// 不依赖 preload：window.star 注入成功则用之，否则回退默认本地端口
const VOICE = (window.star && window.star.voiceUrl) || 'http://127.0.0.1:8420';
const CORE_URL = (window.star && window.star.coreUrl) || 'http://127.0.0.1:8410';
console.log(`[app] VOICE=${VOICE} core=${CORE_URL}`);

// 当前会话 id（切换会话时更新；null = 跟随后端 active）
let currentSessionId = null;
// 最近一次由 sendText 乐观渲染的用户文本（SSE user_text 去重用）
let pendingLocalUser = null;
// 语音双发防护：记录最近到达的用户文本时间戳（5 秒窗口去重）
const lastUserTextAt = new Map();
// 唤醒日志显隐开关（本地 UI 偏好，默认关闭）
function getKwsLogEnabled() {
  return localStorage.getItem('kws_log_enabled') === '1';
}

// 对话流事件仅渲染当前会话；session_id 为空 = 全局事件（子任务/语音），不拦截
function evForCurrentSession(ev) {
  if (!ev.session_id) return true;
  if (!currentSessionId) return true;
  return ev.session_id === currentSessionId;
}

// ---------------- 导航 ----------------
document.querySelectorAll('.nav-item').forEach((btn) => {
  btn.addEventListener('click', () => {
    document.querySelectorAll('.nav-item').forEach((b) => b.classList.remove('active'));
    btn.classList.add('active');
    document.querySelectorAll('.view').forEach((v) => v.classList.remove('active'));
    $('#view-' + btn.dataset.view).classList.add('active');
    if (btn.dataset.view !== 'tasks' && taskDetailModal && !taskDetailModal.hidden) closeTaskDetail();
  });
});

// ---------------- 状态轮询 ----------------
let connected = false;
let statusTimer = null;

// 发送按钮双态：空闲=箭头（发送），有任务=暂停（点击打断 AI）
let sendBusy = false;
let mainBusy = false;      // 后端 main_busy（轮询兜底）
let turnSessionId = null;  // 当前正在回复的会话 id（用于切换会话时判断）

function setSendBtn(busy) {
  sendBusy = !!busy;
  const btn = $('#btn-send');
  if (!btn) return;
  const sendIco = btn.querySelector('.ico-send');
  const pauseIco = btn.querySelector('.ico-pause');
  if (sendIco) sendIco.hidden = sendBusy;
  if (pauseIco) pauseIco.hidden = !sendBusy;
  btn.title = sendBusy ? '暂停（打断 AI）' : '发送';
  btn.setAttribute('aria-label', btn.title);
  btn.classList.toggle('busy', sendBusy);
}

async function pollStatus() {
  try {
    const s = await apiGet('/api/status');
    connected = true;
    $('#conn-dot').className = 'dot on';
    renderVoiceDiag(s);
    mainBusy = s.main_status === 'working';
    // 后端 main_status 是权威状态：无条件跟随，避免 assistant_done 丢失/
    // 会话切换等边界导致暂停图标与后端真实状态不一致。
    // 忙时若 turnSessionId 已被清空则重新登记，保证切回会话能恢复暂停按钮。
    if (mainBusy) {
      if (!turnSessionId) turnSessionId = currentSessionId;
      setSendBtn(true);
    } else {
      setSendBtn(false);
      turnSessionId = null;
    }
    // Agent 监控面板：主 Agent 状态（语音播报优先展示）
    if (s.speaking) setAgentState('main', 'speaking');
    else if (mainBusy) setAgentState('main', 'working');
    else setAgentState('main', 'idle');
    // 唤醒/麦克风按钮指示灯：播报(蓝) > 主Agent工作(LCARS紫) > 唤醒后录音(黄) > 监听(绿) > 关(灰)
    const vd = s.voice_diag || {};
    if (s.speaking) setMicBtn('mic-speaking', '语音播报中（蓝色）');
    else if (mainBusy) setMicBtn('mic-working', '主 Agent 处理中（紫色）');
    else if (s.voice_active) setMicBtn('mic-active', '已唤醒·录音处理中（黄色）');
    else if (vd.listening) setMicBtn('mic-standby', '监听中（绿色）· 点击关闭');
    else setMicBtn('mic-off', '开启语音监听');
  } catch {
    connected = false;
    $('#conn-dot').className = 'dot off';
  }
}

// 语音链路诊断条：后端在线 + KWS 模型 + 常驻采集 + 麦克风权限状态。
// 设置里关闭语音（voice_enabled=false）时整条隐藏，不残留输入框上方。
function renderVoiceDiag(s) {
  const el = $('#voice-diag');
  if (!window.voiceEnabled) {
    el.hidden = true;
    return;
  }
  if (!s.voice_connected) {
    el.hidden = true;
    return;
  }
  const d = s.voice_diag || {};
  const kwsOk = d.kws_ok !== false;
  const capOk = d.cap_ok !== false;
  const micAlive = !!d.mic_alive;
  const listening = !!d.listening;
  const speaking = !!d.speaking;

  const parts = [];
  if (!kwsOk) parts.push('唤醒词模型未加载（检查 kws 模型目录），语音唤醒已禁用');
  if (!capOk) parts.push('麦克风采集不可用（未检测到输入设备）');
  if (micAlive) parts.push('麦克风正常');
  else parts.push('麦克风静音/未授权：请开启监听并允许麦克风权限');
  if (listening) parts.push('监听已开启');
  if (speaking) parts.push('正在播报');
  const ok = kwsOk && capOk && micAlive;
  el.innerHTML = `<div class="diag-${ok ? 'ok' : 'warn'}">${parts.map((p) => esc(p)).join(' · ')}</div>`;
  el.hidden = false;
}

// ---------------- 麦克风（后端常驻采集，前端仅控制监听开关） ----------------
function setMicState(cls, title) {
  const el = $('#mic-dot');
  if (!el) return;
  el.className = 'dot ' + cls;
  el.title = title || '';
}

// 监听按钮颜色状态指示灯：off(灰) / standby(绿) / err(红)
function setMicBtn(mode, title) {
  const btn = $('#btn-mic');
  if (!btn) return;
  btn.className = 'icon-btn ' + mode;
  btn.title = title || (mode === 'mic-off' ? '开启语音监听' : '关闭语音监听');
}

const mic = window.micEngine;
let micOn = false;

$('#btn-mic').addEventListener('click', async () => {
  if (!micOn) {
    // ①授权麦克风（getUserMedia 一次性）+ 重建后端常驻采集 ②开启后端监听
    await mic.start();
    if (mic.getState() === 'err') { setMicBtn('mic-err', '麦克风不可用'); return; }
    await apiPost('/api/voice/listening', { enabled: true, session_id: currentSessionId }).catch(() => {});
    micOn = true;
    setMicState('on', '监听中：等待唤醒词');
    setMicBtn('mic-standby', '监听中（绿色）· 点击关闭');
  } else {
    mic.stop();
    await apiPost('/api/voice/listening', { enabled: false }).catch(() => {});
    micOn = false;
    setMicBtn('mic-off', '开启语音监听');
    setMicState('off', '监听已停止');
    voiceLine('');
  }
});

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
  const el = $('#voice-diag');
  el.innerHTML = `<div class="diag-warn">⚠ ${esc(message)} <button id="btn-open-mic" class="mini" style="margin-left:6px">打开系统设置</button></div>`;
  el.hidden = false;
  const openBtn = $('#btn-open-mic');
  if (openBtn) openBtn.addEventListener('click', () => apiPost('/api/system/open-mic-settings').catch(() => {}));
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
    // 确定当前会话：已有有效 id 沿用；否则后端 active；再否则列表第一个
    const stillExists = currentSessionId && list.some((s) => s.id === currentSessionId);
    if (!stillExists) {
      currentSessionId = (activeId && list.some((s) => s.id === activeId))
        ? activeId
        : list[0].id;
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
    await apiPost(`/api/sessions/${id}/switch`, {});
    currentSessionId = id;
    pendingLocalUser = null;
    await loadMessages(id);
    await loadSessions(); // 刷新 active 高亮
    syncVoiceSession();
    // 切回正在进行回复的会话：恢复暂停按钮 + “正在处理”占位，
    // AI 完成后 assistant_done 到达会正常补上最终文本，避免回复“消失”
    if (turnSessionId === id && sendBusy) {
      setSendBtn(true);
      showProcessing();
    } else {
      setSendBtn(false);
    }
  } catch { /* 忽略 */ }
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
  } catch { /* 忽略 */ }
}

async function loadMessages(id) {
  try {
    const j = await apiGet(`/api/sessions/${id}/messages`);
    renderChat(j.messages || []);
  } catch { /* 忽略 */ }
}

// 渲染整个对话流（会话历史 / 切换时重建）
function renderChat(msgs) {
  const chat = $('#chat-scroll');
  chat.innerHTML = '';
  currentTurn = null;
  resetReasoning();
  processingEl = null; // innerHTML 已清空占位节点，同步重置引用
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
      if (m.tools && m.tools.length) {
        const stack = document.createElement('div');
        stack.className = 'tool-stack';
        m.tools.forEach((t) => stack.appendChild(makeToolCard(t)));
        div.appendChild(stack);
      }
      const bubble = document.createElement('div');
      bubble.className = 'bubble';
      bubble.textContent = m.text || '';
      div.appendChild(bubble);
      chat.appendChild(div);
    }
  });
  chat.scrollTop = chat.scrollHeight;
}

// ---------------- 对话（goose 风格：用户/助手气泡 + 工具调用卡片） ----------------
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

// “正在处理…”占位（goose 风格：助手侧 spinner + 灰字，首个 assistant_text 到达时移除）
let processingEl = null;
function showProcessing() {
  hideProcessing();
  const div = document.createElement('div');
  div.className = 'msg assistant processing';
  const inner = document.createElement('div');
  inner.className = 'bubble';
  inner.innerHTML = '<span class="spinner"></span><span class="processing-text">正在处理…</span>';
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

// 创建或复用当前助手气泡
function ensureTurn() {
  if (currentTurn) return currentTurn;
  const msg = document.createElement('div');
  msg.className = 'msg assistant';
  const tools = document.createElement('div');
  tools.className = 'tool-stack';
  const bubble = document.createElement('div');
  bubble.className = 'bubble';
  msg.appendChild(tools);
  msg.appendChild(bubble);
  chatScroll.appendChild(msg);
  currentTurn = { msg, text: bubble, tools, toolMap: new Map() };
  scrollChat();
  return currentTurn;
}

function finishTurn() {
  currentTurn = null;
  resetReasoning();
}

// ---------------- AI 中间思考过程（左箭头折叠卡，同轮累积） ----------------
let currentReasoningEl = null;
let reasoningBuf = '';
let reasoningTimer = null;

function addReasoningCard(text) {
  const turn = ensureTurn();
  if (!currentReasoningEl) {
    const card = document.createElement('div');
    card.className = 'reasoning-card';
    const head = document.createElement('div');
    head.className = 'rc-head';
    const arrow = document.createElement('span');
    arrow.className = 'rc-arrow';
    arrow.textContent = '←';
    const label = document.createElement('span');
    label.className = 'rc-label';
    label.textContent = '思考过程';
    head.appendChild(arrow);
    head.appendChild(label);
    const body = document.createElement('div');
    body.className = 'rc-body';
    body.hidden = true;
    card.appendChild(head);
    card.appendChild(body);
    turn.msg.insertBefore(card, turn.tools);
    head.addEventListener('click', () => {
      body.hidden = !body.hidden;
      arrow.textContent = body.hidden ? '←' : '↓';
      scrollChat();
    });
    currentReasoningEl = { body, arrow };
  }
  reasoningBuf += text;
  clearTimeout(reasoningTimer);
  reasoningTimer = setTimeout(() => {
    if (currentReasoningEl) {
      currentReasoningEl.body.textContent = reasoningBuf;
      scrollChat();
    }
  }, 120);
}

function resetReasoning() {
  clearTimeout(reasoningTimer);
  reasoningBuf = '';
  currentReasoningEl = null;
}

// ---------------- Agent 监控面板（上=当前运行，下=历史记录） ----------------
const runningPanel = $('#ap-running');
const historyPanel = $('#ap-history');
const subTools = new Map(); // task_id -> [{ name, ok, summary, input }]
const runningAgents = { main: null, sub: null };
const AGENT_LABEL = { main: '主 Agent', sub: '子 Agent' };

function apEmptyCheck() {
  const has = runningAgents.main || runningAgents.sub;
  let empty = runningPanel.querySelector('.ap-empty');
  if (has) {
    if (empty) empty.remove();
  } else {
    if (!empty) {
      const d = document.createElement('div');
      d.className = 'ap-empty';
      d.textContent = '当前无运行中的 Agent';
      runningPanel.appendChild(d);
    }
  }
}

function upsertAgentCard(agent) {
  let el = runningAgents[agent];
  if (el) return el;
  el = document.createElement('div');
  el.className = 'ap-agent';
  el.innerHTML =
    '<div class="ap-agent-head">' +
    '<span class="ap-agent-toggle">▸</span>' +
    '<span class="ap-agent-dot"></span>' +
    '<span class="ap-agent-name"></span>' +
    '<span class="ap-agent-status"></span>' +
    '</div>' +
    '<div class="ap-tools" hidden></div>' +
    '<div class="ap-progress" hidden></div>';
  // 运行中也可点开/收起查看详细工具步骤（默认折叠）
  const toggle = el.querySelector('.ap-agent-toggle');
  const tools = el.querySelector('.ap-tools');
  const prog = el.querySelector('.ap-progress');
  el.querySelector('.ap-agent-head').addEventListener('click', () => {
    const show = tools.hidden && prog.hidden;
    tools.hidden = !show;
    prog.hidden = !show;
    toggle.textContent = show ? '▾' : '▸';
  });
  runningPanel.appendChild(el);
  runningAgents[agent] = el;
  apEmptyCheck();
  return el;
}

function setAgentState(agent, status) {
  if (status === 'idle') {
    const el = runningAgents[agent];
    if (el) { el.remove(); runningAgents[agent] = null; }
    apEmptyCheck();
    return;
  }
  const el = upsertAgentCard(agent);
  el.querySelector('.ap-agent-dot').className = 'ap-agent-dot ' + (status === 'speaking' ? 'speaking' : 'working');
  el.querySelector('.ap-agent-status').textContent = status === 'speaking' ? '播报中' : '工作中';
  if (!el.querySelector('.ap-agent-name').textContent) {
    el.querySelector('.ap-agent-name').textContent = AGENT_LABEL[agent] || agent;
  }
}

function panelAgentStart(taskId, name) {
  setAgentState('sub', 'working');
  const el = upsertAgentCard('sub');
  el.dataset.taskId = taskId;
  el.querySelector('.ap-agent-name').textContent = '子 Agent · ' + (name || '任务');
  el.querySelector('.ap-tools').innerHTML = '';
  const prog = el.querySelector('.ap-progress');
  prog.hidden = true;
  prog.textContent = '';
}

function panelAgentProgress(taskId, message) {
  const el = runningAgents.sub;
  if (!el || el.dataset.taskId !== taskId) return;
  const prog = el.querySelector('.ap-progress');
  prog.hidden = false;
  prog.textContent = message;
}

function recordSubTool(taskId, tool) {
  let arr = subTools.get(taskId);
  if (!arr) { arr = []; subTools.set(taskId, arr); }
  const prev = arr.find((x) => x.name === tool.name);
  if (!prev) arr.push(tool);
  else if (tool.ok !== null) { prev.ok = tool.ok; prev.summary = tool.summary; }
  if (taskDetailOpenId === taskId) renderTaskDetail();
}

function panelSubToolUse(taskId, name, input) {
  const el = runningAgents.sub;
  if (!el || el.dataset.taskId !== taskId) return;
  const toolsBox = el.querySelector('.ap-tools');
  let mc = Array.from(toolsBox.children).find((n) => n.dataset.tool === name);
  if (!mc) {
    mc = document.createElement('div');
    mc.className = 'ap-mini-card';
    mc.dataset.tool = name;
    mc.innerHTML =
      '<div class="ap-mc-head"><span class="tc-toggle">▸</span><span class="tc-name">' + esc(titleCase(name)) + '</span></div>' +
      '<div class="ap-mc-body" hidden></div>';
    toolsBox.appendChild(mc);
    mc.querySelector('.ap-mc-head').addEventListener('click', () => {
      const b = mc.querySelector('.ap-mc-body');
      b.hidden = !b.hidden;
      mc.querySelector('.tc-toggle').textContent = b.hidden ? '▸' : '▾';
    });
  }
  const body = mc.querySelector('.ap-mc-body');
  body.innerHTML = fmtJson(input);
  body.hidden = false;
  recordSubTool(taskId, { name, ok: null, summary: '', input });
}

function panelSubToolResult(taskId, name, ok, summary) {
  const el = runningAgents.sub;
  if (el && el.dataset.taskId === taskId) {
    const mc = Array.from(el.querySelector('.ap-tools').children).find((n) => n.dataset.tool === name);
    if (mc) {
      mc.classList.remove('ok', 'err');
      mc.classList.add(ok ? 'ok' : 'err');
      const body = mc.querySelector('.ap-mc-body');
      body.textContent = body.textContent ? body.textContent + '\n---\n' + summary : summary;
      body.hidden = false;
    }
  }
  recordSubTool(taskId, { name, ok, summary, input: null });
}

function panelAgentDone(taskId, summary) {
  setAgentState('sub', 'idle');
  apHistoryAdd(taskId, summary || '', false);
}

function panelAgentError(taskId, message) {
  setAgentState('sub', 'idle');
  apHistoryAdd(taskId, message || '', true);
}

function apHistoryAdd(taskId, text, isErr) {
  const arr = subTools.get(taskId) || [];
  const first = arr.find((t) => t.ok !== null);
  const title = first ? first.name : (arr[0] ? arr[0].name : '子任务');
  const item = document.createElement('div');
  item.className = 'ap-hist-item';
  const toolHtml = arr.map((t) => {
    const st = t.ok === null ? 'loading' : (t.ok ? 'ok' : 'err');
    const sym = t.ok === null ? '' : (t.ok ? '✓' : '✗');
    return '<div class="ap-mini-card ' + (t.ok === null ? '' : (t.ok ? 'ok' : 'err')) + '">' +
      '<div class="ap-mc-head"><span class="tc-status ' + st + '">' + sym + '</span><span class="tc-name">' + esc(titleCase(t.name)) + '</span></div>' +
      (t.summary ? '<div class="ap-mc-body">' + esc(t.summary) + '</div>' : '') +
      '</div>';
  }).join('');
  item.innerHTML =
    '<div class="ap-hist-head"><span class="tc-toggle">▸</span>' +
    '<span class="ap-hist-name">' + esc(title) + '</span>' +
    '<span class="tag ' + (isErr ? 'err' : 'ok') + '">' + (isErr ? '出错' : '完成') + '</span></div>' +
    '<div class="ap-hist-body" hidden>' +
    (text ? '<div>' + esc(text) + '</div>' : '') +
    toolHtml +
    '</div>';
  item.querySelector('.ap-hist-head').addEventListener('click', () => {
    const b = item.querySelector('.ap-hist-body');
    b.hidden = !b.hidden;
    item.querySelector('.tc-toggle').textContent = b.hidden ? '▸' : '▾';
  });
  historyPanel.prepend(item);
  while (historyPanel.children.length > 30) historyPanel.lastChild.remove();
  loadTasks();
}

// 工具调用卡片（参照 goose ToolCallWithResponse：状态点 + 工具名 + 可展开参数/结果）
function addToolCard(agent, name, input) {
  const turn = ensureTurn();
  const key = name;
  const prev = turn.toolMap.get(key);
  if (prev) {
    // 同 turn 同名工具再次调用：追加输入记录
    prev.inputs.push(input);
    prev.inputEl.innerHTML = prev.inputs.map((i) => fmtJson(i)).join('<hr/>');
    prev.card.classList.add('loading');
    prev.statusEl.className = 'tc-status loading';
    prev.resultEl.hidden = true;
    return;
  }
  const card = document.createElement('div');
  card.className = 'tool-card loading';

  const head = document.createElement('div');
  head.className = 'tc-head';
  const toggle = document.createElement('span');
  toggle.className = 'tc-toggle';
  toggle.textContent = '▸';
  const statusEl = document.createElement('span');
  statusEl.className = 'tc-status loading';
  const nameEl = document.createElement('span');
  nameEl.className = 'tc-name';
  nameEl.textContent = (agent === 'sub' ? '[子] ' : '') + titleCase(name);

  head.appendChild(toggle);
  head.appendChild(statusEl);
  head.appendChild(nameEl);

  const body = document.createElement('div');
  body.className = 'tc-body';
  body.hidden = true;

  const inputEl = document.createElement('div');
  inputEl.className = 'tc-input';
  inputEl.innerHTML = fmtJson(input);

  const resultEl = document.createElement('div');
  resultEl.className = 'tc-result';
  resultEl.hidden = true;

  body.appendChild(inputEl);
  body.appendChild(resultEl);

  card.appendChild(head);
  card.appendChild(body);
  turn.tools.appendChild(card);

  head.addEventListener('click', () => {
    body.hidden = !body.hidden;
    toggle.textContent = body.hidden ? '▸' : '▾';
    scrollChat();
  });
  toggle.textContent = '▸';

  turn.toolMap.set(key, { card, statusEl, inputEl, resultEl, inputs: [input] });
  scrollChat();
}

function setToolResult(agent, name, ok, summary) {
  const turn = currentTurn;
  if (!turn) return;
  const entry = turn.toolMap.get(name);
  if (!entry) {
    // 结果先于卡片到达（异常情况）：直接渲染一张结果卡
    addToolCard(agent, name, {});
    setToolResult(agent, name, ok, summary);
    return;
  }
  entry.card.classList.remove('loading');
  entry.statusEl.className = 'tc-status ' + (ok ? 'ok' : 'err');
  entry.statusEl.textContent = ok ? '✓' : '✗';
  entry.resultEl.hidden = false;
  entry.resultEl.className = 'tc-result ' + (ok ? 'ok' : 'err');
  entry.resultEl.textContent = summary;
  scrollChat();
}

// 历史消息里的静态工具卡片（已完成态：✓/✗ + 名称 + 可展开参数/结果）
function makeToolCard(t) {
  const card = document.createElement('div');
  card.className = 'tool-card';
  const head = document.createElement('div');
  head.className = 'tc-head';
  const toggle = document.createElement('span');
  toggle.className = 'tc-toggle';
  toggle.textContent = '▸';
  const statusEl = document.createElement('span');
  statusEl.className = 'tc-status ' + (t.ok ? 'ok' : 'err');
  statusEl.textContent = t.ok ? '✓' : '✗';
  const nameEl = document.createElement('span');
  nameEl.className = 'tc-name';
  nameEl.textContent = titleCase(t.name);
  head.appendChild(toggle);
  head.appendChild(statusEl);
  head.appendChild(nameEl);
  const body = document.createElement('div');
  body.className = 'tc-body';
  body.hidden = true;
  const inputEl = document.createElement('div');
  inputEl.className = 'tc-input';
  inputEl.innerHTML = fmtJson(t.input);
  const resultEl = document.createElement('div');
  resultEl.className = 'tc-result ' + (t.ok ? 'ok' : 'err');
  resultEl.hidden = !t.summary;
  resultEl.textContent = t.summary || '';
  body.appendChild(inputEl);
  body.appendChild(resultEl);
  card.appendChild(head);
  card.appendChild(body);
  head.addEventListener('click', () => {
    body.hidden = !body.hidden;
    toggle.textContent = body.hidden ? '▸' : '▾';
    scrollChat();
  });
  return card;
}

function fmtJson(v) {
  if (v === undefined || v === null) return '';
  try {
    const s = typeof v === 'string' ? v : JSON.stringify(v, null, 2);
    return esc(s);
  } catch { return esc(String(v)); }
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
  showProcessing();
  pendingLocalUser = text;
  turnSessionId = currentSessionId;
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
function interruptAI() {
  apiPost('/api/chat/interrupt', {}).catch(() => {});
  setSendBtn(false);
  turnSessionId = null; // 本轮已被打断，不再当作“进行中”
}

$('#btn-send').addEventListener('click', () => {
  if (sendBusy) {
    interruptAI();
    return;
  }
  const v = $('#chat-input').value;
  $('#chat-input').value = '';
  sendText(v);
});
$('#chat-input').addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault();
    if (sendBusy) return; // 忙碌中 Enter 不重复发送
    const v = $('#chat-input').value;
    $('#chat-input').value = '';
    sendText(v);
  }
});

// 右侧 Agent 监控面板折叠/展开（右上角小按钮；折叠态面板完全收起，仅保留浮动展开按钮）
const apToggleBtn = $('#btn-ap-toggle');
const apOpenBtn = $('#btn-ap-open');
function setPanelCollapsed(collapsed) {
  const panel = $('#agent-panel');
  panel.classList.toggle('collapsed', collapsed);
  if (apOpenBtn) apOpenBtn.hidden = !collapsed;
  if (apToggleBtn) {
    const lbl = apToggleBtn.querySelector('.ap-toggle-label');
    if (lbl) lbl.textContent = collapsed ? '«' : '»';
  }
}
if (apToggleBtn) {
  apToggleBtn.addEventListener('click', () => {
    setPanelCollapsed(!$('#agent-panel').classList.contains('collapsed'));
  });
}
if (apOpenBtn) {
  apOpenBtn.addEventListener('click', () => setPanelCollapsed(false));
}

// ---------------- SSE 事件 ----------------
function openEvents() {
  const es = new EventSource(CORE_URL + '/api/events');
  es.onmessage = (m) => {
    let ev;
    try { ev = JSON.parse(m.data); } catch { return; }
    handleEvent(ev);
  };
  es.onerror = () => { /* 后端重启后自动重连 */ };
}

function handleEvent(ev) {
  switch (ev.type) {
    case 'report_text':
      // 子 Agent 汇报文本（独立事件，非用户消息）：以系统样式展示，不渲染成用户气泡
      if (!evForCurrentSession(ev)) break;
      addSystem(`🔊 子 Agent 汇报: ${ev.text}`);
      break;
    case 'user_text':
      if (!evForCurrentSession(ev)) break;
      finishTurn();
      if (pendingLocalUser === ev.text) {
        pendingLocalUser = null; // 已由 sendText 乐观渲染，跳过避免重复
        break;
      }
      pendingLocalUser = null;
      // 语音双发防护：同一文本 5 秒内重复到达视为重放，丢弃
      const now = Date.now();
      const last = lastUserTextAt.get(ev.text);
      if (last && now - last < 5000) break;
      lastUserTextAt.set(ev.text, now);
      // 语音等外部输入同样进入忙碌态（按钮变暂停可打断 AI）
      turnSessionId = ev.session_id || currentSessionId;
      setSendBtn(true);
      addUserBubble(ev.text);
      showProcessing(); // 助手侧显示“正在处理…”占位，首个 assistant_text 到达时移除
      break;
    case 'assistant_text':
      if (!evForCurrentSession(ev)) break;
      hideProcessing();
      // 后端发出的是"完整文本快照"而非增量片段：覆盖式渲染，
      // 避免多条快照用 += 拼接导致文本重复。
      ensureTurn().text.textContent = ev.text;
      scrollChat();
      break;
    case 'assistant_done':
      // 全局恢复按钮状态：同一时刻后端只有一个主任务在跑，
      // 任何会话的 done 都代表该任务结束，不因当前查看的会话不同而卡住按钮/占位
      setSendBtn(false);
      turnSessionId = null;
      if (!evForCurrentSession(ev)) break;
      hideProcessing();
      if (currentTurn) {
        // 工具调用后由 tool_use 创建的 currentTurn 气泡为空，回填最终文本；
        // 若已由 assistant_text 覆盖过则保持不动
        if (!currentTurn.text.textContent && ev.text) {
          currentTurn.text.textContent = ev.text;
        }
        finishTurn();
      } else if (ev.text) {
        // 无工具调用的纯文本回复（后端只发 assistant_done）：直接渲染最终文本
        const div = document.createElement('div');
        div.className = 'msg assistant';
        const bubble = document.createElement('div');
        bubble.className = 'bubble';
        bubble.textContent = ev.text;
        div.appendChild(bubble);
        chatScroll.appendChild(div);
      }
      scrollChat();
      break;
    case 'tool_use':
      if (!evForCurrentSession(ev)) break;
      addToolCard(ev.agent || 'main', ev.name, ev.input);
      break;
    case 'tool_result':
      if (!evForCurrentSession(ev)) break;
      setToolResult(ev.agent || 'main', ev.name, ev.ok, ev.summary);
      break;
    case 'reasoning_text':
      if (!evForCurrentSession(ev)) break;
      hideProcessing();
      addReasoningCard(ev.text);
      break;
    case 'voice':
      if (ev.kind === 'wakeword') { voiceLine(`唤醒词 "${ev.text}" 已触发`); }
      else if (ev.kind === 'stt') { voiceLine(`识别: ${ev.text}`); }
      else if (ev.kind === 'speak_start') { voiceLine('语音播报中…'); setAgentState('main', 'speaking'); }
      else if (ev.kind === 'speak_end') { voiceLine(''); }
      break;
    case 'agent_status':
      setAgentState(ev.agent, ev.status);
      break;
    case 'subagent_start':
      addSystem(`▶ 子任务 ${ev.name} (${(ev.task_id || '').slice(0, 8)}) 开始`);
      panelAgentStart(ev.task_id, ev.name);
      break;
    case 'subagent_progress':
      panelAgentProgress(ev.task_id, ev.message);
      break;
    case 'subagent_tool_use':
      panelSubToolUse(ev.task_id, ev.name, ev.input);
      break;
    case 'subagent_tool_result':
      panelSubToolResult(ev.task_id, ev.name, ev.ok, ev.summary);
      break;
    case 'subagent_done':
      addSystem(`✔ 子任务完成: ${ev.summary}`);
      panelAgentDone(ev.task_id, ev.summary);
      break;
    case 'subagent_error':
      addSystem(`✖ 子任务出错: ${ev.message}`);
      panelAgentError(ev.task_id, ev.message);
      break;
    case 'subagent_report_ready':
      addSystem(`🔊 子 Agent 汇报: ${ev.text}`);
      break;
    case 'schedule_triggered': addSystem(`⏰ 定时任务触发: ${ev.title}`); break;
    case 'memory_updated': loadMemory(); break;
    case 'skills_updated': loadSkills(); break;
    case 'schedules_updated': loadSchedules(); break;
    case 'settings_updated': flashSaved(); pollStatus(); break;
    default: break;
  }
}

function voiceLine(text) {
  const el = $('#voice-line');
  if (!text || !window.voiceEnabled) { el.hidden = true; return; }
  el.textContent = text; el.hidden = false;
}

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

// ---------------- 子任务 ----------------
// 子任务详情全屏弹层：点击任务后展示该任务的完整工具工作卡片
const taskDetailModal = $('#task-detail-modal');
const tdmTitle = $('#tdm-title');
const tdmTaskid = $('#tdm-taskid');
const tdmBody = $('#tdm-body');
let taskDetailOpenId = null;

function openTaskDetail(taskId, name) {
  taskDetailOpenId = taskId;
  tdmTitle.textContent = name || '子 Agent 任务';
  tdmTaskid.textContent = taskId ? '#' + taskId : '';
  taskDetailModal.hidden = false;
  renderTaskDetail();
}

function closeTaskDetail() {
  taskDetailOpenId = null;
  taskDetailModal.hidden = true;
}

function renderTaskDetail() {
  if (!taskDetailOpenId) return;
  // 保留用户已展开的卡片状态，避免实时刷新时重置
  const expanded = new Set();
  tdmBody.querySelectorAll('.tool-card .tc-body:not([hidden])').forEach((b) => {
    const c = b.closest('.tool-card');
    if (c && c.dataset.name) expanded.add(c.dataset.name);
  });
  tdmBody.innerHTML = '';
  const tools = subTools.get(taskDetailOpenId) || [];
  if (!tools.length) {
    const hint = document.createElement('div');
    hint.className = 'hint';
    hint.textContent = '暂无工具记录（仅本会话内运行中/刚完成的任务保留工具明细；重启后历史任务仅剩摘要）。';
    tdmBody.appendChild(hint);
    return;
  }
  tools.forEach((t) => {
    const card = document.createElement('div');
    card.className = 'tool-card' + (t.ok === null ? ' loading' : '');
    card.dataset.name = t.name;
    const head = document.createElement('div');
    head.className = 'tc-head';
    const toggle = document.createElement('span');
    toggle.className = 'tc-toggle';
    toggle.textContent = '▸';
    const statusEl = document.createElement('span');
    statusEl.className = 'tc-status ' + (t.ok === null ? 'loading' : (t.ok ? 'ok' : 'err'));
    statusEl.textContent = t.ok === null ? '' : (t.ok ? '✓' : '✗');
    const nameEl = document.createElement('span');
    nameEl.className = 'tc-name';
    nameEl.textContent = titleCase(t.name);
    head.appendChild(toggle);
    head.appendChild(statusEl);
    head.appendChild(nameEl);
    const body = document.createElement('div');
    body.className = 'tc-body';
    body.hidden = !expanded.has(t.name);
    const inputEl = document.createElement('div');
    inputEl.className = 'tc-input';
    inputEl.innerHTML = fmtJson(t.input);
    body.appendChild(inputEl);
    if (t.summary) {
      const resultEl = document.createElement('div');
      resultEl.className = 'tc-result ' + (t.ok ? 'ok' : 'err');
      resultEl.textContent = t.summary;
      body.appendChild(resultEl);
    }
    card.appendChild(head);
    card.appendChild(body);
    head.addEventListener('click', () => {
      body.hidden = !body.hidden;
      toggle.textContent = body.hidden ? '▸' : '▾';
    });
    if (!body.hidden) toggle.textContent = '▾';
    tdmBody.appendChild(card);
  });
}

$('#tdm-close').addEventListener('click', closeTaskDetail);
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && !taskDetailModal.hidden) closeTaskDetail();
});

async function loadTasks() {
  try {
    const list = await apiGet('/api/tasks');
    const box = $('#task-list');
    box.innerHTML = '';
    if (!list.length) {
      box.innerHTML = '<div class="hint">暂无子任务记录</div>';
      return;
    }
    list.forEach((t) => {
      const st = t.status || 'pending';
      const tagCls = st === 'done' ? 'ok' : st === 'running' ? 'run' : st === 'error' ? 'err' : '';
      const item = document.createElement('div');
      item.className = 'list-item task-item';
      item.innerHTML = `
        <div class="row">
          <span class="title">${esc(t.name || '(未命名)')}</span>
          <span class="tag ${tagCls}">${esc(st)}</span>
        </div>
        <div class="meta">id: ${esc(t.task_id || t.id || '')} · ${esc(t.created_at || '')}</div>
        <div class="body">${esc(t.summary || t.result || t.error || t.progress || '')}</div>
        <div class="meta task-open-hint">点击查看完整工具记录 →</div>`;
      item.addEventListener('click', () => openTaskDetail(t.task_id || t.id, t.name));
      box.appendChild(item);
    });
  } catch (e) { /* ignore */ }
}

$('#btn-tasks-refresh').addEventListener('click', loadTasks);

// ---------------- 设置 ----------------
async function loadConfig() {
  try {
    const cfg = await apiGet('/api/config');
    const form = $('#settings-form');
    form.main_base_url.value = cfg.main_base_url || '';
    form.main_api_key.value = cfg.main_api_key || '';
    form.main_model.value = cfg.main_model || '';
    form.main_system_prompt.value = cfg.main_system_prompt || '';
    form.sub_base_url.value = cfg.sub_base_url || '';
    form.sub_api_key.value = cfg.sub_api_key || '';
    form.sub_model.value = cfg.sub_model || '';
    form.sub_system_prompt.value = cfg.sub_system_prompt || '';
    form.voice_host.value = cfg.voice_host || '127.0.0.1';
    form.voice_port.value = cfg.voice_port ?? 8420;
    form.wakeword.value = cfg.wakeword || 'computer';
    form.beep_file.value = cfg.beep_file || '';
    form.voice.value = cfg.voice || '';
    form.rate.value = cfg.rate ?? 1.0;
    form.tts_backend.value = cfg.tts_backend || 'internal';
    form.goose_tts_path.value = cfg.goose_tts_path || '';
    form.interrupt_keywords.value = (cfg.interrupt_keywords || []).join(',');
    form.kws_threshold.value = cfg.kws_threshold ?? 0.15;
    form.max_record_secs.value = cfg.max_record_secs ?? 120;
    form.max_turns.value = cfg.max_turns ?? 1000;
    form.voice_enabled.checked = !!cfg.voice_enabled;
    window.voiceEnabled = !!cfg.voice_enabled;
    form.show_kws_log.checked = getKwsLogEnabled();
    form.skill_dirs.value = (cfg.skill_dirs || []).join('\n');
  } catch (e) { /* ignore */ }
}

function flashSaved() {
  const el = $('#settings-saved');
  el.hidden = false;
  setTimeout(() => { el.hidden = true; }, 2000);
}

$('#settings-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const f = e.target;
  // 唤醒日志显隐是本地 UI 偏好（默认关闭），不入后端配置
  localStorage.setItem('kws_log_enabled', f.show_kws_log.checked ? '1' : '0');
  if (!f.show_kws_log.checked) kwsLog.hidden = true;
  const cfg = {
    main_base_url: f.main_base_url.value.trim(),
    main_api_key: f.main_api_key.value.trim(),
    main_model: f.main_model.value.trim(),
    main_system_prompt: f.main_system_prompt.value,
    sub_base_url: f.sub_base_url.value.trim(),
    sub_api_key: f.sub_api_key.value.trim(),
    sub_model: f.sub_model.value.trim(),
    sub_system_prompt: f.sub_system_prompt.value,
    voice_host: f.voice_host.value.trim() || '127.0.0.1',
    voice_port: parseInt(f.voice_port.value, 10) || 8420,
    wakeword: f.wakeword.value.trim() || 'computer',
    beep_file: f.beep_file.value.trim(),
    voice: f.voice.value.trim(),
    rate: parseFloat(f.rate.value) || 1.0,
    tts_backend: f.tts_backend.value,
    goose_tts_path: f.goose_tts_path.value.trim(),
    interrupt_keywords: f.interrupt_keywords.value.split(/[,，]/).map((s) => s.trim()).filter(Boolean),
    kws_threshold: parseFloat(f.kws_threshold.value) || 0.15,
    max_record_secs: parseFloat(f.max_record_secs.value) || 120,
    max_turns: parseInt(f.max_turns.value, 10) || 1000,
    voice_enabled: f.voice_enabled.checked,
    skill_dirs: f.skill_dirs.value.split('\n').map((s) => s.trim()).filter(Boolean),
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
loadTasks();
loadConfig();
loadSessions();
kwsLogTimer = setInterval(pollKwsLog, 3000);
openEvents();
setInterval(loadTasks, 8000);
