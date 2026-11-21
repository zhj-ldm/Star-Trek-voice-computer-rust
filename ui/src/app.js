// 渲染进程主逻辑：导航、状态轮询、SSE 事件、各面板 CRUD。
// 对话流：用户气泡 / 助手气泡（内含工具调用卡片）+ 底部输入卡。
// 语音链路为纯 Rust 后端采集（cpal→KWS→STT→LLM→TTS），前端只做 UI 展示。
'use strict';

const $ = (sel) => document.querySelector(sel);
// 不依赖 preload：window.star 注入成功则用之，否则回退默认本地端口
const VOICE = (window.star && window.star.voiceUrl) || 'http://127.0.0.1:8420';
const CORE_URL = (window.star && window.star.coreUrl) || 'http://127.0.0.1:8410';
console.log(`[app] VOICE=${VOICE} core=${CORE_URL}`);

// ---------------- Toast 轻提示（替代原生 alert，不打断操作流） ----------------
function toast(message, kind = 'info', ms) {
  let root = document.getElementById('toast-root');
  if (!root) {
    root = document.createElement('div');
    root.id = 'toast-root';
    root.className = 'toast-root';
    root.setAttribute('aria-live', 'polite');
    document.body.appendChild(root);
  }
  while (root.children.length >= 4) root.firstChild.remove();
  const el = document.createElement('div');
  el.className = 'toast toast-' + kind;
  el.setAttribute('role', kind === 'error' ? 'alert' : 'status');
  const tx = document.createElement('span');
  tx.className = 'toast-text';
  tx.textContent = message;
  el.appendChild(tx);
  const life = ms || (kind === 'error' ? 6000 : 3200);
  let timer = setTimeout(dismiss, life);
  function dismiss() {
    clearTimeout(timer);
    if (!el.parentNode) return;
    el.classList.add('is-leaving');
    setTimeout(() => el.remove(), 220);
  }
  el.addEventListener('click', dismiss);
  root.appendChild(el);
  return dismiss;
}

// ---------------- 确认 / 输入弹窗（替代原生 confirm/prompt） ----------------
let modalOpen = false;
function openModal({ title, message, input, confirmText = '确定', cancelText = '取消', danger = false }) {
  // 同一时刻只允许一个弹窗：旧的按「取消」关闭（输入框用 null，确认框用 false）
  if (modalOpen) closeModal(input ? null : false);
  return new Promise((resolve) => {
    const overlay = document.createElement('div');
    overlay.className = 'modal-overlay';
    const box = document.createElement('div');
    box.className = 'modal';
    box.setAttribute('role', 'dialog');
    box.setAttribute('aria-modal', 'true');
    const h = document.createElement('div');
    h.className = 'modal-title';
    h.textContent = title || '请确认';
    box.appendChild(h);
    if (message) {
      const p = document.createElement('div');
      p.className = 'modal-msg';
      p.textContent = message;
      box.appendChild(p);
    }
    let inputEl = null;
    if (input) {
      inputEl = document.createElement('input');
      inputEl.className = 'modal-input';
      inputEl.type = 'text';
      inputEl.value = input.value || '';
      inputEl.placeholder = input.placeholder || '';
      box.appendChild(inputEl);
    }
    const acts = document.createElement('div');
    acts.className = 'modal-actions';
    const cancel = document.createElement('button');
    cancel.type = 'button';
    cancel.textContent = cancelText;
    const ok = document.createElement('button');
    ok.type = 'button';
    ok.className = 'solid' + (danger ? ' modal-danger' : '');
    ok.textContent = confirmText;
    acts.appendChild(cancel);
    acts.appendChild(ok);
    box.appendChild(acts);
    overlay.appendChild(box);
    document.body.appendChild(overlay);
    modalOpen = true;
    const prevFocus = document.activeElement;
    const done = (val) => {
      closeModal(val);
    };
    function closeModal(val) {
      if (!overlay.parentNode) return;
      document.removeEventListener('keydown', onKey, true);
      overlay.remove();
      modalOpen = false;
      if (prevFocus && prevFocus.focus) prevFocus.focus();
      resolve(val);
    }
    // 挂到 window 供「新弹窗顶掉旧弹窗」时调用
    window.__closeModal = closeModal;
    function onKey(e) {
      if (e.key === 'Escape') { e.stopPropagation(); e.preventDefault(); done(input ? null : false); }
      else if (e.key === 'Enter' && !e.shiftKey) {
        // Enter 跟随当前焦点：焦点在「取消」上就取消，不要强行确认
        if (e.target === cancel) {
          e.stopPropagation(); e.preventDefault();
          done(input ? null : false);
          return;
        }
        if (inputEl && e.target !== inputEl && e.target !== ok) return;
        e.stopPropagation(); e.preventDefault();
        done(input ? inputEl.value : true);
      }
    }
    document.addEventListener('keydown', onKey, true);
    overlay.addEventListener('mousedown', (e) => { if (e.target === overlay) done(input ? null : false); });
    cancel.addEventListener('click', () => done(input ? null : false));
    ok.addEventListener('click', () => done(input ? inputEl.value : true));
    requestAnimationFrame(() => {
      overlay.classList.add('is-open');
      (inputEl || ok).focus();
      if (inputEl) inputEl.select();
    });
  });
}
function closeModal(val) { if (window.__closeModal) window.__closeModal(val); }
const confirmDialog = (message, opts = {}) => openModal({ title: opts.title || '请确认', message, danger: !!opts.danger, confirmText: opts.confirmText || (opts.danger ? '删除' : '确定') });
const promptDialog = (opts) => openModal({ ...opts, input: { value: opts.value || '', placeholder: opts.placeholder || '' } });

// 异步按钮 loading 态：执行期间禁用并换文案，结束后恢复
async function withBusy(btn, busyText, fn) {
  if (!btn) return fn();
  const old = btn.textContent;
  const wasDisabled = btn.disabled;
  btn.disabled = true;
  btn.classList.add('is-busy');
  if (busyText) btn.textContent = busyText;
  try { return await fn(); }
  finally {
    btn.disabled = wasDisabled;
    btn.classList.remove('is-busy');
    if (busyText) btn.textContent = old;
  }
}

// 当前会话 id（切换会话时更新；null = 跟随后端 active）
let currentSessionId = null;
// 由 sendText 乐观渲染、尚未被 SSE user_text 消费的用户文本队列
// （按发送顺序逐个消费；连发多条不同文本时不会错位/重复渲染用户气泡）
const pendingLocalQueue = [];
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
  // 视图可能正挂在另一套外壳里（LCARS 的 #lcars-slot 或经典的 #content），
  // 必须全局收集再整体搬迁，否则切回经典时 content 里为空 → 整个界面空白。
  const views = Array.from(document.querySelectorAll('.view'));
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

// ---------------- 子 Agent ----------------
let subagentTasks = [];
// 当前会话里的实时子 Agent 卡片（id -> DOM 引用）
const subagentLiveCards = new Map();

function subagentStatusText(status) {
  if (status === 'running') return '运行中';
  if (status === 'done') return '已完成';
  if (status === 'cancelled') return '已取消';
  return '失败';
}

function renderSubagentBadge() {
  const badge = document.getElementById('sa-nav-badge');
  if (!badge) return;
  const running = subagentTasks.filter((t) => t.status === 'running').length;
  badge.hidden = running === 0;
  badge.textContent = String(running);
}

function upsertSubagent(task) {
  const i = subagentTasks.findIndex((t) => t.id === task.id);
  if (i >= 0) subagentTasks[i] = { ...subagentTasks[i], ...task };
  else subagentTasks.unshift(task);
  renderSubagentBadge();
  renderSubagents();
}

function subagentJump(sid) {
  if (!sid) return;
  switchView('chat');
  const navChat = document.querySelector('.nav-item[data-view="chat"], .nav__link[data-view="chat"]');
  if (navChat) {
    document.querySelectorAll('.nav-item, .nav__link').forEach((b) => b.classList.remove('active'));
    navChat.classList.add('active');
  }
  switchSession(sid);
}

function renderSubagents() {
  const list = document.getElementById('subagent-list');
  if (!list) return;
  if (!subagentTasks.length) {
    list.innerHTML = `
      <div class="sa-empty">
        <strong>暂无子 Agent</strong>
        <span>在对话里让 AI 处理复杂、耗时的任务时，它会自动派发子 Agent，并在这里显示进度。</span>
      </div>`;
    return;
  }
  const running = subagentTasks.filter((t) => t.status === 'running').length;
  const ordered = [...subagentTasks].sort((a, b) => {
    if (a.status === 'running' && b.status !== 'running') return -1;
    if (b.status === 'running' && a.status !== 'running') return 1;
    return (b.started_at || 0) - (a.started_at || 0);
  });
  list.innerHTML = '';
  const head = document.createElement('div');
  head.className = 'sa-list__head';
  head.innerHTML = `<span>共 ${subagentTasks.length} 条${running ? ` · 运行中 ${running}` : ''}</span><span>内容过长时自动折叠</span>`;
  list.appendChild(head);
  ordered.forEach((t) => {
    const item = document.createElement('div');
    item.className = 'sa-item';
    const detail = t.status === 'running'
      ? `<div class="sa-item__progress">${esc(t.progress || '正在准备…')}</div>`
      : `<div class="sa-item__summary">${esc(t.summary || '')}</div>`;
    item.innerHTML = `
      <div class="sa-item__top">
        <span class="sa-dot ${esc(t.status)}"></span>
        <div>
          <div class="sa-item__title">${esc(t.title || '未命名任务')}</div>
          <div class="sa-item__meta">
            <span class="sa-chip" title="来源对话">对话：${esc(t.session_title || '未命名对话')}</span>
            <span>${subagentStatusText(t.status)}</span>
          </div>
        </div>
      </div>
      ${detail}
      <div class="sa-item__actions">
        <button type="button" class="sa-act" data-act="jump">查看对话</button>
        <button type="button" class="sa-act" data-act="detail">查看详情</button>
        ${t.status === 'running'
          ? '<button type="button" class="sa-act danger" data-act="cancel">中止任务</button>'
          : ''}
        <button type="button" class="sa-act" data-act="del">删除</button>
      </div>`;
    item.querySelector('[data-act="jump"]').addEventListener('click', () => subagentJump(t.parent_session));
    item.querySelector('[data-act="detail"]').addEventListener('click', () => openSubagentDetail(t.id));
    const cancelBtn = item.querySelector('[data-act="cancel"]');
    if (cancelBtn) cancelBtn.addEventListener('click', async () => {
      cancelBtn.disabled = true;
      cancelBtn.textContent = '正在中止…';
      try {
        await apiPost(`/api/subagents/${encodeURIComponent(t.id)}/cancel`, {});
      } catch (e) {
        toast('中止失败: ' + e.message, 'error');
      }
      await loadSubagents();
    });
    item.querySelector('[data-act="del"]').addEventListener('click', () => deleteSubagentTask(t));
    list.appendChild(item);
    keepSubagentCollapse(item);
  });
}

// 内容超过两行时提供「展开全文 / 收起」，避免单条任务铺满屏幕。
function keepSubagentCollapse(item) {
  const body = item.querySelector('.sa-item__summary, .sa-item__progress');
  if (!body) return;
  requestAnimationFrame(() => {
    if (body.scrollHeight <= body.clientHeight + 2) return;
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'sa-item__toggle';
    btn.textContent = '展开全文';
    btn.addEventListener('click', () => {
      const open = item.classList.toggle('is-open');
      btn.textContent = open ? '收起' : '展开全文';
    });
    item.insertBefore(btn, item.querySelector('.sa-item__actions'));
  });
}

async function deleteSubagentTask(t) {
  const withConv = !!(document.getElementById('sa-del-conv') || {}).checked;
  const tip = withConv
    ? `删除子 Agent「${t.title}」的任务记录，并同时删除主对话中的完成卡片？`
    : `删除子 Agent「${t.title}」的任务记录？（主对话中的卡片会保留）`;
  if (!confirm(tip)) return;
  try {
    await apiDelete(`/api/subagents/${encodeURIComponent(t.id)}?with_conversation=${withConv}`);
    toast('已删除任务记录', 'success');
  } catch (e) {
    toast('删除失败: ' + e.message, 'error');
  }
  await loadSubagents();
}

async function loadSubagents() {
  try {
    const j = await apiGet('/api/subagents');
    subagentTasks = (j && j.tasks) || [];
    renderSubagents();
    renderSubagentBadge();
  } catch (e) {
    /* 后端未就绪时静默，避免启动瞬间弹错 */
  }
}

// 在对话流里插入一张子 Agent 卡片（运行中 / 完成共用）
function addSubagentCard(ev, done) {
  const chat = document.getElementById('chat-scroll');
  if (!chat) return null;
  const wrap = document.createElement('div');
  wrap.className = 'msg assistant';
  wrap.dataset.saId = ev.id;
  const ok = ev.ok !== false && ev.status !== 'cancelled';
  wrap.innerHTML = `
    <div class="sa-card ${done ? '' : 'is-running'}">
      <div class="sa-card__head">
        <span class="sa-dot ${done ? (ok ? 'done' : 'failed') : 'running'}"></span>
        <span class="sa-card__eyebrow">${done ? (ev.status === 'cancelled' ? '子 Agent 已取消' : ok ? '子 Agent 已完成' : '子 Agent 执行失败') : '子 Agent 运行中'}</span>
      </div>
      <div class="sa-card__title">${esc(ev.title || '')}</div>
      <div class="sa-card__summary">${esc(done ? (ev.summary || '') : '正在准备…')}</div>
      <div class="sa-card__foot">
        <button type="button" class="sa-card__link" data-act="jump">查看对话</button>
        <button type="button" class="sa-card__link" data-act="detail">查看详情</button>
      </div>
    </div>`;
  wrap.querySelector('[data-act="jump"]').addEventListener('click', () => {
    subagentJump(ev.session_id || ev.parent_session || currentSessionId);
  });
  wrap.querySelector('[data-act="detail"]').addEventListener('click', () => openSubagentDetail(ev.id));
  chat.appendChild(wrap);
  scrollChat();
  if (done) return null;
  const card = {
    wrap,
    dot: wrap.querySelector('.sa-dot'),
    eyebrow: wrap.querySelector('.sa-card__eyebrow'),
    summary: wrap.querySelector('.sa-card__summary'),
  };
  subagentLiveCards.set(ev.id, card);
  return card;
}

// ---------------- 子 Agent 全屏详情 ----------------
let saDetailId = null;
let saDetailTask = null;

