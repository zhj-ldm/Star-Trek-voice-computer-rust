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

// 对话流事件仅渲染当前会话；session_id 为空 = 全局事件（语音），不拦截
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
    /* 单 Agent 架构：无子任务弹层 */
  });
});

// ---------------- 状态轮询 ----------------
let connected = false;
let statusTimer = null;

// 发送按钮双态：空闲=箭头（发送），有任务=暂停（点击打断 AI）
let sendBusy = false;
let mainBusy = false;      // 后端 main_busy（轮询兜底）
let turnSessionId = null;  // 当前正在回复的会话 id（用于切换会话时判断）
let backendTurnSession = null; // 后端权威：当前主任务所属会话（/api/status turn_session）
let interruptPending = false; // 已请求打断、等待后端确认停止（避免按钮闪烁回"发送"）
let showReasoning = localStorage.getItem('st_show_reasoning') !== '0'; // 设置→显示 AI 思考过程
// 每个会话一份实时 turn 快照（per-session）：切走再切回时用它重建
// "AI 工作中"的思考卡/工具卡/已出文本（尚未持久化，历史里没有）；
// done 后保留最终文本，作为"切回时历史尚未刷新"的兜底补齐来源。
const turnSnapshots = new Map(); // sessionId -> { text, tools: Map(name->{name,inputs,ok,summary}), reasoning: [], done }

function snapFor(sessionId) {
  let s = turnSnapshots.get(sessionId);
  if (!s) { s = { text: '', tools: new Map(), reasoning: [], done: false }; turnSnapshots.set(sessionId, s); }
  return s;
}
function cacheTurnReset(sessionId) {
  turnSnapshots.set(sessionId, { text: '', tools: new Map(), reasoning: [], done: false });
}
function cacheTurnText(sessionId, t) { snapFor(sessionId).text = t; }
function cacheTurnReasoning(sessionId, t) { snapFor(sessionId).reasoning.push(t); }
function cacheTurnToolUse(sessionId, name, input) {
  const e = snapFor(sessionId).tools.get(name) || { name, inputs: [], ok: null, summary: '' };
  e.inputs.push(input);
  snapFor(sessionId).tools.set(name, e);
}
function cacheTurnToolResult(sessionId, name, ok, summary) {
  const e = snapFor(sessionId).tools.get(name);
  if (e) { e.ok = ok; e.summary = summary; }
}
function cacheTurnDone(sessionId) { snapFor(sessionId).done = true; }

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
      if (m.tools && m.tools.length) {
        const stack = document.createElement('div');
        stack.className = 'tool-stack';
        m.tools.forEach((t) => stack.appendChild(makeToolCard(t)));
        div.appendChild(stack);
      }
      const bubble = document.createElement('div');
      bubble.className = 'bubble';
      bubble.innerHTML = renderMd(m.text);
      div.appendChild(bubble);
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
        const stack = document.createElement('div');
        stack.className = 'tool-stack';
        snap.tools.forEach((e) => stack.appendChild(buildCachedToolCard(e).card));
        div.appendChild(stack);
        const bubble = document.createElement('div');
        bubble.className = 'bubble';
        bubble.innerHTML = renderMd(snap.text);
        div.appendChild(bubble);
        chat.appendChild(div);
      }
    }
  }
  chat.scrollTop = chat.scrollHeight;
}