function openSubagentDetail(id) {
  const box = document.getElementById('sa-detail');
  if (!box) return;
  saDetailId = id;
  saDetailTask = null;
  box.hidden = false;
  const scroll = document.getElementById('sa-detail-scroll');
  scroll.innerHTML = '<div class="sa-detail__empty">正在加载执行轨迹…</div>';
  document.getElementById('sa-detail-title').textContent = '子 Agent 详情';
  document.getElementById('sa-detail-sub').textContent = '';
  document.getElementById('sa-detail-cancel').hidden = true;
  apiGet(`/api/subagents/${encodeURIComponent(id)}`)
    .then((j) => {
      if (saDetailId !== id) return;
      saDetailTask = (j && j.task) || null;
      if (!saDetailTask) throw new Error('任务不存在');
      renderSubagentDetail();
    })
    .catch((e) => {
      if (saDetailId !== id) return;
      scroll.innerHTML = `<div class="sa-detail__empty">${esc(e.message || '加载失败')}</div>`;
    });
}

function closeSubagentDetail() {
  const box = document.getElementById('sa-detail');
  if (box) box.hidden = true;
  saDetailId = null;
  saDetailTask = null;
}

function traceEntryDom(e) {
  const div = document.createElement('div');
  if (e.kind === 'user') {
    div.className = 'sa-tr sa-tr-user';
    div.innerHTML = '<div class="sa-tr__role">指令</div>';
    const bubble = document.createElement('div');
    bubble.className = 'sa-tr__bubble';
    bubble.textContent = e.text || '';
    div.appendChild(bubble);
    return div;
  }
  if (e.kind === 'thinking') {
    div.className = 'sa-tr sa-tr-thinking';
    const d = document.createElement('details');
    const s = document.createElement('summary');
    s.textContent = '思考过程';
    const pre = document.createElement('pre');
    pre.textContent = e.text || '';
    d.appendChild(s);
    d.appendChild(pre);
    div.appendChild(d);
    return div;
  }
  if (e.kind === 'tool') {
    div.className = 'sa-tr sa-tr-tool';
    const head = document.createElement('div');
    head.className = 'sa-tr-tool__head';
    head.innerHTML = `<span>${esc(e.name || '工具')}</span><span class="sa-tr__tag ${e.ok ? 'ok' : 'fail'}">${e.ok ? '成功' : '失败'}</span>`;
    div.appendChild(head);
    if (e.input && Object.keys(e.input).length) {
      const pre = document.createElement('pre');
      pre.textContent = '参数：' + JSON.stringify(e.input, null, 2);
      div.appendChild(pre);
    }
    if (e.text) {
      const pre = document.createElement('pre');
      pre.textContent = '结果：' + e.text;
      div.appendChild(pre);
    }
    return div;
  }
  div.className = 'sa-tr sa-tr-assistant';
  div.innerHTML = '<div class="sa-tr__role">子 Agent</div>';
  const bubble = document.createElement('div');
  bubble.className = 'sa-tr__bubble';
  bubble.innerHTML = renderMd(e.text || '');
  div.appendChild(bubble);
  return div;
}

function renderSubagentDetail() {
  const t = saDetailTask;
  if (!t || !saDetailId) return;
  document.getElementById('sa-detail-title').textContent = t.title || '未命名任务';
  const time = t.started_at ? new Date(t.started_at).toLocaleString() : '';
  document.getElementById('sa-detail-sub').textContent =
    `来源对话：${t.session_title || '未命名对话'} · ${subagentStatusText(t.status)}${time ? ' · ' + time : ''}`;
  const cancelBtn = document.getElementById('sa-detail-cancel');
  cancelBtn.hidden = t.status !== 'running';
  const scroll = document.getElementById('sa-detail-scroll');
  scroll.innerHTML = '';
  const trace = Array.isArray(t.trace) ? t.trace : [];
  if (!trace.length) {
    scroll.innerHTML = '<div class="sa-detail__empty">暂时还没有执行轨迹。</div>';
  } else {
    trace.forEach((e) => scroll.appendChild(traceEntryDom(e)));
  }
  if (t.status === 'running') {
    const row = document.createElement('div');
    row.className = 'sa-detail__running';
    row.id = 'sa-detail-live';
    row.innerHTML = '<span class="sa-dot running"></span><span>' + esc(t.progress || '正在执行…') + '</span>';
    scroll.appendChild(row);
  }
  scroll.scrollTop = scroll.scrollHeight;
}

function appendDetailTrace(ev) {
  if (!saDetailId || ev.id !== saDetailId) return;
  // 发送指令时本地已乐观插入一条「用户指令」，后端回传同一内容时去重
  const prev = saDetailTask && saDetailTask.trace && saDetailTask.trace[saDetailTask.trace.length - 1];
  if (ev.entry && ev.entry.kind === 'user' && prev && prev.kind === 'user' && prev.text === ev.entry.text) {
    return;
  }
  const scroll = document.getElementById('sa-detail-scroll');
  const empty = scroll.querySelector('.sa-detail__empty');
  if (empty) empty.remove();
  const live = document.getElementById('sa-detail-live');
  const node = traceEntryDom(ev.entry || {});
  if (live) scroll.insertBefore(node, live);
  else scroll.appendChild(node);
  const t = saDetailTask;
  if (t) t.trace = (t.trace || []).concat([ev.entry]);
  scroll.scrollTop = scroll.scrollHeight;
}

function markSubagentCardDone(id, ev) {
  const ok = ev.ok !== false && ev.status !== 'cancelled';
  document.querySelectorAll(`[data-sa-id="${id}"]`).forEach((wrap) => {
    const dot = wrap.querySelector('.sa-dot');
    const eyebrow = wrap.querySelector('.sa-card__eyebrow');
    const summary = wrap.querySelector('.sa-card__summary');
    if (dot) dot.className = 'sa-dot ' + (ok ? 'done' : 'failed');
    if (eyebrow) eyebrow.textContent = ev.status === 'cancelled' ? '子 Agent 已取消' : (ok ? '子 Agent 已完成' : '子 Agent 执行失败');
    if (summary) summary.textContent = ev.summary || '';
    const card = wrap.querySelector('.sa-card');
    if (card) card.classList.remove('is-running');
  });
}


// 对话流事件仅渲染当前会话；session_id 为空 = 全局事件（语音），不拦截
function evForCurrentSession(ev) {
  if (!ev.session_id) return true;
  if (!currentSessionId) return true;
  return ev.session_id === currentSessionId;
}

// 外部输入（语音唤醒等）落到了其他会话：界面跟随该会话，
// 清空当前视图后直接接收后续流式事件，保证"说的和看到的"在同一个会话里。
function adoptSession(sid) {
  if (!sid || sid === currentSessionId) return;
  currentSessionId = sid;
  turnSessionId = sid;
  currentTurn = null;
  pendingLocalQueue.length = 0;
  resetReasoning();
  hideProcessing();
  const chat = $('#chat-scroll');
  if (chat) chat.innerHTML = '';
  syncVoiceSession();
  // 只刷新侧栏高亮，不重新加载历史，避免打断正在流式输出的内容
  apiGet('/api/sessions')
    .then((j) => {
      if (j && Array.isArray(j.sessions)) renderSessionList(j.sessions, sid);
    })
    .catch(() => {});
}

// ---------------- 导航 ----------------
// 视图切换：经典侧栏(.nav-item) 与 LCARS 导航(.nav__link) 共用
function switchView(view, activeEl) {
  document.querySelectorAll('.nav-item, .nav__link').forEach((b) => b.classList.remove('active'));
  if (activeEl) activeEl.classList.add('active');
  document.querySelectorAll('.view').forEach((v) => v.classList.remove('active'));
  const target = document.getElementById('view-' + view);
  if (target) target.classList.add('active');
  if (view === 'subagents') loadSubagents();
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
    { view: 'subagents', label: '子 AGENT', num: '04' },
    { view: 'schedules', label: '定时任务', num: '05' },
    { view: 'settings', label: '设置', num: '06' },
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
let interruptSession = null;  // 请求打断时任务所属会话（只冻结该会话的在途事件）
let interruptTimer = null;    // 打断确认超时兜底：后端迟迟不回 idle 时复位按钮
let lastTurnUserText = ''; // 上一轮用户指令（失败节点"重试本轮"用）

function clearInterruptWait() {
  interruptPending = false;
  interruptSession = null;
  if (interruptTimer) { clearTimeout(interruptTimer); interruptTimer = null; }
}
// 思考过程只要有就展示（不再单独提供开关，避免"没开开关"导致看不到思考）
const showReasoning = true;
// 每个会话一份实时 turn 快照（per-session）：切走再切回时用它重建
// "AI 工作中"的思考卡/工具卡/已出文本（尚未持久化，历史里没有）；
// done 后保留最终文本，作为"切回时历史尚未刷新"的兜底补齐来源。
const turnSnapshots = new Map(); // sessionId -> { text, tools: Map(call_id->{id,name,inputs,ok,summary}), reasoning: [], done }

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
// 工具快照按 call_id 精确建卡：同名工具并发/连续调用不再互相覆盖
function cacheTurnToolUse(sessionId, id, name, input) {
  const tools = snapFor(sessionId).tools;
  const key = id || name;
  if (id || !tools.has(key)) {
    tools.set(key, { id: id || '', name, inputs: [input], ok: null, summary: '', dur: '', startTs: Date.now() });
  } else {
    const e = tools.get(key);
    e.inputs.push(input);
    e.startTs = Date.now();
  }
}
function cacheTurnToolResult(sessionId, id, name, ok, summary) {
  const tools = snapFor(sessionId).tools;
  let e = id ? tools.get(id) : null;
  if (!e) {
    // 无 id（旧后端兼容）：按名字找最近一个未完成的同名调用
    for (const v of [...tools.values()].reverse()) {
      if (v.name === name && v.ok === null) { e = v; break; }
    }
  }
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
  // SVGElement 没有 hidden 这个 IDL 属性，直接赋 JS 属性只会挂个 expando，
  // 不会真正显示/隐藏图标，必须操作 attribute。
  if (sendIco) sendIco.toggleAttribute('hidden', sendBusy);
  if (stopIco) stopIco.toggleAttribute('hidden', !sendBusy);
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
    const banner = $('#conn-banner');
    banner.hidden = true;
    banner.textContent = '后端未连接 · 正在重连…';
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
      clearInterruptWait();
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
      syncVoiceSession();
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
    // 等消息渲染完成再返回：调用方（switchSession 等）不会出现「历史稍后才覆盖 UI」的竞态
    await loadMessages(currentSessionId);
    syncVoiceSession();
  } catch {
    sessionList.innerHTML = '<div class="session-empty">会话服务不可用</div>';
  }
}

function renderSessionList(list, activeId) {
  if (!list.length) {
    sessionList.innerHTML = '<div class="session-empty">暂无对话，点击右上角新建</div>';
    updateCollapsedTitle();
    return;
  }
  // FLIP：记录重建前每条会话的位置，新建/删除后让旧条目平滑地"被挤下/收回"
  const prevTops = new Map();
  sessionList.querySelectorAll('.session-item[data-sid]').forEach((el) => {
    prevTops.set(el.dataset.sid, el.getBoundingClientRect().top);
  });
  const hadItems = prevTops.size > 0;
  sessionList.innerHTML = '';
  list.forEach((s) => {
    const item = document.createElement('div');
    item.className = 'session-item' + (s.id === activeId ? ' active' : '');
    item.dataset.sid = s.id;
    item.innerHTML = `
      <span class="s-ico"></span>
      <span class="s-name" title="${esc(s.title)}">${esc(s.title)}</span>
      <button class="s-menu-btn" title="会话菜单" aria-label="会话菜单">⋮</button>
      <div class="session-pop" hidden>
        <button data-act="switch">切换到此对话</button>
        <button data-act="rename">重命名</button>
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
      if (willOpen) {
        // 靠近列表底部时改为向上弹出，避免被滚动容器裁剪
        const listRect = sessionList.getBoundingClientRect();
        const itemRect = item.getBoundingClientRect();
        pop.classList.toggle('is-up', itemRect.bottom + 128 > listRect.bottom);
      }
      pop.hidden = !willOpen;
    });
    pop.addEventListener('click', (e) => {
      e.stopPropagation();
      const act = e.target.dataset.act;
      pop.hidden = true;
      if (act === 'switch') switchSession(s.id);
      else if (act === 'rename') renameSession(s);
      else if (act === 'del') deleteSession(s.id);
    });
    sessionList.appendChild(item);
  });
  // 先同步把旧条目放回原位，再在下一帧滑到新位置，形成"新会话把旧会话往下挤"的动画
  if (hadItems) {
    sessionList.querySelectorAll('.session-item[data-sid]').forEach((el) => {
      const oldTop = prevTops.get(el.dataset.sid);
      if (oldTop === undefined) {
        el.classList.add('is-entering');
        return;
      }
      const dy = oldTop - el.getBoundingClientRect().top;
      if (Math.abs(dy) < 1) return;
      el.style.transition = 'none';
      el.style.transform = `translateY(${dy}px)`;
      requestAnimationFrame(() => {
        el.style.transition = 'transform .24s cubic-bezier(.22,1,.36,1)';
        el.style.transform = '';
        el.addEventListener('transitionend', () => {
          el.style.transition = '';
          el.style.transform = '';
        }, { once: true });
      });
    });
  }
  updateCollapsedTitle();
}

function closeAllPops() {
  sessionList.querySelectorAll('.session-pop').forEach((p) => { p.hidden = true; });
}
document.addEventListener('click', closeAllPops);

async function deleteSession(id) {
  const yes = await confirmDialog('删除该对话？历史消息将一并移除，无法恢复。', { title: '删除对话', danger: true });
  if (!yes) return;
  try {
    const j = await apiDelete(`/api/sessions/${encodeURIComponent(id)}`);
    if (j && j.ok === false) throw new Error(j.error || '后端未确认删除');
    turnSnapshots.delete(id);
    if (currentSessionId === id) currentSessionId = null;
    toast('对话已删除', 'success');
    await loadSessions();
  } catch (e) { toast('删除失败: ' + e.message, 'error'); }
}

async function renameSession(s) {
  const title = await promptDialog({
    title: '重命名对话',
    value: s.title,
    placeholder: '输入新的对话名称',
    confirmText: '保存',
  });
  if (title === null) return;
  const t = title.trim();
  if (!t) { toast('名称不能为空', 'warn'); return; }
  try {
    const j = await apiPost(`/api/sessions/${encodeURIComponent(s.id)}/rename`, { title: t });
    if (!j || !j.ok) throw new Error('后端未确认（会话可能已被删除）');
    toast('已重命名', 'success');
    // 只刷新侧栏，不重建聊天区（避免打断正在流式输出的内容）
    const list = await apiGet('/api/sessions');
    renderSessionList(list.sessions || [], currentSessionId);
  } catch (e) { toast('重命名失败: ' + e.message, 'error'); }
}

// 语音对话目标会话：任何会话变化（切换/新建/删除/启动恢复）都同步给后端，
// 保证唤醒后说的话落在"界面上正在看的那个会话"里。
function syncVoiceSession() {
  if (!currentSessionId) return;
  apiPost('/api/voice/session', { session_id: currentSessionId }).catch(() => {});
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
    pendingLocalQueue.length = 0;
    // loadSessions 会按后端 active 加载一次消息，这里不再重复拉取
    await loadSessions(); // 刷新 active 高亮 + 加载消息（以后端 active 为准）
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
    pendingLocalQueue.length = 0;
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
      const el = createMsg(m.text, m.id);
      el.classList.add('user');
      chat.appendChild(el);
    } else if (m.role === 'assistant') {
      const div = document.createElement('div');
      div.className = 'msg assistant';
      const records = m.tools || [];
      // 历史同样走时间线：工具汇总行 → 回答节点 → 交付物节点
      const t = buildAssistantTurn();
      t.interrupted = !!m.interrupted;
      if (records.length) {
        t.activity.hidden = false;
        const recs = records.map((r) => ({
          ...r,
          title: toolTitleFromInputs(r.name, r.input || {}),
        }));
        showToolSummary(t, m.elapsed_ms, recs);
        t.tl.classList.add('has-line');
      } else {
        t.tl.classList.add('is-plain');
      }
      if (m.interrupted) {
        // 被打断的一轮：最终回答位置只显示「被打断」，耗时显示在上方
        renderInterruptedTurn(t, m.elapsed_ms, records.length > 0);
      } else if (m.text) {
        t.answerNode.hidden = false;
        t.bubble.innerHTML = renderMd(m.text);
        tlSetState(t.answerNode, 'completed');
      }
      const dv = deliverFromRecords(records);
      if (dv) {
        const card = buildDeliverCard(dv);
        if (card) { t.reportNode.hidden = false; t.deliverBox.appendChild(card); }
      }
      t.block.statusRow.hidden = true;
      div.appendChild(t.tl);
      chat.appendChild(div);
    } else if (m.role === 'subagent') {
      // 子 Agent 完成卡片（后台通道写入，历史回放与实时卡片同款）
      const meta = m.subagent || {};
      const ok = meta.ok !== false && meta.status !== 'cancelled';
      const div = document.createElement('div');
      div.className = 'msg assistant';
      div.dataset.saId = meta.id || '';
      div.innerHTML = `
        <div class="sa-card">
          <div class="sa-card__head">
            <span class="sa-dot ${ok ? 'done' : 'failed'}"></span>
            <span class="sa-card__eyebrow">子 Agent ${meta.status === 'cancelled' ? '已取消' : ok ? '已完成' : '执行失败'}</span>
          </div>
          <div class="sa-card__title">${esc(meta.title || '')}</div>
          <div class="sa-card__summary">${esc(meta.summary || m.text || '')}</div>
          <div class="sa-card__foot">
            <button type="button" class="sa-card__link" data-act="jump">查看对话</button>
            <button type="button" class="sa-card__link" data-act="detail">查看详情</button>
          </div>
        </div>`;
      div.querySelector('[data-act="jump"]').addEventListener('click', () => {
        subagentJump(meta.session_id || currentSessionId);
      });
      div.querySelector('[data-act="detail"]').addEventListener('click', () => openSubagentDetail(meta.id));
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
        if (snap.tools.size) {
          t.activity.hidden = false;
          const recs = [...snap.tools.values()].map((e) => ({
            name: e.name,
            input: e.inputs && e.inputs.length ? e.inputs[e.inputs.length - 1] : {},
            title: toolTitleFromInputs(e.name, e.inputs && e.inputs.length ? e.inputs[e.inputs.length - 1] : {}),
            ok: e.ok,
            summary: e.summary,
          }));
          showToolSummary(t, snap.elapsedMs, recs);
          t.tl.classList.add('has-line');
        } else {
          t.tl.classList.add('is-plain');
        }
        t.answerNode.hidden = false;
        t.bubble.innerHTML = renderMd(snap.text);
        tlSetState(t.answerNode, 'completed');
        div.appendChild(t.tl);
        chat.appendChild(div);
      }
    }
  }
  chat.scrollTop = chat.scrollHeight;
  chatPinned = true;
  chatLastTop = chat.scrollTop;
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

// 思考行：与工具调用同一版式（白底直排一行），运行中右侧实时走秒，内容默认折叠。
// 完成后收起为"已完成思考 · 用时 X 秒"，让用户知道 AI 在干活而不是卡死。
function makeThoughtNode() {
  const { node, body } = tlNodeEl('thought', 'running');
  node.hidden = true;
  const head = document.createElement('button');
  head.type = 'button';
  head.className = 'tl-head tl-thought-head';
  head.setAttribute('aria-expanded', 'false');
  const chev = document.createElement('span');
  chev.innerHTML = TL_IC.chev;
  const title = document.createElement('span');
  title.className = 'tl-title';
  title.textContent = '正在思考';
  const timeEl = document.createElement('span');
  timeEl.className = 'tl-thought-time';
  head.appendChild(chev);
  head.appendChild(title);
  head.appendChild(timeEl);
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
  let paused = false;
  let accumMs = 0;   // 已结束的思考时段累计
  let runSince = 0;  // 当前思考时段起点（0 = 未在计时）
  let ticker = null;

  const elapsed = () => accumMs + (runSince ? Date.now() - runSince : 0);
  const fmtSecs = (ms) => (ms < 60000 ? (Math.max(0, ms) / 1000).toFixed(1) + ' 秒' : fmtDuration(ms));
  const tick = () => {
    // 节点已随会话切换/重建被移除：停表，避免定时器泄漏
    if (!node.isConnected) { stopTicker(); return; }
    if (runSince) timeEl.textContent = fmtSecs(elapsed());
  };
  const stopTicker = () => { if (ticker) { clearInterval(ticker); ticker = null; } };
  const setOpen = (v) => {
    open = v;
    node.classList.toggle('is-open', open);
    head.setAttribute('aria-expanded', String(open));
    if (open) requestAnimationFrame(() => { textEl.scrollTop = textEl.scrollHeight; });
  };
  const start = () => {
    node.hidden = false;
    stopTicker();
    finished = false;
    paused = false;
    tlSetState(node, 'running');
    title.textContent = '正在思考';
    runSince = Date.now();
    ticker = setInterval(tick, 200);
    tick();
  };

  // 思考过程中也允许点开查看
  head.addEventListener('click', () => setOpen(!open));

  return {
    node, body,
    get steps() { return steps; },
    start,
    // 历史/切换会话恢复：展示已有思考内容，不重新计时
    restore(text) {
      node.hidden = false;
      if (text) { steps = 1; textEl.textContent = text; }
      else textEl.textContent = '（本次回复未返回思考内容，模型可能未开启思考模式）';
      finished = true;
      title.textContent = '已完成思考';
      timeEl.textContent = '';
      tlSetState(node, 'completed');
      node.classList.remove('is-peek');
      setOpen(false);
    },
    add(chunk) {
      if (!chunk) return;
      steps += 1;
      if (finished || !runSince) start();
    },
    flush(text) {
      if (!text) return;
      textEl.textContent = text;
      // 有思考内容时：折叠区自动弹出一小条，内容在其中持续向上滚动
      node.classList.add('is-peek');
      requestAnimationFrame(() => { textEl.scrollTop = textEl.scrollHeight; });
    },
    // 打断：立刻停表并折叠，保持「已暂停」状态不被后续收尾覆盖
    pause() {
      if (node.hidden || paused) return;
      if (runSince) { accumMs += Date.now() - runSince; runSince = 0; }
      stopTicker();
      finished = true;
      paused = true;
      node.classList.remove('is-peek');
      setOpen(false);
      title.textContent = '已暂停';
      timeEl.textContent = accumMs ? '用时 ' + fmtSecs(accumMs) : '';
      tlSetState(node, 'failed');
    },
    finish() {
      if (node.hidden || paused) return;
      if (runSince) { accumMs += Date.now() - runSince; runSince = 0; }
      stopTicker();
      finished = true;
      title.textContent = '已完成思考';
      timeEl.textContent = accumMs ? '用时 ' + fmtSecs(accumMs) : '';
      tlSetState(node, 'completed');
      if (!textEl.textContent) textEl.textContent = '（本次回复未返回思考内容，模型可能未开启思考模式）';
      // 思考完成：小条自动收回，只剩一行摘要
      node.classList.remove('is-peek');
      setOpen(false);
    },
    stop() { stopTicker(); },
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

  // 工具活动区：固定一行高，工具切换时旧行上滑收起、新行滑入，同一时刻只显示一个工具
  const activity = document.createElement('div');
  activity.className = 'tl-activity';
  activity.hidden = true;
  tl.appendChild(activity);

  const ans = tlNodeEl('answer', 'running');
  ans.node.hidden = true;
  // 任务耗时：完成后显示在最终回答文字上方
  const elapsedEl = document.createElement('div');
  elapsedEl.className = 'tl-elapsed';
  elapsedEl.hidden = true;
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
  ans.body.appendChild(elapsedEl);
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

  const block = { stack: tl, statusRow: foot, thought, elapsedEl };
  return {
    tl, block, thought, activity,
    answerNode: ans.node, answerBody: ans.body, bubble, answerActions: actions, elapsedEl,
    reportNode: report.node, deliverBox,
  };
}

// 点击暂停：立刻冻结当前这一轮的 UI（思考停表折叠、工具行停转并标记已中断），
// 不等后端 assistant_done 到达，避免出现「已经停了但工具还在转圈」。
function pauseTurnUI() {
  const turn = currentTurn;
  if (!turn) return;
  const line = turn.activityLine;
  if (line && line.row) {
    line.row.classList.remove('is-entering');
    line.row.classList.add('is-paused');
    if (line.stateEl) line.stateEl.textContent = '已中断';
    if (line.record && line.record.ok === null) {
      line.record.ok = false;
      line.record.summary = '已中断';
      line.record.endTs = Date.now();
    }
  }
  if (turn.thought) turn.thought.pause();
  turn.paused = true;
}

// 被打断的一轮：回答位置只显示「被打断」，上方显示本轮耗时
function renderInterruptedTurn(t, elapsedMs, hideElapsed) {
  t.interrupted = true;
  t.answerNode.hidden = false;
  if (!hideElapsed) {
    t.elapsedEl.hidden = false;
    t.elapsedEl.textContent = elapsedMs > 0 ? `任务耗时 ${fmtDuration(elapsedMs)}` : '任务已打断';
  }
  t.bubble.innerHTML = '<span class="tl-interrupted">被打断</span>';
  tlSetState(t.answerNode, 'failed');
}

// 用缓存重建进行中的助手消息块（切会话回来时调用）
function renderLiveTurnBlock(cache) {
  const chat = $('#chat-scroll');
  const div = document.createElement('div');
  div.className = 'msg assistant';
  const t = buildAssistantTurn();
  div.appendChild(t.tl);
  const reasoningText = cache.reasoning.join('');
  if (!cache.done) {
    // 切回正在运行的会话：思考卡继续实时走秒
    t.thought.start();
    if (showReasoning && reasoningText) t.thought.flush(reasoningText);
  } else if (reasoningText) {
    t.thought.restore(showReasoning ? reasoningText : null);
  }
  // 工具：重建为一行活动行（运行中显示最后一个工具；完成后由 finalizeTurn 收拢成汇总行）
  let lastLabel = '';
  let lastRunning = null;
  const toolCount = cache.tools.size;
  cache.tools.forEach((e) => {
    lastLabel = toolTitleFromInputs(e.name, e.inputs);
    if (e.ok === null) {
      lastRunning = { name: e.name, label: lastLabel };
    }
  });
  let liveLine = null;
  if (toolCount) {
    t.activity.hidden = false;
    const line = makeActivityRow(lastLabel || '工具调用');
    line.name = lastRunning ? lastRunning.name : '';
    if (lastRunning) {
      liveLine = line;
      t.activity.appendChild(line.row);
    } else {
      line.row.classList.remove('is-entering');
      line.row.classList.add('is-done');
      line.stateEl.textContent = '已完成';
      t.activity.appendChild(line.row);
    }
  }
  if (reasoningText || toolCount) t.tl.classList.add('has-line');
  if (cache.text) {
    t.answerNode.hidden = false;
    t.bubble.innerHTML = renderMd(cache.text);
  }
  chat.appendChild(div);
  currentTurn = {
    msg: div, text: t.bubble, tl: t.tl, block: t.block, detail: t.tl,
    deliverBox: t.deliverBox, deliver: deliverFromRecords([...cache.tools.values()]),
    startTs: Date.now(), answerNode: t.answerNode,
    thought: t.thought, activity: t.activity, activityLine: liveLine,
    toolCount, elapsedEl: t.elapsedEl, reportNode: t.reportNode,
  };
  if (cache.done) {
    finalizeTurn(currentTurn, cache.elapsedMs);
  } else {
    if (toolCount > 0) t.thought.finish();
    setBlockRunning(t.block, lastLabel || '处理');
  }
}

// ---------------- 对话（用户/助手气泡 + 工具调用卡片） ----------------
const chatScroll = $('#chat-scroll');

// 当前助手消息容器：{ msg, text, tools, toolMap }
let currentTurn = null;

// 自动跟随底部：仅当用户本来就停在底部时才跟随流式输出。
// 用户手动往上滚动后取消吸附，避免每个 token 都把视图拽回底部、导致"拖不动"。
let chatPinned = true;
let chatLastTop = 0;
const CHAT_PIN_SLACK = 48;
function chatAtBottom() {
  return chatScroll.scrollHeight - chatScroll.scrollTop - chatScroll.clientHeight <= CHAT_PIN_SLACK;
}
function scrollChat(force) {
  if (!force && !chatPinned) return;
  chatScroll.scrollTop = chatScroll.scrollHeight;
  chatPinned = true;
  chatLastTop = chatScroll.scrollTop;
}
chatScroll.addEventListener('scroll', () => {
  const top = chatScroll.scrollTop;
  // 向上拖动（滚动条/触控板/键盘皆可）→ 取消吸附；回到底部 → 恢复吸附
  if (top < chatLastTop - 2 && !chatAtBottom()) chatPinned = false;
  if (chatAtBottom()) chatPinned = true;
  chatLastTop = top;
}, { passive: true });

function addUserBubble(text) {
  const div = document.createElement('div');
  div.className = 'msg user';
  const inner = document.createElement('div');
  inner.className = 'bubble';
  inner.textContent = text;
  div.appendChild(inner);
  attachUserMenu(div, '');
  chatScroll.appendChild(div);
  scrollChat(true);
}

function addSystem(text) {
  const div = document.createElement('div');
  div.className = 'msg system';
  div.textContent = text;
  chatScroll.appendChild(div);
  scrollChat();
}

// ---------------- AI 运行中：思考内容直接显示在助手消息顶部的"正在思考"行内 ----------------
// （旧版底部 dock 已移除：思考小条在时间线思考节点内弹出/滚动/收起）

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
    deliverBox: t.deliverBox,
    deliver: null,
    startTs: Date.now(),
    answerNode: t.answerNode,
    thought: t.thought,
    activity: t.activity,
    activityLine: null,
    toolCount: 0,
    elapsedEl: t.elapsedEl,
    reportNode: t.reportNode,
  };
  scrollChat();
  return currentTurn;
}

// 新一轮开始：立即渲染时间线并启动思考卡（含实时用时），替代纯文字"正在思考"占位
function beginTurnUI() {
  hideProcessing();
  const turn = ensureTurn();
  if (turn && turn.thought) turn.thought.start();
  scrollChat();
  return turn;
}

// 请求被后端拒绝/发送失败：撤掉刚建好的空助手块，避免留下一直走秒的思考卡
function abortTurnUI() {
  hideProcessing();
  if (currentTurn) {
    if (currentTurn.thought) currentTurn.thought.stop();
    if (currentTurn.msg) currentTurn.msg.remove();
    currentTurn = null;
  }
}

function finishTurn(elapsedMs) {
  if (currentTurn) finalizeTurn(currentTurn, elapsedMs);
  currentTurn = null;
  resetReasoning();
}

// 本轮收尾：状态行变"已完成，用时 X 分 X 秒"，明细保持收起，渲染交付卡片。
// 无论实时路径还是历史重建路径都走这里，保证"切走再回来"这行不会消失。
function finalizeTurn(turn, elapsedMs) {
  if (!turn || !turn.block) return;
  const thinking = !!(turn.thought && !turn.thought.node.hidden);
  const ms = (elapsedMs && elapsedMs > 0)
    ? elapsedMs
    : (turn.startTs ? Date.now() - turn.startTs : 0);
  const count = turn.toolCount || 0;
  const hasActivity = count > 0 || thinking;
  if (turn.thought) turn.thought.finish();
  // 任务完成：所有工具调用收拢成一行（含总耗时）
  showToolSummary(turn, ms);
  if (!hasActivity) turn.tl.classList.add('is-plain');
  finalizeBlock(turn.block, ms, hasActivity && !count);
  if (turn.answerNode && !turn.answerNode.hidden) {
    tlSetState(turn.answerNode, (turn.interrupted || turn.failed) ? 'failed' : 'completed');
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
  'ConnectMcp': '连接 MCP 服务',
  'ImportSkill': '导入 Skill',
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

// 结束：最终回答文字上方显示"任务耗时 X"；底部旧摘要行不再使用
function finalizeBlock(block, elapsedMs, hasActivity) {
  if (!block) return;
  if (block.statusRow) block.statusRow.hidden = true;
  if (block.elapsedEl && hasActivity && elapsedMs > 0) {
    block.elapsedEl.hidden = false;
    block.elapsedEl.textContent = '任务耗时 ' + fmtDuration(elapsedMs);
  }
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

// 工具活动行：整个工具阶段共用一个固定高度的小窗，同一时刻只出现一行。
// 切换工具时旧行向上滑出、新行从下方滑入（滚动折叠式切换），不会一路拉长。
function makeActivityRow(title) {
  const row = document.createElement('div');
  row.className = 'tl-act is-entering';
  const ic = document.createElement('span');
  ic.className = 'tl-act-ic';
  ic.innerHTML = TL_IC.spin;
  const nameEl = document.createElement('span');
  nameEl.className = 'tl-act-name';
  nameEl.textContent = title;
  const stateEl = document.createElement('span');
  stateEl.className = 'tl-act-state';
  stateEl.textContent = '执行中';
  row.appendChild(ic);
  row.appendChild(nameEl);
  row.appendChild(stateEl);
  requestAnimationFrame(() => row.classList.remove('is-entering'));
  return { row, nameEl, stateEl, title };
}

// 实时工具调用：始终只占一行，新工具替换当前行
function addToolCard(agent, name, input, id) {
  const isSpeak = name === 'SpeakToUser';
  const turn = ensureTurn();
  if (!turn.activity) return;
  if (name === 'DeliverFiles') turn.deliver = input;
  const title = isSpeak ? '语音播报' : toolTitleFromInputs(name, input);
  const old = turn.activityLine;
  if (old && old.row) {
    old.row.classList.add('is-leaving');
    setTimeout(() => { if (old.row.parentNode) old.row.remove(); }, 280);
  }
  const line = makeActivityRow(title);
  line.name = name;
  line.callId = id || '';
  line.startTs = Date.now();
  // 累积本轮全部工具记录，结束后汇总行可展开逐条查看
  turn.toolRecords = turn.toolRecords || [];
  line.record = { id: id || '', name, title, input, startTs: line.startTs, ok: null, summary: '', line };
  turn.toolRecords.push(line.record);
  turn.toolById = turn.toolById || new Map();
  if (id) turn.toolById.set(id, line.record);
  turn.activity.hidden = false;
  turn.activity.appendChild(line.row);
  turn.activityLine = line;
  turn.toolCount = (turn.toolCount || 0) + 1;
  turn.tl.classList.remove('is-plain');
  turn.tl.classList.add('has-line');
  if (turn.thought) turn.thought.finish();
  setBlockRunning(turn.block, title);
  scrollChat();
}

// 工具结果按 call_id 精确配对：即使活动行已被下一个工具替换，
// 旧工具的记录也会被正确标记（汇总列表里不再出现"永远执行中"的条目）。
function setToolResult(agent, name, ok, summary, id) {
  const turn = currentTurn;
  if (!turn) return;
  if (!turn.toolCount) addToolCard(agent, name, {}, id);
  let rec = id && turn.toolById ? turn.toolById.get(id) : null;
  if (!rec && turn.toolRecords) {
    // 无 id 兼容：优先当前活动行，其次最近一个未完成的同名记录
    const cur = turn.activityLine;
    if (cur && cur.record && (!name || cur.name === name) && cur.record.ok === null) rec = cur.record;
    else {
      for (let i = turn.toolRecords.length - 1; i >= 0; i -= 1) {
        const r = turn.toolRecords[i];
        if (r.name === name && r.ok === null) { rec = r; break; }
      }
    }
  }
  if (!rec) return;
  rec.ok = ok;
  rec.summary = summary || '';
  rec.endTs = Date.now();
  const line = rec.line;
  // 只有结果属于当前仍在展示的活动行时才改行状态（旧行已滑出，只更新记录）
  if (line && turn.activityLine === line && line.row && line.row.isConnected) {
    line.stateEl.textContent = ok ? '已完成' : (summary === '已中断' ? '已中断' : '失败');
    line.row.classList.toggle('is-failed', !ok);
    line.row.classList.toggle('is-done', !!ok);
  }
  scrollChat();
}

// 把本轮工具记录渲染成可逐条展开的明细列表
function buildToolRecordList(records) {
  const list = document.createElement('div');
  list.className = 'tl-tool-list';
  list.hidden = true;
  records.forEach((rec, idx) => {
    const item = document.createElement('div');
    item.className = 'tl-tool-item';
    const head = document.createElement('button');
    head.type = 'button';
    head.className = 'tl-tool-head';
    const dot = document.createElement('span');
    dot.className = 'tl-tool-dot' + (rec.ok === false ? ' is-failed' : rec.ok === true ? ' is-done' : '');
    const nameEl = document.createElement('span');
    nameEl.className = 'tl-tool-name';
    nameEl.textContent = rec.title || rec.name || ('工具 ' + (idx + 1));
    const stateEl = document.createElement('span');
    stateEl.className = 'tl-tool-state';
    stateEl.textContent = rec.ok === false ? '失败' : rec.ok === true ? '已完成' : '已中断';
    const chev = document.createElement('span');
    chev.className = 'tl-tool-chev';
    chev.innerHTML = TL_IC.chev;
    head.appendChild(dot);
    head.appendChild(nameEl);
    head.appendChild(stateEl);
    head.appendChild(chev);
    const detail = document.createElement('div');
    detail.className = 'tl-tool-detail';
    detail.hidden = true;
    if (rec.input && Object.keys(rec.input).length) {
      const label = document.createElement('div');
      label.className = 'tl-tool-label';
      label.textContent = '调用参数';
      const pre = document.createElement('pre');
      pre.className = 'tl-tool-pre';
      pre.textContent = JSON.stringify(rec.input, null, 2);
      detail.appendChild(label);
      detail.appendChild(pre);
    }
    if (rec.summary) {
      const label = document.createElement('div');
      label.className = 'tl-tool-label';
      label.textContent = '返回结果';
      const pre = document.createElement('pre');
      pre.className = 'tl-tool-pre';
      pre.textContent = rec.summary;
      detail.appendChild(label);
      detail.appendChild(pre);
    }
    if (!detail.firstChild) {
      const empty = document.createElement('div');
      empty.className = 'tl-tool-label';
      empty.textContent = '没有更多详情';
      detail.appendChild(empty);
    }
    head.addEventListener('click', (e) => {
      e.stopPropagation();
      detail.hidden = !detail.hidden;
      chev.classList.toggle('is-open', !detail.hidden);
    });
    item.appendChild(head);
    item.appendChild(detail);
    list.appendChild(item);
  });
  return list;
}

// 一轮结束后：所有工具收拢成一行（带箭头），点开可逐条查看、每条再展开细节
function showToolSummary(turn, elapsedMs, records) {
  if (!turn || !turn.activity) return;
  const recs = records || turn.toolRecords || [];
  const count = recs.length || turn.toolCount || 0;
  if (!count) return;
  turn.activity.hidden = false;
  turn.activity.innerHTML = '';
  turn.activity.classList.add('is-summary');
  const row = document.createElement('button');
  row.type = 'button';
  row.className = 'tl-act is-final tl-summary-toggle' + (turn.interrupted ? ' is-failed' : '');
  const ic = document.createElement('span');
  ic.className = 'tl-act-ic';
  ic.innerHTML = turn.interrupted ? TL_IC.fail : TL_IC.check;
  const nameEl = document.createElement('span');
  nameEl.className = 'tl-act-name';
  nameEl.textContent = turn.interrupted
    ? (count > 1 ? ('已中断 · ' + count + ' 个工具调用') : '已中断 · 1 个工具调用')
    : (count > 1 ? ('已完成 ' + count + ' 个工具调用') : '已完成 1 个工具调用');
  const stateEl = document.createElement('span');
  stateEl.className = 'tl-act-state';
  stateEl.textContent = elapsedMs ? ('任务耗时 ' + fmtDuration(elapsedMs)) : '';
  const chev = document.createElement('span');
  chev.className = 'tl-summary-chev';
  chev.innerHTML = TL_IC.chev;
  row.appendChild(ic);
  row.appendChild(nameEl);
  row.appendChild(stateEl);
  row.appendChild(chev);
  turn.activity.appendChild(row);
  let list = null;
  if (recs.length) {
    list = buildToolRecordList(recs);
    turn.activity.appendChild(list);
    row.addEventListener('click', () => {
      list.hidden = !list.hidden;
      chev.classList.toggle('is-open', !list.hidden);
    });
  }
  turn.activityLine = null;
  // 汇总行已带耗时，避免与回答上方的耗时重复
  if (turn.elapsedEl) turn.elapsedEl.hidden = true;
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

// 关闭所有用户消息的"更多"菜单
function closeUserMenus(except) {
  document.querySelectorAll('.msg.user .user-menu-pop').forEach((p) => {
    if (p !== except) p.hidden = true;
  });
  document.querySelectorAll('.msg.user .user-menu-btn').forEach((b) => {
    if (!except || !b.parentElement.contains(except)) b.setAttribute('aria-expanded', 'false');
  });
}
document.addEventListener('click', () => closeUserMenus());

// 删除一整轮问答：用户消息 + 其后的助手回复一起移除，并从后端历史剔除
async function deleteUserTurn(el) {
  const sid = currentSessionId;
  if (!sid) return;
  const bubble = el.querySelector('.bubble');
  const text = bubble ? bubble.textContent : '';
  let mid = el.dataset.mid;
  // 实时发送的这条还没写回 id：按文本匹配后端最后一条同内容 user 消息
  if (!mid) {
    try {
      const j = await apiGet(`/api/sessions/${sid}/messages`);
      const msgs = (j && j.messages) || [];
      for (let i = msgs.length - 1; i >= 0; i -= 1) {
        if (msgs[i].role === 'user' && msgs[i].text === text) { mid = msgs[i].id; break; }
      }
    } catch { /* 忽略，下面给出失败提示 */ }
  }
  if (!mid) { toast('无法定位这条消息，请刷新会话后重试', 'error'); return; }
  try {
    await apiDelete(`/api/sessions/${sid}/messages/${mid}`);
  } catch (e) {
    toast('删除失败: ' + e.message, 'error');
    return;
  }
  // 移除 DOM：本条用户气泡 + 紧跟其后的助手消息（直到下一条用户消息）
  let next = el.nextElementSibling;
  el.remove();
  while (next && next.classList && next.classList.contains('assistant')) {
    const n = next.nextElementSibling;
    next.remove();
    next = n;
  }
  if (currentTurn && !document.body.contains(currentTurn.msg)) {
    currentTurn = null;
    setSendBtn(false);
  }
}

// 把后端消息 id 同步到界面上的用户气泡（实时发送的乐观气泡一开始没有 id）
let syncingUserIds = false;
async function syncUserMsgIds(sid) {
  if (!sid || sid !== currentSessionId || syncingUserIds) return;
  syncingUserIds = true;
  try {
    const j = await apiGet(`/api/sessions/${sid}/messages`);
    if (sid !== currentSessionId) return;
    const ids = ((j && j.messages) || []).filter((m) => m.role === 'user' && m.id).map((m) => m.id);
    const els = Array.from(document.querySelectorAll('#chat-scroll .msg.user'));
    // 从后往前一一对应，避免文本重复时匹配错位
    for (let i = 0; i < Math.min(ids.length, els.length); i += 1) {
      const el = els[els.length - 1 - i];
      const id = ids[ids.length - 1 - i];
      if (!el.dataset.mid) el.dataset.mid = id;
    }
  } catch { /* 忽略：下次 done 再同步 */ }
  finally { syncingUserIds = false; }
}

// 给用户消息挂右下角三点菜单（删除整轮问答）
function attachUserMenu(el, mid) {
  if (mid) el.dataset.mid = mid;
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'user-menu-btn';
  btn.setAttribute('aria-expanded', 'false');
  btn.title = '更多';
  btn.textContent = '⋯';
  const pop = document.createElement('div');
  pop.className = 'user-menu-pop';
  pop.hidden = true;
  const del = document.createElement('button');
  del.type = 'button';
  del.textContent = '删除本轮问答';
  pop.appendChild(del);
  el.appendChild(btn);
  el.appendChild(pop);
  btn.addEventListener('click', (e) => {
    e.stopPropagation();
    const willOpen = pop.hidden;
    closeUserMenus(pop);
    pop.hidden = !willOpen;
    btn.setAttribute('aria-expanded', String(willOpen));
  });
  del.addEventListener('click', (e) => {
    e.stopPropagation();
    pop.hidden = true;
    deleteUserTurn(el);
  });
  return el;
}

// 会话历史重建时创建一个静态消息气泡（返回后由调用方追加 class）
function createMsg(text, mid) {
  const div = document.createElement('div');
  div.className = 'msg';
  const inner = document.createElement('div');
  inner.className = 'bubble';
  inner.textContent = text;
  div.appendChild(inner);
  return attachUserMenu(div, mid);
}

function sendText(text) {
  text = (text || '').trim();
  if (!text) return;
  finishTurn();
  // 乐观渲染：立即显示用户气泡 + “正在处理…”占位，不等待 SSE 事件
  // （避免 SSE 迟到/丢失时界面“没反应”）；SSE user_text 到达时按文本去重。
  addUserBubble(text);
  lastTurnUserText = text;
  beginTurnUI();
  pendingLocalQueue.push(text);
  turnSessionId = currentSessionId;
  cacheTurnReset(currentSessionId);
  setSendBtn(true);
  apiPost('/api/chat', { text, session_id: currentSessionId })
    .then((j) => {
      if (j && j.ok === false) {
        abortTurnUI();
        const qi = pendingLocalQueue.lastIndexOf(text);
        if (qi >= 0) pendingLocalQueue.splice(qi, 1);
        addSystem(j.error || '发送失败');
        // 后端拒绝：本轮未真正占用任务，立即复位按钮（轮询会按 main_status 兜底纠正）
        setSendBtn(false);
        turnSessionId = null;
      }
    })
    .catch((e) => {
      abortTurnUI();
      const qi = pendingLocalQueue.lastIndexOf(text);
      if (qi >= 0) pendingLocalQueue.splice(qi, 1);
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
  interruptSession = turnSessionId || currentSessionId;
  setSendBtn(true);
  pauseTurnUI(); // UI 立即冻结，不等后端回执
  // 兜底：后端 10s 内未确认停止（收尾卡住/事件丢失）则复位按钮并拉取权威状态
  if (interruptTimer) clearTimeout(interruptTimer);
  interruptTimer = setTimeout(() => {
    interruptTimer = null;
    if (!interruptPending) return;
    clearInterruptWait();
    pollStatus();
  }, 10000);
  apiPost('/api/chat/interrupt', {})
    .catch(() => {
      // 请求本身失败：复位为发送态（轮询会按后端真实状态纠正）
      clearInterruptWait();
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
let busyEnterHintAt = 0;
$('#chat-input').addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault();
    if (sendBusy) {
      // 忙碌中 Enter 不重复发送，但给出明确反馈（而不是"按了没反应"）
      const now = Date.now();
      if (now - busyEnterHintAt > 3000) {
        busyEnterHintAt = now;
        toast('AI 正在回复，点右侧暂停按钮可打断', 'info');
      }
      return;
    }
    const v = $('#chat-input').value;
    $('#chat-input').value = '';
    setSendBtn(false);
    sendText(v);
  }
});
$('#chat-input').addEventListener('input', () => setSendBtn(sendBusy));

// Esc = 打断当前会话正在进行的回复（弹窗打开时由弹窗自己处理 Esc）
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape' || modalOpen) return;
  if (sendBusy && turnSessionId === currentSessionId) interruptAI();
});

// ---------------- SSE 事件 ----------------
// 事件序号去重：后端重放（重连/Lagged 补齐）可能与实时流重叠，按 id 丢弃已处理事件
let lastEventId = 0;
let esHadError = false;
function openEvents() {
  const es = new EventSource(CORE_URL + '/api/events');
  es.onmessage = (m) => {
    const id = Number(m.lastEventId || 0);
    if (id && id <= lastEventId) return;
    if (id) lastEventId = id;
    let ev;
    try { ev = JSON.parse(m.data); } catch { return; }
    handleEvent(ev);
  };
  es.onerror = () => {
    // 后端不可达：顶部横幅提示（EventSource 会自动重连）
    esHadError = true;
    connected = false;
    $('#conn-banner').hidden = false;
  };
  es.onopen = () => {
    $('#conn-banner').hidden = true;
    if (esHadError) {
      // 断线重连：后端可能已重启（序号归零），放弃本地去重水位，
      // 由后端按 Last-Event-ID 重放补齐；这里只刷新权威状态与侧栏，
      // 不整体重建聊天区（避免打断正在流式输出的内容）。
      esHadError = false;
      lastEventId = 0;
      pollStatus();
      apiGet('/api/sessions')
        .then((j) => renderSessionList(j.sessions || [], currentSessionId))
        .catch(() => {});
    }
  };
}

function handleEvent(ev) {
  // 已点暂停：丢弃该会话仍在途的流式事件，UI 保持冻结直到 assistant_done 收尾。
  // tool_result 放行：后端打断时会给未完成工具补发「已中断」结果，正好落快照。
  if (interruptPending && (!ev.session_id || ev.session_id === interruptSession) && (
    ev.type === 'assistant_text' || ev.type === 'tool_use' ||
    ev.type === 'reasoning_text'
  )) return;
  // 语音唤醒/定时任务等外部输入可能落在别的会话：界面立即切过去，
  // 否则会出现"AI 有语音反馈但界面完全没有消息、会话里也找不到"的情况。
  if (ev.type === 'user_text' && ev.session_id && currentSessionId && ev.session_id !== currentSessionId) {
    adoptSession(ev.session_id);
  }
  switch (ev.type) {
    case 'user_text':
      // 打字路径：与 sendText 乐观渲染按发送顺序一一对应（队列消费），
      // 连发多条不同文本时不会错位、也不会重复渲染用户气泡。
      // 注意：打字路径的时间线已由 sendText 建好，这里不能 finishTurn，否则思考卡会被提前收尾。
      if (pendingLocalQueue.length && pendingLocalQueue[0] === ev.text) {
        pendingLocalQueue.shift();
        cacheTurnReset(ev.session_id || currentSessionId);
        break;
      }
      finishTurn(); // 新轮开始：先清理上一轮 DOM 状态（无论是否当前会话）
      cacheTurnReset(ev.session_id || currentSessionId);
      if (!evForCurrentSession(ev)) break;
      // 语音路径（非打字来源）双发防护：同一文本 5 秒内重复到达视为重放，丢弃
      const now = Date.now();
      const last = lastUserTextAt.get(ev.text);
      if (last && now - last < 5000) break;
      lastUserTextAt.set(ev.text, now);
      // 语音等外部输入同样进入忙碌态（按钮变暂停可打断 AI）
      turnSessionId = ev.session_id || currentSessionId;
      lastTurnUserText = ev.text; // 语音轮失败后同样可「重试本轮」
      setSendBtn(true);
      addUserBubble(ev.text);
      beginTurnUI();
      break;
    case 'assistant_text': {
      cacheTurnText(ev.session_id || currentSessionId, ev.text);
      if (!evForCurrentSession(ev)) break;
      hideProcessing();
      // 流式增量：后端现在发的是 token 片段，需要追加到缓冲区再渲染
      const textTurn = ensureTurn();
      // 本轮已渲染错误卡片（turn_failed）：失败补发的 "[错误]…" 文本不再覆盖
      if (textTurn.failed) break;
      textTurn.answerNode.hidden = false;
      if (textTurn._streamBuf == null) textTurn._streamBuf = '';
      textTurn._streamBuf += ev.text;
      textTurn.text.innerHTML = renderMd(textTurn._streamBuf);
      tlSetState(textTurn.answerNode, 'running');
      // 开始输出最终回答 = 思考阶段结束，思考块自动收起为一行摘要，小窗同时收起
      if (textTurn.thought) textTurn.thought.finish();
      // 纯文字回复保持旧版白底直排观感，不显示时间线圆点
      if (!textTurn.toolCount && (!textTurn.thought || textTurn.thought.node.hidden)) {
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
      clearInterruptWait();
      cacheTurnDone(ev.session_id || currentSessionId, ev.elapsed_ms); // 标记快照完成 + 留存本轮耗时
      if (!evForCurrentSession(ev)) break;
      hideProcessing();
      if (currentTurn) {
        // 工具调用后由 tool_use 创建的 currentTurn 回答节点为空，回填最终文本；
        // 若已由 assistant_text 覆盖过则保持不动
        if (ev.interrupted) {
          // 被打断：已流式出的半截文字不作为最终回答，只显示「被打断」+ 耗时
          renderInterruptedTurn(currentTurn, ev.elapsed_ms);
        } else if (!currentTurn.text.textContent && ev.text) {
          currentTurn.text.innerHTML = renderMd(ev.text);
          currentTurn.answerNode.hidden = false;
        }
        finishTurn(ev.elapsed_ms);
      } else if (ev.interrupted || ev.text) {
        // 无工具调用的纯文本回复（后端只发 assistant_done）：直接渲染最终文本
        const div = document.createElement('div');
        div.className = 'msg assistant';
        const t = buildAssistantTurn();
        t.tl.classList.add('is-plain');
        if (ev.interrupted) {
          renderInterruptedTurn(t, ev.elapsed_ms);
        } else {
          t.answerNode.hidden = false;
          t.bubble.innerHTML = renderMd(ev.text);
          tlSetState(t.answerNode, 'completed');
        }
        div.appendChild(t.tl);
        chatScroll.appendChild(div);
      }
      scrollChat();
      // 本轮结束后把最新一条用户气泡接上消息 id（供三点菜单精确删除）
      syncUserMsgIds(ev.session_id || currentSessionId);
      break;
    case 'tool_use':
      cacheTurnToolUse(ev.session_id || currentSessionId, ev.id, ev.name, ev.input);
      if (!evForCurrentSession(ev)) break;
      addToolCard(ev.agent || 'main', ev.name, ev.input, ev.id);
      break;
    case 'tool_result':
      cacheTurnToolResult(ev.session_id || currentSessionId, ev.id, ev.name, ev.ok, ev.summary);
      if (!evForCurrentSession(ev)) break;
      setToolResult(ev.agent || 'main', ev.name, ev.ok, ev.summary, ev.id);
      break;
    case 'reasoning_text':
      // 思考内容进入时间线内的思考节点（默认收起，展开才显示全文）
      cacheTurnReasoning(ev.session_id || currentSessionId, ev.text);
      if (!evForCurrentSession(ev)) break;
      hideProcessing();
      if (showReasoning) addReasoningCard(ev.text);
      break;
    case 'voice':
      // 语音状态不再单独弹条展示：右下角语音按钮的颜色已表达监听/播报状态
      if (ev.kind === 'speak_error') toast(ev.text || '语音播报失败', 'error');
      else if (ev.kind === 'continuous') pollStatus(); // 同步多轮按钮点亮状态
      break;
    case 'turn_failed': {
      toast('本轮请求失败：' + ev.message, 'error', 6000);
      cacheTurnText(ev.session_id || currentSessionId, '[错误] ' + ev.message);
      if (!evForCurrentSession(ev)) break;
      hideProcessing();
      const ft = ensureTurn();
      ft.failed = true;
      if (ft.thought) ft.thought.finish();
      ft.answerNode.hidden = false;
      ft.text.innerHTML = '';
      const box = document.createElement('div');
      box.className = 'tl-turn-error';
      const msg = document.createElement('div');
      msg.className = 'tl-turn-error__msg';
      msg.textContent = '请求失败：' + ev.message;
      const retry = document.createElement('button');
      retry.type = 'button';
      retry.className = 'tl-action';
      retry.textContent = '重试本轮';
      retry.addEventListener('click', () => retryLastTurn());
      box.appendChild(msg);
      box.appendChild(retry);
      ft.text.appendChild(box);
      tlSetState(ft.answerNode, 'failed');
      scrollChat();
      break;
    }
    case 'report_text': {
      // 定时任务汇报：独立渲染为一条助手消息，不占用/打断当前 turn
      if (!evForCurrentSession(ev)) break;
      const div = document.createElement('div');
      div.className = 'msg assistant';
      const t = buildAssistantTurn();
      t.tl.classList.add('is-plain');
      t.answerNode.hidden = false;
      t.bubble.innerHTML = renderMd(ev.text);
      tlSetState(t.answerNode, 'completed');
      div.appendChild(t.tl);
      chatScroll.appendChild(div);
      scrollChat();
      break;
    }
    case 'resync':
      // 断流缺口过大、环形缓冲补不齐：整体拉一次权威状态 + 当前会话历史
      pollStatus();
      if (currentSessionId) loadMessages(currentSessionId);
      apiGet('/api/sessions')
        .then((j) => renderSessionList(j.sessions || [], currentSessionId))
        .catch(() => {});
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
          clearInterruptWait();
        }
      }
      break;
    case 'schedule_triggered': addSystem(`⏰ 定时任务触发: ${ev.title}`); break;
    case 'subagent_started':
      upsertSubagent({
        id: ev.id,
        parent_session: ev.session_id,
        session_title: ev.session_title,
        title: ev.title,
        prompt: ev.prompt,
        status: 'running',
        progress: '正在准备…',
        started_at: Date.now(),
      });
      if (evForCurrentSession(ev)) addSubagentCard(ev, false);
      break;
    case 'subagent_progress': {
      if (!subagentTasks.some((t) => t.id === ev.id)) { loadSubagents(); break; }
      upsertSubagent({ id: ev.id, status: 'running', progress: ev.progress });
      const live = subagentLiveCards.get(ev.id);
      if (live) live.summary.textContent = ev.progress || '正在准备…';
      if (saDetailId === ev.id && saDetailTask) {
        saDetailTask.progress = ev.progress;
        const row = document.getElementById('sa-detail-live');
        if (row) {
          const txt = row.querySelector('span:last-child');
          if (txt) txt.textContent = ev.progress || '正在执行…';
        }
      }
      break;
    }
    case 'subagent_trace':
      appendDetailTrace(ev);
      break;
    case 'subagent_deleted':
      subagentTasks = subagentTasks.filter((t) => t.id !== ev.id);
      renderSubagents();
      renderSubagentBadge();
      if (ev.with_conversation) {
        document.querySelectorAll(`[data-sa-id="${ev.id}"]`).forEach((el) => el.remove());
      }
      if (saDetailId === ev.id) closeSubagentDetail();
      break;
    case 'subagent_done': {
      upsertSubagent({
        id: ev.id,
        parent_session: ev.session_id,
        session_title: ev.session_title,
        title: ev.title,
        summary: ev.summary,
        ok: ev.ok,
        status: ev.status || 'done',
      });
      const live = subagentLiveCards.get(ev.id);
      if (live) {
        live.dot.className = 'sa-dot ' + (ev.ok === false ? 'failed' : 'done');
        live.eyebrow.textContent = ev.status === 'cancelled'
          ? '子 Agent 已取消'
          : (ev.ok === false ? '子 Agent 执行失败' : '子 Agent 已完成');
        live.summary.textContent = ev.summary || '';
        subagentLiveCards.delete(ev.id);
        scrollChat();
      } else if (evForCurrentSession(ev)) {
        // 兜底：本会话的完成卡片还没出现（例如页面刷新后中途接管），直接补一张
        if (!document.querySelector(`#chat-scroll [data-sa-id="${ev.id}"]`)) {
          addSubagentCard(ev, true);
        } else {
          markSubagentCardDone(ev.id, ev);
        }
      } else {
        toast('子 Agent 已完成：' + (ev.title || ''), 'info');
      }
      if (saDetailId === ev.id && saDetailTask) {
        saDetailTask.status = ev.status || 'done';
        saDetailTask.summary = ev.summary || '';
        renderSubagentDetail();
      }
      break;
    }
    case 'memory_updated': loadMemory(); break;
    case 'skills_updated': loadSkills(); break;
    case 'mcp_updated': loadMCP(); break;
    case 'schedules_updated': loadSchedules(); break;
    case 'settings_updated': flashSaved(); pollStatus(); break;
    default: break;
  }
}