// 用缓存重建进行中的助手消息块（切会话回来时调用）
function renderLiveTurnBlock(cache) {
  const chat = $('#chat-scroll');
  // 尚未产出任何内容（思考/文本/工具都没有）：直接沿用"实时"占位
  // （"正在思考" + 三点跳动动画），而不是静态的"正在处理…"。
  // 后者无动画、DOM 结构也不同，切回会话时观感像对话卡死/断掉。
  if (!cache.text && !cache.reasoning.length && cache.tools.size === 0) {
    showProcessing();
    return;
  }
  const div = document.createElement('div');
  div.className = 'msg assistant';
  const tools = document.createElement('div');
  tools.className = 'tool-stack';
  const bubble = document.createElement('div');
  bubble.className = 'bubble';
  // 思考卡（沿用 addReasoningCard 的 DOM 契约，后续 reasoning_text 直接续写）
  let reasoningEl = null;
  if (showReasoning && cache.reasoning.length) {
    const rc = document.createElement('div');
    rc.className = 'reasoning-card';
    const head = document.createElement('div');
    head.className = 'rc-head';
    const arrow = document.createElement('span');
    arrow.className = 'rc-arrow';
    arrow.textContent = '▸';
    const label = document.createElement('span');
    label.className = 'rc-label';
    label.textContent = '正在思考…'; // 折叠态一行浅色字；思维链内容点击才展开
    head.appendChild(arrow);
    head.appendChild(label);
    const body = document.createElement('div');
    body.className = 'rc-body';
    body.textContent = cache.reasoning.join('');
    rc.appendChild(head);
    rc.appendChild(body);
    head.addEventListener('click', () => { rc.classList.toggle('open'); });
    div.appendChild(rc);
    reasoningEl = { body, arrow };
  }
  // 工具卡：同时登记进 toolMap，保证后续 tool_result 就地更新（而非重复插卡）、
  // tool_use 同名合并、finishTurn 的"完成 N 个工具调用"统计都正确。
  const toolMap = new Map();
  cache.tools.forEach((e) => {
    const refs = buildCachedToolCard(e);
    tools.appendChild(refs.card);
    toolMap.set(e.name, { ...refs, inputs: e.inputs.slice(), startTs: Date.now() });
  });
  div.appendChild(tools);
  // 文本：有快照文本则渲染；否则保持空气泡（与实时一致，等 assistant_text 到达）
  if (cache.text) bubble.innerHTML = renderMd(cache.text);
  div.appendChild(bubble);
  chat.appendChild(div);
  // 登记为 currentTurn，让后续 SSE 事件（assistant_text/tool_use/…）直接续写本块
  currentTurn = { msg: div, text: bubble, tools, toolMap };
  currentReasoningEl = reasoningEl;
  reasoningBuf = reasoningEl ? cache.reasoning.join('') : '';
}

// 缓存工具条 → 静态工具行（与实时工具行同一视觉语言；loading 态标题光线波动）
function buildCachedToolCard(e) {
  const isSpeak = e.name === 'SpeakToUser';
  const loading = e.ok === null;
  const inputHtml = isSpeak
    ? e.inputs.map((i) => esc((i && i.text) || jsonHtml(i))).join('<hr/>')
    : e.inputs.map((i) => jsonHtml(i)).join('<hr/>');
  const { card, titleBtn, argsEl, resultEl, durEl } = dbStepSkeleton(isSpeak ? '语音播报' : toolTitleFromInputs(e.name, e.inputs), inputHtml);
  if (loading) {
    titleBtn.classList.add('is-running');
    if (durEl) durEl.textContent = '耗时 —';
  } else {
    card.classList.toggle('is-err', !e.ok);
    if (durEl) durEl.textContent = '耗时 ' + (e.dur || '—');
  }
  resultEl.innerHTML = esc(e.summary || '');
  return { card, titleBtn, argsEl, resultEl, durEl };
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
  // 任务完成：整轮工具调用折叠成一个汇总行（"完成 N 个工具调用 · 用时 X 秒"）
  if (currentTurn) collapseTurnTools(currentTurn);
  currentTurn = null;
  resetReasoning();
}