$('#btn-new-session').addEventListener('click', () => newSession());

// ---------------- 侧边栏折叠 ----------------
const SIDEBAR_COLLAPSED_KEY = 'st.sidebar.collapsed';
function updateCollapsedTitle() {
  const el = $('#collapsed-chat-title');
  if (!el) return;
  const active = document.querySelector('.session-item.active .s-name');
  const text = active && active.textContent.trim();
  el.textContent = text || '新对话';
  el.title = el.textContent;
}
function setSidebarCollapsed(collapsed) {
  document.body.classList.toggle('sidebar-collapsed', collapsed);
  try { localStorage.setItem(SIDEBAR_COLLAPSED_KEY, collapsed ? '1' : '0'); } catch {}
  if (collapsed) updateCollapsedTitle();
}
$('#btn-sidebar-collapse').addEventListener('click', () => setSidebarCollapsed(true));
$('#btn-sidebar-expand').addEventListener('click', () => setSidebarCollapsed(false));
$('#btn-new-session-top').addEventListener('click', () => newSession());
try { setSidebarCollapsed(localStorage.getItem(SIDEBAR_COLLAPSED_KEY) === '1'); } catch {}

// ---------------- 侧栏宽度拖拽 ----------------
const SIDEBAR_W_KEY = 'st.sidebar.width';
const appClassic = $('#app-classic');
const sidebarResizer = $('#sidebar-resizer');
function applySidebarWidth(w) {
  appClassic.style.setProperty('--sidebar-w', `${w}px`);
}
try {
  const savedW = parseInt(localStorage.getItem(SIDEBAR_W_KEY) || '', 10);
  if (savedW >= 160 && savedW <= 520) applySidebarWidth(savedW);
} catch {}
if (sidebarResizer) {
  let dragging = false;
  sidebarResizer.addEventListener('mousedown', (e) => {
    if (document.body.classList.contains('sidebar-collapsed')) return;
    dragging = true;
    e.preventDefault();
    document.body.classList.add('is-resizing');
    appClassic.classList.add('resizing');
  });
  window.addEventListener('mousemove', (e) => {
    if (!dragging) return;
    applySidebarWidth(Math.min(520, Math.max(160, e.clientX)));
  });
  window.addEventListener('mouseup', () => {
    if (!dragging) return;
    dragging = false;
    document.body.classList.remove('is-resizing');
    appClassic.classList.remove('resizing');
    const w = parseInt(getComputedStyle(appClassic).getPropertyValue('--sidebar-w'), 10) || 248;
    try { localStorage.setItem(SIDEBAR_W_KEY, String(w)); } catch {}
  });
}

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
  // 编辑保护：用户正在改某条记忆时不重建列表（避免输入到一半被 SSE 刷新冲掉）
  if (document.querySelector('#memory-list .mem-edit-title')) return;
  try {
    const list = await apiGet('/api/memory');
    $('#memory-count').textContent = list.length;
    const box = $('#memory-list');
    box.innerHTML = '';
    if (!list.length) {
      box.innerHTML = '<div class="list-empty">暂无记忆条目。在左侧新增，或让 AI 在对话中自动沉淀。</div>';
      return;
    }
    list.forEach((m) => {
      const item = document.createElement('div');
      item.className = 'list-item';
      const view = () => {
        item.innerHTML = `
          <div class="row">
            <span class="tag">${esc(m.memory_type)}</span>
            <span class="meta">${esc(m.created_at)}</span>
          </div>
          <div class="title">${esc(m.title)}</div>
          <div class="body">${esc(m.content)}</div>
          <div class="row"><div></div><div class="acts">
            <button data-edit>编辑</button>
            <button data-del>删除</button>
          </div></div>`;
        item.querySelector('[data-edit]').addEventListener('click', () => edit());
        item.querySelector('[data-del]').addEventListener('click', async () => {
          const yes = await confirmDialog(`删除记忆「${m.title}」？`, { title: '删除记忆', danger: true });
          if (!yes) return;
          try {
            const j = await apiDelete('/api/memory/' + encodeURIComponent(m.id));
            if (j && j.ok === false) throw new Error(j.error || '后端未确认');
            toast('记忆已删除', 'success');
          } catch (e) { toast('删除失败: ' + e.message, 'error'); }
        });
      };
      const edit = () => {
        item.innerHTML = `
          <div class="row"><span class="tag">${esc(m.memory_type)}</span></div>
          <input class="mem-edit-title" placeholder="标题" value="${esc(m.title)}" />
          <textarea class="mem-edit-content" rows="4" placeholder="内容">${esc(m.content)}</textarea>
          <div class="row"><div></div><div class="acts">
            <button data-cancel>取消</button>
            <button data-save class="solid">保存</button>
          </div></div>`;
        const titleEl = item.querySelector('.mem-edit-title');
        const contentEl = item.querySelector('.mem-edit-content');
        item.querySelector('[data-cancel]').addEventListener('click', view);
        item.querySelector('[data-save]').addEventListener('click', async (ev) => {
          const t = titleEl.value.trim();
          const c = contentEl.value.trim();
          if (!t || !c) { toast('标题和内容都不能为空', 'warn'); return; }
          await withBusy(ev.currentTarget, '保存中…', async () => {
            try {
              const j = await apiPut('/api/memory/' + encodeURIComponent(m.id), { title: t, content: c });
              if (j && j.ok === false) throw new Error(j.error || '后端未确认');
              m.title = t; m.content = c;
              view();
              toast('记忆已更新', 'success');
            } catch (e) { toast('保存失败: ' + e.message, 'error'); }
          });
        });
      };
      view();
      box.appendChild(item);
    });
  } catch (e) { $('#memory-count').textContent = '错误'; }
}

$('#memory-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const body = {
    type: $('#memory-type').value,
    title: $('#memory-title').value.trim(),
    content: $('#memory-content').value.trim(),
  };
  if (!body.title || !body.content) { toast('标题和内容都不能为空', 'warn'); return; }
  await withBusy(e.submitter || $('#memory-form button[type="submit"]'), '保存中…', async () => {
    try {
      const j = await apiPost('/api/memory', body);
      if (j && j.ok === false) throw new Error(j.error || '后端未确认');
      $('#memory-title').value = ''; $('#memory-content').value = '';
      toast('记忆已保存', 'success');
    } catch (err) { toast('保存失败: ' + err.message, 'error'); }
  });
});

$('#btn-memory-clear').addEventListener('click', async () => {
  const yes = await confirmDialog('确定清空全部记忆？此操作无法恢复。', { title: '清空记忆', danger: true, confirmText: '清空' });
  if (!yes) return;
  try {
    const j = await apiPost('/api/memory/clear');
    if (j && j.ok === false) throw new Error(j.error || '后端未确认');
    toast('记忆已清空', 'success');
  } catch (e) { toast('清空失败: ' + e.message, 'error'); }
});

// ---------------- Skills ----------------
async function loadSkills() {
  try {
    const list = await apiGet('/api/skills');
    $('#skill-count').textContent = list.length;
    const box = $('#skill-list');
    box.innerHTML = '';
    if (!list.length) {
      box.innerHTML = '<div class="list-empty">暂无 Skill。在右侧选择文件夹导入，或让 AI 用 ImportSkill 工具自行导入。</div>';
      return;
    }
    list.forEach((s) => {
      const item = document.createElement('div');
      item.className = 'list-item';
      // 开关状态放 data-enabled：按钮文案会被 i18n 翻译，不能拿文案当状态判断
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
            <button data-toggle="${esc(s.name)}" data-enabled="${s.enabled ? '1' : '0'}">${s.enabled ? '停用' : '启用'}</button>
            <button data-remove="${esc(s.name)}">移除</button>
          </div>
        </div>`;
      box.appendChild(item);
    });
    box.querySelectorAll('[data-toggle]').forEach((b) => b.addEventListener('click', async () => {
      const enabled = b.dataset.enabled === '1';
      b.disabled = true;
      try {
        const j = await apiPost('/api/skills/' + encodeURIComponent(b.dataset.toggle) + '/toggle', { enabled: !enabled });
        if (j && j.ok === false) throw new Error(j.error || '后端未确认');
        loadSkills();
      } catch (e) {
        toast('操作失败: ' + e.message, 'error');
        b.disabled = false;
      }
    }));
    box.querySelectorAll('[data-remove]').forEach((b) => b.addEventListener('click', async () => {
      const yes = await confirmDialog(`移除 skill「${b.dataset.remove}」？（不删除磁盘文件）`, { title: '移除 Skill', danger: true, confirmText: '移除' });
      if (!yes) return;
      try {
        const j = await apiDelete('/api/skills/' + encodeURIComponent(b.dataset.remove));
        if (j && j.ok === false) throw new Error(j.error || '后端未确认');
        toast('已移除', 'success');
      } catch (e) { toast('移除失败: ' + e.message, 'error'); }
    }));
  } catch (e) { /* ignore */ }
}

// 原生「选择文件夹」：Electron 下打开系统对话框；浏览器调试时可手动输入路径兜底
$('#btn-pick-skill').addEventListener('click', async () => {
  if (window.star && window.star.pickSkillDir) {
    try {
      const p = await window.star.pickSkillDir();
      if (p) $('#skill-path').value = p;
    } catch (e) { toast('打开文件夹选择器失败: ' + e.message, 'error'); }
    return;
  }
  $('#skill-path').removeAttribute('readonly');
  $('#skill-path').placeholder = '请输入 skill 文件夹绝对路径';
  $('#skill-path').focus();
});

$('#skill-import-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const p = $('#skill-path').value.trim();
  if (!p) { toast('请先选择一个 skill 文件夹', 'warn'); return; }
  await withBusy(e.submitter || $('#skill-import-form button[type="submit"]'), '导入中…', async () => {
    try {
      await apiPost('/api/skills/import', { path: p });
      $('#skill-path').value = '';
      toast('Skill 已导入', 'success');
    } catch (err) { toast('导入失败: ' + err.message, 'error'); }
  });
});

// ---------------- 连接器：Skills / MCP ----------------
function setConnectorTab(tab) {
  document.querySelectorAll('.seg-tab').forEach((b) => b.classList.toggle('active', b.dataset.connectorTab === tab));
  document.querySelectorAll('[data-connector-panel]').forEach((p) => {
    p.hidden = p.dataset.connectorPanel !== tab;
  });
  if (tab === 'mcp') loadMCP();
}
document.querySelectorAll('.seg-tab').forEach((b) => {
  b.addEventListener('click', () => setConnectorTab(b.dataset.connectorTab));
});

function syncMcpTransport() {
  const isStdio = $('#mcp-transport').value === 'stdio';
  $('#mcp-stdio-row').classList.toggle('hidden', !isStdio);
  $('#mcp-url-row').classList.toggle('hidden', isStdio);
}
$('#mcp-transport').addEventListener('change', syncMcpTransport);
syncMcpTransport();

// 把带引号的参数安全拆开
function splitMcpArgs(text) {
  const out = [];
  const re = /"([^"]*)"|'([^']*)'|(\S+)/g;
  let m;
  while ((m = re.exec(text)) !== null) out.push(m[1] ?? m[2] ?? m[3]);
  return out;
}

function renderMcpServers(list) {
  const box = $('#mcp-list');
  const connected = list.filter((s) => s.status && s.status.connected).length;
  $('#mcp-count').textContent = `${connected} / ${list.length}`;
  if (!list.length) {
    box.innerHTML = '<div class="mcp-empty">暂无 MCP 服务，右侧添加后会自动连接。</div>';
    return;
  }
  box.innerHTML = '';
  list.forEach((s) => {
    const st = s.status || {};
    const online = !!st.connected;
    const item = document.createElement('div');
    item.className = 'list-item mcp-item' + (online ? ' connected' : (st.error ? ' error' : ''));
    const target = s.transport === 'stdio' ? [s.command, ...(s.args || [])].join(' ') : (s.url || '');
    const tools = (st.tools || []).map((t) => `<span class="mcp-tool" title="${esc(t.description || '')}">${esc(t.name)}</span>`).join('');
    item.innerHTML = `
      <div class="row">
        <span class="title">${esc(s.name)}</span>
        <span class="mcp-status"><i class="mcp-dot"></i>${online ? '已连接' : '未连接'}</span>
      </div>
      <div class="mcp-meta">${esc(s.transport)} · ${esc(target)}</div>
      ${st.error ? `<div class="mcp-meta" style="color:var(--danger)">${esc(st.error)}</div>` : ''}
      ${tools ? `<div class="mcp-tools">${tools}</div>` : ''}
      <div class="row">
        <span class="meta">${(st.tools || []).length} 个工具</span>
        <div class="mcp-actions">
          <button data-test="${esc(s.name)}">重新连接</button>
          <button class="danger" data-remove="${esc(s.name)}">移除</button>
        </div>
      </div>`;
    box.appendChild(item);
  });
  box.querySelectorAll('[data-test]').forEach((b) => b.addEventListener('click', async () => {
    b.disabled = true;
    b.textContent = '连接中…';
    try { await apiPost('/api/mcp/servers/' + encodeURIComponent(b.dataset.test) + '/test'); } catch (e) { /* 忽略，状态里会显示 */ }
    loadMCP();
  }));
  box.querySelectorAll('[data-remove]').forEach((b) => b.addEventListener('click', async () => {
    const yes = await confirmDialog(`移除 MCP 服务「${b.dataset.remove}」？`, { title: '移除 MCP 服务', danger: true, confirmText: '移除' });
    if (!yes) return;
    try {
      const j = await apiDelete('/api/mcp/servers/' + encodeURIComponent(b.dataset.remove));
      if (j && j.ok === false) throw new Error(j.error || '后端未确认');
      toast('已移除', 'success');
    } catch (e) { toast('移除失败: ' + e.message, 'error'); }
    loadMCP();
  }));
}

async function loadMCP() {
  const box = $('#mcp-list');
  box.innerHTML = '<div class="mcp-empty">正在连接…</div>';
  try {
    const data = await apiGet('/api/mcp/servers');
    renderMcpServers(data.servers || []);
  } catch (e) {
    box.innerHTML = `<div class="mcp-empty">加载失败：${esc(e.message)}</div>`;
  }
}

$('#mcp-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const hint = $('#mcp-form-hint');
  const name = $('#mcp-name').value.trim();
  const transport = $('#mcp-transport').value;
  const isStdio = transport === 'stdio';
  const cfg = {
    name,
    transport,
    command: isStdio ? $('#mcp-command').value.trim() : '',
    args: isStdio ? splitMcpArgs($('#mcp-args').value.trim()) : [],
    url: isStdio ? '' : $('#mcp-url').value.trim(),
    enabled: true,
  };
  if (!name) { hint.textContent = '请填写名称'; return; }
  hint.textContent = '正在连接…';
  await withBusy(e.submitter || $('#mcp-form button[type="submit"]'), '连接中…', async () => {
    try {
      const res = await apiPost('/api/mcp/servers', cfg);
      if (!res.ok) {
        hint.textContent = '添加失败：' + (res.error || '未知错误');
        return;
      }
      const st = res.status || {};
      hint.textContent = st.connected
        ? `已连接，发现 ${(st.tools || []).length} 个工具（对话中可直接调用）`
        : `配置已保存，但连接失败：${st.error || '未知错误'}`;
      if (st.connected) toast(`MCP「${name}」已连接`, 'success');
      loadMCP();
    } catch (err) {
      hint.textContent = '添加失败：' + err.message;
    }
  });
});

// ---------------- 定时任务 ----------------
// 用「频率 + 时间」代替手写 cron；自定义表达式只放在「高级」里。
function syncSchedRow() {
  const f = $('#sched-freq').value;
  $('#sched-cron').classList.toggle('hidden', f !== 'custom');
  $('#sched-time-row').classList.toggle('hidden', f === 'custom');
  $('#sched-time').classList.toggle('hidden', f === 'hourly');
  $('#sched-weekday').classList.toggle('hidden', f !== 'weekly');
  $('#sched-day').classList.toggle('hidden', f !== 'monthly');
}

function buildSchedCron() {
  const f = $('#sched-freq').value;
  if (f === 'custom') return $('#sched-cron').value.trim();
  const [hh, mm] = ($('#sched-time').value || '08:00').split(':').map((x) => parseInt(x, 10));
  const m = Number.isFinite(mm) ? mm : 0;
  const h = Number.isFinite(hh) ? hh : 8;
  if (f === 'daily') return `${m} ${h} * * *`;
  if (f === 'weekly') return `${m} ${h} * * ${$('#sched-weekday').value}`;
  if (f === 'monthly') return `${m} ${h} ${$('#sched-day').value} * *`;
  return `${m} * * * *`; // 每小时
}

// 把本程序生成的 cron 还原成人类可读描述；无法识别时原样显示
function describeCron(expr) {
  const p = (expr || '').trim().split(/\s+/);
  if (p.length !== 5) return expr || '';
  const [m, h, dom, mon, dow] = p;
  if (m === '*' && h === '*') return '每小时整点';
  const num = (s) => (/^\d+$/.test(s) ? parseInt(s, 10) : null);
  if (h === '*' && dom === '*' && mon === '*' && dow === '*' && num(m) !== null) {
    return `每小时第 ${num(m)} 分`;
  }
  const mm = num(m), hh = num(h);
  if (mm === null || hh === null) return expr;
  const t = `${String(hh).padStart(2, '0')}:${String(mm).padStart(2, '0')}`;
  if (dom === '*' && mon === '*' && dow === '*') return `每天 ${t}`;
  if (dom === '*' && mon === '*' && num(dow) !== null) {
    return `每${['周日', '周一', '周二', '周三', '周四', '周五', '周六'][num(dow) % 7]} ${t}`;
  }
  if (num(dom) !== null && mon === '*' && dow === '*') return `每月 ${dom} 号 ${t}`;
  return expr;
}

$('#sched-freq').addEventListener('change', syncSchedRow);
syncSchedRow();

async function loadSchedules() {
  try {
    const list = await apiGet('/api/schedules');
    $('#schedule-count').textContent = list.length;
    const box = $('#schedule-list');
    box.innerHTML = '';
    if (!list.length) {
      box.innerHTML = '<div class="list-empty">暂无定时任务。在左侧创建，到点后自动交给 AI 执行。</div>';
      return;
    }
    list.forEach((t) => {
      const item = document.createElement('div');
      item.className = 'list-item';
      item.innerHTML = `
        <div class="row">
          <span class="title">${esc(t.title)}</span>
          <span class="tag ${t.enabled ? 'ok' : 'off'}">${t.enabled ? '启用' : '停用'}</span>
        </div>
        <div class="meta">${esc(describeCron(t.cron_expr))} · 上次: ${esc(t.last_run || '从未')}</div>
        <div class="body">${esc(t.prompt)}</div>
        <div class="row"><div></div><div class="acts">
          <button data-toggle="${esc(t.id)}" data-enabled="${t.enabled ? '1' : '0'}">${t.enabled ? '停用' : '启用'}</button>
          <button data-del="${esc(t.id)}">删除</button>
        </div></div>`;
      box.appendChild(item);
    });
    box.querySelectorAll('[data-toggle]').forEach((b) => b.addEventListener('click', async () => {
      const enabled = b.dataset.enabled === '1';
      b.disabled = true;
      try {
        const j = await apiPost('/api/schedules/' + encodeURIComponent(b.dataset.toggle), { enabled: !enabled });
        if (j && j.ok === false) throw new Error(j.error || '后端未确认');
        loadSchedules();
      } catch (e) {
        toast('操作失败: ' + e.message, 'error');
        b.disabled = false;
      }
    }));
    box.querySelectorAll('[data-del]').forEach((b) => b.addEventListener('click', async () => {
      const yes = await confirmDialog('删除该定时任务？', { title: '删除定时任务', danger: true });
      if (!yes) return;
      try {
        const j = await apiDelete('/api/schedules/' + encodeURIComponent(b.dataset.del));
        if (j && j.ok === false) throw new Error(j.error || '后端未确认');
        toast('定时任务已删除', 'success');
      } catch (e) { toast('删除失败: ' + e.message, 'error'); }
    }));
  } catch (e) { /* ignore */ }
}

$('#schedule-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const body = {
    title: $('#sched-title').value.trim(),
    prompt: $('#sched-prompt').value.trim(),
    cron_expr: buildSchedCron(),
  };
  if (!body.title || !body.prompt) { toast('请填写任务标题和指令', 'warn'); return; }
  if (!body.cron_expr) { toast('请填写 cron 表达式', 'warn'); return; }
  await withBusy(e.submitter || $('#schedule-form button[type="submit"]'), '创建中…', async () => {
    try {
      const j = await apiPost('/api/schedules', body);
      if (j && j.ok === false) throw new Error(j.error || '后端未确认');
      $('#sched-title').value = ''; $('#sched-prompt').value = '';
      toast('定时任务已创建', 'success');
    } catch (err) { toast('创建失败: ' + err.message, 'error'); }
  });
});


// ---------------- 设置 ----------------
function keyRowHTML(k, idx) {
  return (
    '<div class="key-row">' +
    '<span class="k-badge">Key ' + (idx + 1) + '</span>' +
    '<input class="p-key" type="password" placeholder="同服务商 API Key" value="' + esc(k || '') + '" />' +
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

// ---- 模型配置：内置 API（OpenAI 兼容端点） ----
let modelList = [];
let activeModelIdx = 0;

function activeProvider() {
  if (!modelList.length) return null;
  return modelList[Math.min(activeModelIdx, modelList.length - 1)] || null;
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
    '<label class="pc-model-wrap">模型名 <input class="me-model" list="model-name-presets" placeholder="如 gpt-4o / qwen2.5 / deepseek-chat" /></label>' +
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
  const textOf = (m, i) => (m.name || ('模型 ' + (i + 1))) + (m.model ? ' · ' + m.model : '');

  if (label) label.textContent = '模型';
  if (newBtn) newBtn.hidden = false;
  if (delBtn) delBtn.hidden = false;

  if (!modelList.length) {
    optFor(sel, '', '（还没有模型配置，点「＋ 新建」）');
    if (chatSel) optFor(chatSel, '', '（无模型）');
    renderModelEdit();
    return;
  }

  modelList.forEach((m, i) => optFor(sel, String(i), textOf(m, i)));
  activeModelIdx = Math.min(Math.max(activeModelIdx, 0), modelList.length - 1);
  sel.value = String(activeModelIdx);

  if (chatSel) {
    chatSel.title = '在已保存的模型配置之间切换（立即生效）';
    modelList.forEach((m, i) => optFor(chatSel, String(i), textOf(m, i)));
    chatSel.value = String(activeModelIdx);
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
  box.innerHTML = modelEditHTML(m, idx + 1);
  box.querySelector('.me-name').value = m.name || '';
  box.querySelector('.me-url').value = m.base_url || '';
  box.querySelector('.me-model').value = m.model || '';
  box.querySelector('.me-rpm').value = m.rpm_limit ?? '';
  wireKeyList(box);
}

// Base URL 纠错：去掉误填的请求路径（/chat、/chat/v1、/v1/chat/completions），
// DeepSeek 统一成站点根，否则会拼出 /chat/v1/chat/completions → 404。
function normalizeProviderUrl(raw) {
  let s = (raw || '').trim().replace(/\/+$/, '');
  s = s.replace(/\/(v1\/chat\/completions|chat\/completions|chat\/v1|chat)$/, '');
  if (s.includes('api.deepseek.com')) return 'https://api.deepseek.com';
  return s;
}

// 把当前编辑区的内容写回内存（切模型/后端/保存前调用，避免未保存改动被覆盖）
function flushModelEdit() {
  if (!modelList.length) return;
  const edit = $('#model-edit');
  if (!edit || !edit.querySelector('.me-name')) return;
  const idx = Math.min(activeModelIdx, modelList.length - 1);
  const cur = modelList[idx];
  if (!cur) return;
  cur.name = (edit.querySelector('.me-name').value || '').trim() || cur.name || ('模型 ' + (idx + 1));
  cur.model = (edit.querySelector('.me-model').value || '').trim();
  cur.base_url = normalizeProviderUrl((edit.querySelector('.me-url').value || '').trim());
  // 模型名留空或写了不完整的 "deepseek" / "deepseek-v3" 时补成官方型号
  if (cur.base_url.includes('deepseek.com')) {
    if (!cur.model || /^(deepseek|deepseek-v3)$/.test(cur.model)) {
      cur.model = 'deepseek-chat';
    }
  }
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
    toast('模型切换保存失败: ' + e.message, 'error');
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
    form.voice.value = cfg.voice || '';
    form.rate.value = String(Number(((cfg.rate ?? 1.0)).toFixed(2)));
    form.tts_provider.value = cfg.tts_provider || 'edge';
    form.local_voice.value = cfg.local_voice || 'zf_001';
    form.local_speed.value = String(Number((cfg.local_speed ?? 1.0).toFixed(2)));
    form.kws_threshold.value = String(Number(((cfg.kws_threshold ?? 0.15)).toFixed(2)));
    form.max_record_secs.value = cfg.max_record_secs ?? 120;
    form.silence_limit_secs.value = cfg.silence_limit_secs ?? 1.5;
    form.no_speech_timeout_secs.value = cfg.no_speech_timeout_secs ?? 4;
    form.continuous_silence_secs.value = cfg.continuous_silence_secs ?? 1.5;
    form.continuous_max_record_secs.value = cfg.continuous_max_record_secs ?? 12;
    form.continuous_idle_rounds.value = cfg.continuous_idle_rounds ?? 3;
    form.max_turns.value = cfg.max_turns ?? 1000;
    form.rpm_limit.value = cfg.rpm_limit ?? 20;
    form.thinking_level.value = cfg.thinking_level || (cfg.enable_thinking ? 'high' : 'off');
    form.voice_enabled.checked = !!cfg.voice_enabled;
    window.voiceEnabled = !!cfg.voice_enabled;
    form.ask_user_enabled.checked = cfg.ask_user_enabled !== false;
    form.tool_loading.value = cfg.tool_loading || 'progressive';
    form.context_feeding_enabled.checked = !!cfg.context_feeding_enabled;
    form.context_recent_count.value = cfg.context_recent_count ?? 10;
    form.context_compress_count.value = cfg.context_compress_count ?? 30;
    form.output_language.value = cfg.output_language || 'zh';
    form.show_kws_log.checked = getKwsLogEnabled();
    const themeSel = $('#ui-theme');
    if (themeSel) themeSel.value = getUiTheme();
    loadToolLoadList();
  } catch (e) { /* ignore */ }
}

// 渐进式加载：渲染「默认加载」工具开关列表（每项独立开关）
async function loadToolLoadList() {
  const box = $('#tool-load-list');
  if (!box) return;
  try {
    const j = await apiGet('/api/tools');
    const tools = (j && j.tools) || [];
    const defaults = new Set((j && j.defaults) || []);
    box.innerHTML = '';
    if (!tools.length) {
      box.innerHTML = '<div class="tool-load-empty">暂无按需加载的工具</div>';
      return;
    }
    tools.forEach((t) => {
      const row = document.createElement('label');
      row.className = 'tool-load-row';
      const sw = document.createElement('span');
      sw.className = 'switch';
      const cb = document.createElement('input');
      cb.type = 'checkbox';
      cb.value = t.name;
      cb.checked = defaults.has(t.name);
      const track = document.createElement('span');
      track.className = 'track';
      sw.appendChild(cb);
      sw.appendChild(track);
      const name = document.createElement('span');
      name.className = 'tl-name';
      name.textContent = t.name;
      const desc = document.createElement('span');
      desc.className = 'tl-desc';
      desc.textContent = t.description || '';
      desc.title = t.description || '';
      row.appendChild(sw);
      row.appendChild(name);
      row.appendChild(desc);
      box.appendChild(row);
    });
  } catch (e) {
    box.innerHTML = '<div class="tool-load-empty">读取工具列表失败</div>';
  }
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

// 设置页模型下拉：列出全部模型配置
$('#model-select').addEventListener('change', async (e) => {
  const val = e.target.value;
  if (!val) return;
  flushModelEdit();
  activeModelIdx = parseInt(val, 10) || 0;
  renderModelSelector();
  await saveModels();
});

// 对话界面底部直接切换模型，并立即持久化
$('#chat-model-select').addEventListener('change', async (e) => {
  const val = e.target.value;
  if (!val) return;
  const idx = parseInt(val, 10);
  if (!Number.isNaN(idx) && modelList[idx]) {
    activeModelIdx = idx;
    renderModelSelector();
    await saveModels();
  }
});

// 页面样式主题切换（本地 UI 偏好，立即生效，不入后端配置）
$('#ui-theme').addEventListener('change', (e) => applyUiTheme(e.target.value));

$('#btn-model-new').addEventListener('click', () => {
  flushModelEdit();
  const ord = modelList.length + 1;
  modelList.push({ name: '模型 ' + ord, base_url: '', model: '', api_keys: [''], api_key: '', rpm_limit: null });
  activeModelIdx = modelList.length - 1;
  renderModelSelector();
  const url = document.querySelector('#model-edit .me-url');
  if (url) url.focus();
});

$('#btn-model-del').addEventListener('click', async () => {
  if (modelList.length <= 1) return;
  const cur = modelList[Math.min(activeModelIdx, modelList.length - 1)];
  const yes = await confirmDialog(`删除模型配置「${(cur && cur.name) || ''}」？`, { title: '删除模型配置', danger: true });
  if (!yes) return;
  flushModelEdit();
  modelList.splice(activeModelIdx, 1);
  activeModelIdx = Math.min(activeModelIdx, modelList.length - 1);
  renderModelSelector();
  saveModels();
});

$('#settings-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const f = e.target;
  // 唤醒日志显隐是本地 UI 偏好（默认关闭），不入后端配置
  localStorage.setItem('kws_log_enabled', f.show_kws_log.checked ? '1' : '0');
  if (!f.show_kws_log.checked) kwsLog.hidden = true;
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
    voice: f.voice.value.trim(),
    rate: Math.round((parseFloat(f.rate.value) || 1.0) * 100) / 100,
    tts_provider: f.tts_provider.value,
    local_voice: f.local_voice.value,
    local_speed: Math.min(2, Math.max(0.5, parseFloat(f.local_speed.value) || 1.0)),
    kws_threshold: Math.round((parseFloat(f.kws_threshold.value) || 0.15) * 100) / 100,
    max_record_secs: parseFloat(f.max_record_secs.value) || 120,
    silence_limit_secs: parseFloat(f.silence_limit_secs.value) || 1.5,
    no_speech_timeout_secs: parseFloat(f.no_speech_timeout_secs.value) || 4,
    continuous_silence_secs: parseFloat(f.continuous_silence_secs.value) || 1.5,
    continuous_max_record_secs: parseFloat(f.continuous_max_record_secs.value) || 12,
    continuous_idle_rounds: parseInt(f.continuous_idle_rounds.value, 10) || 3,
    max_turns: parseInt(f.max_turns.value, 10) || 1000,
    rpm_limit: parseInt(f.rpm_limit.value, 10) || 20,
    thinking_level: f.thinking_level.value,
    enable_thinking: f.thinking_level.value !== 'off',
    voice_enabled: f.voice_enabled.checked,
    ask_user_enabled: f.ask_user_enabled.checked,
    tool_loading: f.tool_loading.value || 'progressive',
    context_feeding_enabled: f.context_feeding_enabled.checked,
    context_recent_count: parseInt(f.context_recent_count.value, 10) || 10,
    context_compress_count: Math.max(0, parseInt(f.context_compress_count.value, 10) || 0),
    default_loaded_tools: Array.from(
      document.querySelectorAll('#tool-load-list input[type="checkbox"]:checked')
    ).map((c) => c.value),
    output_language: f.output_language.value || 'zh',
  };
  window.voiceEnabled = cfg.voice_enabled;
  // 防呆：若模型列表尚未加载成功，不要用空列表覆盖已有配置
  if (!cfg.providers.length) {
    try {
      const cur = await apiGet('/api/config');
      if ((cur.providers || []).length) {
        cfg.providers = cur.providers;
        cfg.active_model = cur.active_model;
        cfg.main_base_url = cur.main_base_url;
        cfg.main_api_key = cur.main_api_key;
        cfg.main_model = cur.main_model;
        cfg.sub_base_url = cur.sub_base_url;
        cfg.sub_api_key = cur.sub_api_key;
        cfg.sub_model = cur.sub_model;
      }
    } catch (_) {}
  }
  await withBusy(e.submitter || $('#settings-form button[type="submit"]'), '保存中…', async () => {
    try {
      const r = await apiPost('/api/config', cfg);
      if (r && r.ok === false) throw new Error(r.error || '后端未确认');
      flashSaved();
      toast('设置已保存，Agent 已重建', 'success');
    } catch (err) { toast('保存失败: ' + err.message, 'error'); }
  });
});

// ---------------- 工具 ----------------
function esc(s) {
  return String(s ?? '').replace(/[&<>"']/g, (c) => (
    { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]
  ));
}

// ---------------- 连续曲率圆角（Apple Squircle / G2） ----------------
// 普通 border-radius 是圆弧，和直边相切处曲率突变；这里用 Figma 的 squircle
// 参数化路径给有圆角的元素加 clip-path，让曲率在角落平滑上升/下降。
function sqCornerParams(cornerRadius, smoothing, budget) {
  let p = (1 + smoothing) * cornerRadius;
  const maxSmoothing = budget / cornerRadius - 1;
  smoothing = Math.min(smoothing, maxSmoothing);
  p = Math.min(p, budget);
  const arcMeasure = 90 * (1 - smoothing);
  const rad = (deg) => (deg * Math.PI) / 180;
  const arcSectionLength = Math.sin(rad(arcMeasure / 2)) * cornerRadius * Math.SQRT2;
  const angleAlpha = (90 - arcMeasure) / 2;
  const p3p4 = cornerRadius * Math.tan(rad(angleAlpha / 2));
  const angleBeta = 45 * smoothing;
  const c = p3p4 * Math.cos(rad(angleBeta));
  const d = c * Math.tan(rad(angleBeta));
  const b = (p - arcSectionLength - c - d) / 3;
  return { a: 2 * b, b, c, d, p, arcSectionLength, cornerRadius };
}
function squirclePath(w, h, radius, smoothing = 0.6) {
  const budget = Math.min(w, h) / 2;
  const r = Math.min(radius, budget);
  if (r <= 0.5) return '';
  const { a, b, c, d, p, arcSectionLength } = sqCornerParams(r, smoothing, budget);
  const f = (n) => Number(n.toFixed(3));
  const A = f(a), B = f(b), C = f(c), D = f(d), P = f(p), L = f(arcSectionLength);
  return [
    `M ${f(w - p)} 0`,
    `c ${A} 0 ${A + B} 0 ${A + B + C} ${D}`,
    `a ${f(r)} ${f(r)} 0 0 1 ${L} ${L}`,
    `c ${D} ${C} ${D} ${B + C} ${D} ${A + B + C}`,
    `L ${f(w)} ${f(h - p)}`,
    `c 0 ${A} 0 ${A + B} ${-D} ${A + B + C}`,
    `a ${f(r)} ${f(r)} 0 0 1 ${-L} ${L}`,
    `c ${-C} ${D} ${-(B + C)} ${D} ${-(A + B + C)} ${D}`,
    `L ${P} ${f(h)}`,
    `c ${-A} 0 ${-(A + B)} 0 ${-(A + B + C)} ${-D}`,
    `a ${f(r)} ${f(r)} 0 0 1 ${-L} ${-L}`,
    `c ${-D} ${-C} ${-D} ${-(B + C)} ${-D} ${-(A + B + C)}`,
    `L 0 ${P}`,
    `c 0 ${-A} 0 ${-(A + B)} ${D} ${-(A + B + C)}`,
    `a ${f(r)} ${f(r)} 0 0 1 ${L} ${-L}`,
    `c ${C} ${-D} ${B + C} ${-D} ${A + B + C} ${-D}`,
    'Z',
  ].join(' ');
}
const SQUIRCLE_SELECTOR = [
  'button', 'input', 'select', 'textarea',
  // 注意：.session-item 不能加 clip-path —— 它的 ⋮ 菜单是绝对定位子元素，
  // 会被父元素的 clip-path 整个裁掉，表现为"点了没反应、弹不出来"。
  '.card', '.list-item', '.nav-item', '.bubble', '.composer',
  '.pop', '.modal', '.toast', '.tag', '.badge', '.chip', '.tool-act', '.tl-activity',
].join(',');
function applySquircle(el) {
  if (!el || el.closest('.lcars')) return;
  if (!el.getClientRects().length) return;
  const cs = getComputedStyle(el);
  if (cs.clipPath && cs.clipPath !== 'none' && !cs.clipPath.startsWith('path(')) return; // 已有其它自定义形状
  const raw = cs.borderTopLeftRadius;
  if (raw.includes('%')) return;
  const radius = parseFloat(raw);
  if (!Number.isFinite(radius) || radius < 3) return;
  const w = el.offsetWidth;
  const h = el.offsetHeight;
  if (!w || !h) return;
  const budget = Math.min(w, h) / 2;
  if (radius >= budget - 0.5) return; // 胶囊/圆形本身已连续
  const key = `${w}x${h}:${radius}`;
  if (el.dataset.sq === key) return; // 尺寸/半径未变
  const path = squirclePath(w, h, radius);
  if (!path) return;
  el.dataset.sq = key;
  el.style.clipPath = `path('${path}')`;
}
let squircleTimer = 0;
function refreshSquircles() {
  clearTimeout(squircleTimer);
  squircleTimer = setTimeout(() => {
    document.querySelectorAll(SQUIRCLE_SELECTOR).forEach(applySquircle);
  }, 120);
}
const squircleObserver = new MutationObserver(refreshSquircles);
squircleObserver.observe(document.body, { childList: true, subtree: true });
window.addEventListener('resize', refreshSquircles);
// 侧栏收起/展开等宽度动画结束后重算，保证圆角随尺寸变形
document.addEventListener('transitionend', (e) => {
  if (e.propertyName === 'width' || e.propertyName === 'grid-template-columns') refreshSquircles();
}, true);
refreshSquircles();

// ---------------- 启动 ----------------
// Electron 主进程通知：后端崩溃/自动重启（渲染层立即给出可见反馈）
if (window.star && window.star.onBackendDown) {
  window.star.onBackendDown((message) => {
    connected = false;
    const b = $('#conn-banner');
    if (b) {
      b.textContent = message || '后端已断开 · 正在自动重启…';
      b.hidden = false;
    }
    toast(message || '后端进程退出，正在自动重启', 'error', 6000);
  });
}
pollStatus();
statusTimer = setInterval(pollStatus, 3000);
loadMemory();
loadSkills();
loadSchedules();
loadSubagents();
const saRefreshBtn = $('#btn-subagents-refresh');
if (saRefreshBtn) saRefreshBtn.addEventListener('click', () => loadSubagents());
renderSubagents();

// 子 Agent 管理：按时间批量清理
const saPurgeBtn = $('#sa-purge-btn');
if (saPurgeBtn) saPurgeBtn.addEventListener('click', async () => {
  const days = Number(($('#sa-purge-range') || {}).value || 0);
  const withConv = !!(($('#sa-del-conv') || {}).checked);
  const rangeText = days === 1 ? '24 小时前' : days > 0 ? `${days} 天前` : '全部';
  const tip = `确定清理 ${rangeText}的已结束子 Agent 记录？` +
    (withConv ? '\n同时会删除主对话中的完成卡片。' : '\n主对话中的卡片会保留。');
  if (!confirm(tip)) return;
  saPurgeBtn.disabled = true;
  try {
    const beforeMs = days > 0 ? Date.now() - days * 86400000 : 0;
    const j = await apiPost('/api/subagents/purge', {
      before_ms: beforeMs,
      with_conversation: withConv,
    });
    toast(`已清理 ${(j && j.removed) || 0} 条记录`, 'success');
  } catch (e) {
    toast('清理失败: ' + e.message, 'error');
  }
  saPurgeBtn.disabled = false;
  await loadSubagents();
});

// 子 Agent 全屏详情：退出 / 发起对话 / 中止
const saDetailBox = $('#sa-detail');
const saDetailBack = $('#sa-detail-back');
if (saDetailBack) saDetailBack.addEventListener('click', closeSubagentDetail);
const saDetailJump = $('#sa-detail-jump');
if (saDetailJump) saDetailJump.addEventListener('click', () => {
  const sid = saDetailTask && saDetailTask.parent_session;
  closeSubagentDetail();
  if (sid) subagentJump(sid);
});
const saDetailCancel = $('#sa-detail-cancel');
if (saDetailCancel) saDetailCancel.addEventListener('click', async () => {
  if (!saDetailId) return;
  saDetailCancel.disabled = true;
  try {
    await apiPost(`/api/subagents/${encodeURIComponent(saDetailId)}/cancel`, {});
  } catch (e) {
    toast('中止失败: ' + e.message, 'error');
  }
  saDetailCancel.disabled = false;
});
const saDetailForm = $('#sa-detail-form');
if (saDetailForm) saDetailForm.addEventListener('submit', async (e) => {
  e.preventDefault();
  const input = $('#sa-detail-input');
  const text = (input && input.value || '').trim();
  if (!text || !saDetailId) return;
  const sendBtn = $('#sa-detail-send');
  const id = saDetailId;
  sendBtn.disabled = true;
  input.value = '';
  appendDetailTrace({ id, entry: { kind: 'user', text, at: Date.now() } });
  try {
    const j = await apiPost(`/api/subagents/${encodeURIComponent(id)}/message`, { text });
    if (!j || j.ok === false) throw new Error((j && j.error) || '发送失败');
    if (saDetailTask) {
      saDetailTask.status = 'running';
      saDetailTask.progress = j.queued ? '已加入队列，等待下一轮…' : '继续执行…';
      renderSubagentDetail();
    }
    toast(j.queued ? '指令已加入队列' : '子 Agent 已继续执行', 'success');
    await loadSubagents();
  } catch (err) {
    toast('发送失败: ' + err.message, 'error');
  }
  sendBtn.disabled = false;
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && saDetailBox && !saDetailBox.hidden) closeSubagentDetail();
});
loadConfig();
loadSessions();
kwsLogTimer = setInterval(pollKwsLog, 3000);
waveTimer = setInterval(pollVoiceLevel, 320);
openEvents();