// 任务完成后把整轮工具调用折叠成一个汇总行（豆包式收束）。
// 保留原始 db-step 工具行（含展开面板），汇总行点击展开/收起。
function collapseTurnTools(turn) {
  const steps = turn.tools ? turn.tools.querySelectorAll('.db-step') : [];
  if (!steps.length) return;
  const count = turn.toolMap ? turn.toolMap.size : steps.length;
  // 总耗时：最早开始的工具 -> 现在
  let firstTs = Infinity;
  if (turn.toolMap) {
    turn.toolMap.forEach((e) => { if (e.startTs && e.startTs < firstTs) firstTs = e.startTs; });
  }
  const secs = Math.max(1, Math.round((Date.now() - (firstTs === Infinity ? Date.now() : firstTs)) / 1000));

  const wrap = document.createElement('div');
  wrap.className = 'db-summary';
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'db-summary__title';
  btn.innerHTML = '完成 ' + count + ' 个工具调用 · 用时 ' + secs + ' 秒' + DB_CHEV;
  const holder = document.createElement('div');
  holder.className = 'db-summary__steps';
  holder.style.display = 'none';
  steps.forEach((st) => holder.appendChild(st));
  wrap.appendChild(btn);
  wrap.appendChild(holder);
  turn.tools.innerHTML = '';
  turn.tools.appendChild(wrap);
  btn.addEventListener('click', () => {
    const willOpen = holder.style.display === 'none';
    holder.style.display = willOpen ? '' : 'none';
    wrap.classList.toggle('is-open', willOpen);
  });
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
    arrow.textContent = '▸';
    const label = document.createElement('span');
    label.className = 'rc-label';
    label.textContent = '正在思考…'; // 折叠态一行浅色字；思维链内容点击才展开
    head.appendChild(arrow);
    head.appendChild(label);
    const body = document.createElement('div');
    body.className = 'rc-body';
    card.appendChild(head);
    card.appendChild(body);
    turn.msg.insertBefore(card, turn.tools);
    head.addEventListener('click', () => {
      card.classList.toggle('open');
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
      ? '<span class="db-json__k">"' + esc(m[1]) + '"' + m[2] + '</span>'
      : '<span class="db-json__s">"' + esc(m[1]) + '"</span>';
    last = re.lastIndex;
  }
  if (last < text.length) out += esc(text.slice(last));
  return out;
}

const DB_CHEV = '<svg class="db-ic db-step__chev" viewBox="64 64 896 896" aria-hidden="true"><path d="M765.7 486.8L314.9 134.7A7.97 7.97 0 00302 141v77.3c0 4.9 2.3 9.6 6.1 12.6l360 281.1-360 281.1c-3.9 3-6.1 7.7-6.1 12.6V883c0 6.7 7.7 10.4 12.9 6.3l450.8-352.1a31.96 31.96 0 000-50.4z"/></svg>';
const DB_COPY = '<svg class="db-ic" viewBox="64 64 896 896" aria-hidden="true"><path d="M832 64H296c-4.4 0-8 3.6-8 8v56c0 4.4 3.6 8 8 8h496v688c0 4.4 3.6 8 8 8h56c4.4 0 8-3.6 8-8V96c0-17.7-14.3-32-32-32zM704 192H192c-17.7 0-32 14.3-32 32v530.7c0 8.5 3.4 16.6 9.4 22.6l173.3 173.3c2.2 2.2 4.7 4 7.4 5.5v1.9h4.2c3.5 1.3 7.2 2 11 2H704c17.7 0 32-14.3 32-32V224c0-17.7-14.3-32-32-32zM350 856.2L263.9 770H350v86.2zM664 888H414V746c0-22.1-17.9-40-40-40H232V264h432v624z"/></svg>';
const DB_CHECK = '<svg class="db-ic" viewBox="64 64 896 896" aria-hidden="true"><path d="M912 190h-69.9c-9.8 0-19.1 4.5-25.1 12.2L404.7 724.5 207 474a32 32 0 00-25.1-12.2H112c-6.7 0-10.4 7.7-6.3 12.9l273.9 347c12.8 16.2 37.4 16.2 50.3 0l488.4-618.9c4.1-5.1.4-12.8-6.3-12.8z"/></svg>';

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

// db-step 工具行骨架（HTML 结构 = doubao toolRowHtml；返回 {card,titleBtn,resultEl,durEl}）
function dbStepSkeleton(title, inputHtml) {
  const card = document.createElement('div');
  card.className = 'db-step';

  const titleBtn = document.createElement('button');
  titleBtn.type = 'button';
  titleBtn.className = 'db-step__title';
  titleBtn.innerHTML = esc(title) + DB_CHEV;

  const panel = document.createElement('div');
  panel.className = 'db-step__panel';
  const panelInner = document.createElement('div');
  const detail = document.createElement('div');
  detail.className = 'db-detail';

  const argsRow = document.createElement('div');
  argsRow.className = 'db-detail__row';
  const argsLabel = document.createElement('span');
  argsLabel.className = 'db-detail__label';
  argsLabel.textContent = '输入参数';
  const argsEl = document.createElement('pre');
  argsEl.className = 'db-json';
  argsEl.innerHTML = inputHtml || '';
  if (!inputHtml) argsRow.style.display = 'none';
  argsRow.appendChild(argsLabel);
  argsRow.appendChild(argsEl);

  const resRow = document.createElement('div');
  resRow.className = 'db-detail__row';
  const resLabel = document.createElement('span');
  resLabel.className = 'db-detail__label';
  resLabel.textContent = '执行结果';
  const resultEl = document.createElement('p');
  resultEl.className = 'db-detail__text';
  resultEl.setAttribute('data-role', 'result');
  resultEl.innerHTML = '<span class="db-detail__pending">执行中…</span>';
  resRow.appendChild(resLabel);
  resRow.appendChild(resultEl);

  const metaEl = document.createElement('div');
  metaEl.className = 'db-detail__meta';
  const durEl = document.createElement('span');
  durEl.setAttribute('data-role', 'dur');
  durEl.textContent = '耗时 —';
  const dot = document.createElement('span');
  dot.className = 'db-detail__dot';
  dot.textContent = '·';
  const env = document.createElement('span');
  env.textContent = '本地 · Agent 工具';
  const copyBtn = document.createElement('button');
  copyBtn.type = 'button';
  copyBtn.className = 'db-detail__copy';
  copyBtn.innerHTML = DB_COPY + '复制结果';
  copyBtn.addEventListener('click', () => {
    const text = title + '\n' + (argsEl.textContent || '') + '\n→ ' + (resultEl.textContent || '');
    if (navigator.clipboard) navigator.clipboard.writeText(text).catch(() => {});
    copyBtn.innerHTML = DB_CHECK + '已复制';
    setTimeout(() => { copyBtn.innerHTML = DB_COPY + '复制结果'; }, 1600);
  });
  metaEl.appendChild(durEl);
  metaEl.appendChild(dot);
  metaEl.appendChild(env);
  metaEl.appendChild(copyBtn);

  detail.appendChild(argsRow);
  detail.appendChild(resRow);
  detail.appendChild(metaEl);
  panelInner.appendChild(detail);
  panel.appendChild(panelInner);
  card.appendChild(titleBtn);
  card.appendChild(panel);

  titleBtn.addEventListener('click', () => {
    card.classList.toggle('is-open');
  });
  return { card, titleBtn, argsEl, resultEl, durEl };
}

// 实时工具调用行
function addToolCard(agent, name, input) {
  const isSpeak = name === 'SpeakToUser';
  const turn = ensureTurn();
  const key = name;
  const prev = turn.toolMap.get(key);
  if (prev) {
    // 同 turn 同名工具再次调用：追加输入记录，回到执行态
    prev.inputs.push(input);
    prev.argsEl.innerHTML = prev.inputs.map((i) => jsonHtml(i)).join('<hr/>');
    prev.titleBtn.classList.add('is-running');
    prev.titleBtn.classList.remove('is-err');
    prev.startTs = Date.now();
    if (prev.durEl) prev.durEl.textContent = '耗时 —';
    prev.resultEl.innerHTML = '<span class="db-detail__pending">执行中…</span>';
    return;
  }
  const title = isSpeak ? '语音播报' : toolTitleFromInputs(name, input);
  const inputHtml = isSpeak
    ? (input && input.text ? esc(String(input.text)) : jsonHtml(input))
    : jsonHtml(input);
  const { card, titleBtn, argsEl, resultEl, durEl } = dbStepSkeleton(title, inputHtml);
  titleBtn.classList.add('is-running');
  if (isSpeak) card.classList.add('speak');
  turn.tools.appendChild(card);
  turn.toolMap.set(key, { card, titleBtn, argsEl, resultEl, durEl, inputs: [input], startTs: Date.now() });
  scrollChat();
}

function setToolResult(agent, name, ok, summary) {
  const turn = currentTurn;
  if (!turn) return;
  const entry = turn.toolMap.get(name);
  if (!entry) {
    // 结果先于工具行到达（异常情况）：补一张结果行
    addToolCard(agent, name, {});
    setToolResult(agent, name, ok, summary);
    return;
  }
  entry.titleBtn.classList.remove('is-running');
  entry.titleBtn.classList.toggle('is-err', !ok);
  const secs = Math.max(1, Math.round((Date.now() - entry.startTs) / 1000));
  if (entry.durEl) entry.durEl.textContent = '耗时 ' + secs + ' 秒';
  entry.resultEl.innerHTML = esc(summary || (ok ? '' : '（无结果）'));
  scrollChat();
}

// 历史消息里的静态工具行（已完成态）
function makeToolCard(t) {
  const isSpeak = t.name === 'SpeakToUser';
  const inputHtml = isSpeak
    ? (t.input && t.input.text ? esc(String(t.input.text)) : jsonHtml(t.input))
    : jsonHtml(t.input);
  const { card, titleBtn, resultEl, durEl } = dbStepSkeleton(isSpeak ? '语音播报' : toolTitleFromInputs(t.name, t.input), inputHtml);
  if (t.ok === undefined) {
    titleBtn.classList.add('is-running');
    if (durEl) durEl.textContent = '耗时 —';
  } else {
    card.classList.toggle('is-err', !t.ok);
    if (durEl) durEl.textContent = '耗时 ' + (t.dur || '—');
  }
  resultEl.innerHTML = esc(t.summary || '');
  return card;
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
      showProcessing(); // 助手侧显示“正在处理…”占位，首个 assistant_text 到达时移除
      break;
    case 'assistant_text':
      cacheTurnText(ev.session_id || currentSessionId, ev.text);
      if (!evForCurrentSession(ev)) break;
      hideProcessing();
      // 后端发出的是"完整文本快照"而非增量片段：覆盖式渲染，
      // 避免多条快照用 += 拼接导致文本重复。
      const textTurn = ensureTurn();
      textTurn.text.style.display = ''; // 有文本时确保气泡可见
      textTurn.text.innerHTML = renderMd(ev.text);
      scrollChat();
      break;
    case 'assistant_done':
      // 全局恢复按钮状态：同一时刻后端只有一个主任务在跑，
      // 任何会话的 done 都代表该任务结束，不因当前查看的会话不同而卡住按钮/占位
      setSendBtn(false);
      turnSessionId = null;
      interruptPending = false;
      cacheTurnDone(ev.session_id || currentSessionId); // 标记快照完成（保留最终文本兜底）
      if (!evForCurrentSession(ev)) break;
      hideProcessing();
      if (currentTurn) {
        // 工具调用后由 tool_use 创建的 currentTurn 气泡为空，回填最终文本；
        // 若已由 assistant_text 覆盖过则保持不动
        if (!currentTurn.text.textContent && ev.text) {
          currentTurn.text.innerHTML = renderMd(ev.text);
        }
        finishTurn();
      } else if (ev.text) {
        // 无工具调用的纯文本回复（后端只发 assistant_done）：直接渲染最终文本
        const div = document.createElement('div');
        div.className = 'msg assistant';
        const bubble = document.createElement('div');
        bubble.className = 'bubble';
        bubble.innerHTML = renderMd(ev.text);
        div.appendChild(bubble);
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
      if (!showReasoning) break; // 设置里关闭"显示思考过程"
      cacheTurnReasoning(ev.session_id || currentSessionId, ev.text);
      if (!evForCurrentSession(ev)) break;
      hideProcessing();
      addReasoningCard(ev.text);
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

// ---- 多模型配置：选择框 + 当前配置编辑区（可新建/删除/切换）----
let modelList = [];
let activeModelIdx = 0;

function modelEditHTML(m, idx) {
  const keys = (m.api_keys && m.api_keys.length) ? m.api_keys : (m.api_key ? [m.api_key] : ['']);
  let ks = '<div class="key-list">';
  keys.forEach((k, i) => { ks += keyRowHTML(k, i); });
  ks += '<button type="button" class="key-add" title="添加一个同配置 API Key">＋ 添加一个 Key</button></div>';
  return (
    '<div class="me-head">' +
    '<span class="pc-badge">模型 ' + (idx + 1) + '</span>' +
    '<label class="me-name-wrap">名称 <input class="me-name" placeholder="如 主 API / 本地 Ollama" /></label>' +
    '</div>' +
    '<div class="pc-grid">' +
    '<label class="pc-url-wrap">Base URL <input class="me-url" placeholder="OpenAI 兼容端点，如 https://api.openai.com/v1（带不带 /v1 均可）" /></label>' +
    '<label class="pc-model-wrap">模型名 <input class="me-model" placeholder="如 gpt-4o / qwen2.5 / deepseek-chat" /></label>' +
    '<label>每个 Key 限额 RPM <input class="me-rpm" type="number" min="1" step="1" placeholder="20" /></label>' +
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
  sel.innerHTML = '';
  if (!modelList.length) {
    const opt = document.createElement('option');
    opt.value = '0';
    opt.textContent = '（无模型配置）';
    sel.appendChild(opt);
  } else {
    modelList.forEach((m, i) => {
      const opt = document.createElement('option');
      opt.value = String(i);
      opt.textContent = (m.name || ('模型 ' + (i + 1)));
      sel.appendChild(opt);
    });
    sel.value = String(Math.min(activeModelIdx, modelList.length - 1));
  }
  renderModelEdit();
}

function renderModelEdit() {
  const box = $('#model-edit');
  if (!modelList.length) {
    box.innerHTML = '<p class="form-hint">还没有模型配置，点「＋ 新建」添加第一套。</p>';
    return;
  }
  const idx = Math.min(activeModelIdx, modelList.length - 1);
  const m = modelList[idx];
  box.innerHTML = modelEditHTML(m, idx);
  box.querySelector('.me-name').value = m.name || '';
  box.querySelector('.me-url').value = m.base_url || '';
  box.querySelector('.me-model').value = m.model || '';
  box.querySelector('.me-rpm').value = m.rpm_limit ?? '';
  wireKeyList(box);
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
    form.goose_tts_path.value = cfg.goose_tts_path || '';
    form.interrupt_keywords.value = (cfg.interrupt_keywords || []).join(',');
    form.kws_threshold.value = String(Number(((cfg.kws_threshold ?? 0.15)).toFixed(2)));
    form.max_record_secs.value = cfg.max_record_secs ?? 120;
    form.max_turns.value = cfg.max_turns ?? 1000;
    form.rpm_limit.value = cfg.rpm_limit ?? 20;
    form.enable_thinking.checked = !!cfg.enable_thinking;
    form.voice_enabled.checked = !!cfg.voice_enabled;
    window.voiceEnabled = !!cfg.voice_enabled;
    form.ask_user_enabled.checked = cfg.ask_user_enabled !== false;
    form.show_kws_log.checked = getKwsLogEnabled();
    form.show_reasoning.checked = showReasoning;
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

$('#model-select').addEventListener('change', (e) => {
  activeModelIdx = parseInt(e.target.value, 10) || 0;
  renderModelEdit();
});

$('#btn-model-new').addEventListener('click', () => {
  modelList.push({ name: '新模型 ' + (modelList.length + 1), base_url: '', model: '', api_keys: [''], api_key: '', rpm_limit: null });
  activeModelIdx = modelList.length - 1;
  renderModelSelector();
  const url = document.querySelector('#model-edit .me-url');
  if (url) url.focus();
});

$('#btn-model-del').addEventListener('click', () => {
  if (!modelList.length) return;
  modelList.splice(activeModelIdx, 1);
  if (activeModelIdx >= modelList.length) activeModelIdx = Math.max(0, modelList.length - 1);
  renderModelSelector();
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
  if (modelList.length) {
    const edit = $('#model-edit');
    const cur = modelList[Math.min(activeModelIdx, modelList.length - 1)];
    const keys = Array.from(edit.querySelectorAll('.p-key')).map((el) => el.value.trim()).filter(Boolean);
    cur.name = edit.querySelector('.me-name').value.trim() || ('模型 ' + (activeModelIdx + 1));
    cur.base_url = edit.querySelector('.me-url').value.trim();
    cur.model = edit.querySelector('.me-model').value.trim();
    cur.rpm_limit = edit.querySelector('.me-rpm').value ? parseInt(edit.querySelector('.me-rpm').value, 10) : null;
    cur.api_keys = keys;
    cur.api_key = keys[0] || '';
  }
  const providerRows = modelList.filter((p) => p.base_url);
  const activeModelName = modelList.length
    ? (modelList[Math.min(activeModelIdx, modelList.length - 1)].name || '')
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
    goose_tts_path: f.goose_tts_path.value.trim(),
    interrupt_keywords: f.interrupt_keywords.value.split(/[,，]/).map((s) => s.trim()).filter(Boolean),
    kws_threshold: Math.round((parseFloat(f.kws_threshold.value) || 0.15) * 100) / 100,
    max_record_secs: parseFloat(f.max_record_secs.value) || 120,
    max_turns: parseInt(f.max_turns.value, 10) || 1000,
    rpm_limit: parseInt(f.rpm_limit.value, 10) || 20,
    enable_thinking: f.enable_thinking.checked,
    voice_enabled: f.voice_enabled.checked,
    ask_user_enabled: f.ask_user_enabled.checked,
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
openEvents();
