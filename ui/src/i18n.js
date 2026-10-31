// 界面中英文切换：以「中文原文」为 key 的字典 + DOM 自动翻译。
// - 静态 HTML 与 JS 动态生成的内容都会被翻译（MutationObserver 监听新增/变更节点）。
// - 原文会记录在节点上，切回中文时可无损还原。
// 无需逐处加 data-i18n 属性，运维成本最低。
'use strict';

(function () {
  const EN = {
    // ---- 导航 / 顶栏 ----
    '对话': 'Chat',
    '记忆': 'Memory',
    '定时任务': 'Schedules',
    '设置': 'Settings',
    '新建对话': 'New chat',
    '新建': 'New',
    '后端未连接 · 正在重连…': 'Backend disconnected · reconnecting…',
    '唤醒日志（排查唤醒问题）': 'Wake log (troubleshoot wake issues)',
    '输入指令，或说唤醒词「computer」后直接说话…': 'Type a command, or say the wake word "computer" to speak…',
    '开启/关闭语音监听': 'Toggle voice listening',
    '语音监听': 'Voice listening',
    '开启多轮对话模式（连续对话，无需重复唤醒）': 'Enable continuous mode (no repeated wake word)',
    '多轮对话模式': 'Continuous mode',
    '发送': 'Send',

    // ---- 记忆 ----
    '记忆系统': 'Memory System',
    '清空全部': 'Clear all',
    '新增记忆': 'Add Memory',
    '知识 (knowledge)': 'Knowledge (knowledge)',
    '偏好 (preference)': 'Preference (preference)',
    '踩坑 (gotcha)': 'Gotcha (gotcha)',
    '经历 (narrative)': 'Narrative (narrative)',
    '方法 (workflow)': 'Workflow (workflow)',
    '标题': 'Title',
    '内容': 'Content',
    '保存': 'Save',
    '记忆条目 (': 'Memory entries (',

    // ---- Skills ----
    'Skills 系统': 'Skills System',
    '已加载 (': 'Loaded (',
    '导入 Skill': 'Import Skill',
    '每个 skill 是一个文件夹，内含运行文件 + 说明文档（README.md / SKILL.md）。': 'Each skill is a folder containing a runtime file + docs (README.md / SKILL.md).',
    'skill 文件夹绝对路径，如 ~/Desktop/foo-skill': 'Absolute path to the skill folder, e.g. ~/Desktop/foo-skill',
    '导入': 'Import',

    // ---- 定时任务 ----
    '新建定时任务': 'New Schedule',
    '任务标题': 'Task title',
    '到点后交给主 Agent 执行的指令': 'Instruction for the main Agent to run at the scheduled time',
    'cron 表达式，如 0 8 * * *（每天 8:00）': 'cron expression, e.g. 0 8 * * * (daily at 8:00)',
    '创建': 'Create',
    '任务列表 (': 'Task list (',

    // ---- 设置 ----
    '已保存': 'Saved',
    '模型': 'Model',
    '语音': 'Voice',
    '行为': 'Behavior',
    '显示': 'Display',
    '模型配置（可新建多套，顶部切换当前使用）': 'Model configs (create multiple; switch active on top)',
    '当前模型': 'Current model',
    '＋ 新建': '＋ New',
    '删除': 'Delete',
    '每套模型配置 = Base URL + 模型名 + 多个 API Key（请求按 round-robin 分摊到各 Key，各自遵守 RPM 限速）。顶部下拉切换当前使用的模型，保存后立即生效；本地 Ollama 可留空 Key，关闭思考时自动使用 -nothink 变体。':
      'Each model config = Base URL + model name + multiple API keys (requests are distributed round-robin across keys, each honoring its own RPM limit). Switch the active model via the top dropdown; takes effect on save. Local Ollama can leave the key blank; when thinking is off, the -nothink variant is used automatically.',
    'AI 人设（身份 / 性格，可自由编辑）': 'AI persona (identity / personality, freely editable)',
    '核心行为规则（语音播报格式、单 Agent、主动询问等）由 resources/prompts/system.md 系统提示词文件强制注入，人设叠加在规则之后，不影响系统规则。':
      'Core behavior rules (voice broadcast format, single-agent, proactive asking, etc.) are injected from resources/prompts/system.md; the persona is layered on top and does not override system rules.',
    '唤醒词': 'Wake word',
    '唤醒词阈值': 'Wake word threshold',
    '提示音文件': 'Beep file',
    'TTS 音色': 'TTS voice',
    '语速': 'Rate',
    'TTS 后端': 'TTS backend',
    '打断关键词（逗号分隔）': 'Interrupt keywords (comma-separated)',
    '最长录音秒数': 'Max record secs',
    '启用语音': 'Enable voice',
    '静默结束秒数': 'Silence end secs',
    '多轮静默秒数': 'Continuous silence secs',
    '多轮最长录音秒数': 'Continuous max record secs',
    '多轮静默退出轮数': 'Continuous idle exit rounds',
    '「静默结束秒数」：普通语音指令识别到人声后，静默多久算说完（默认 2.5）。「多轮静默秒数」：连续对话模式每句的静默结束阈值（默认 1.5，说完这么久即发送）。「多轮最长录音秒数」：连续模式单轮最长录音。「多轮静默退出轮数」：连续多少轮没人说话自动退出。':
      '"Silence end secs": after speech is detected for a normal voice command, how long silence counts as end-of-speech (default 2.5). "Continuous silence secs": per-sentence silence threshold in continuous mode (default 1.5; sent after this long). "Continuous max record secs": max recording per turn in continuous mode. "Continuous idle exit rounds": auto-exit after this many silent rounds.',
    '每分钟请求数（RPM）': 'Requests per minute (RPM)',
    '最大轮数': 'Max turns',
    '模型思考（thinking，默认关闭）': 'Model thinking (default off)',
    '允许 AI 中途询问（语音问答）': 'Allow AI to ask mid-turn (voice Q&A)',
    '输出语言': 'Output language',
    '中文（强制）': 'Chinese (forced)',
    'English（强制）': 'English (forced)',
    '自动（跟随用户）': 'Auto (follow user)',
    '英文音色': 'English voice',
    '中文/英文：提示词强制 AI 用该语言回复正文与【Voice】播报；自动：AI 自行判断（默认跟随用户输入语言）。选英文且当前 TTS 音色是中文音色时，播报自动改用「英文音色」。':
      'Chinese/English: the prompt forces the AI to reply and broadcast【Voice】in that language. Auto: the AI decides (defaults to following the user input language). If English is chosen and the current TTS voice is a Chinese voice, broadcast automatically switches to the "English voice".',
    '开启后，AI 在意图不清、需要拍板或需要关键信息时，可主动播报问题并聆听你的语音回复；关闭后 AI 不再中途提问。RPM 超限时请求在本地排队等待，上游拒绝时自动重试；关闭模型思考可大幅加快本地 qwen3/llama 响应（自动使用带 -nothink 后缀的模型）。':
      'When enabled, the AI may broadcast a question and listen for your voice reply when intent is unclear, a decision is needed, or key info is missing; when disabled, the AI will not ask mid-turn. Requests over the RPM limit queue locally and retry on upstream rejection; disabling thinking greatly speeds up local qwen3/llama (automatically uses the -nothink model variant).',
    '显示唤醒日志（默认关闭）': 'Show wake log (default off)',
    '显示 AI 思考过程（reasoning）': 'Show AI reasoning',
    '页面样式': 'Page style',
    '经典（浅色）': 'Classic (light)',
    'LCARS 风格': 'LCARS style',
    '界面语言': 'UI language',
    '中文': 'Chinese',
    '页面样式：经典（浅色商务风）/ LCARS 风格（星舰终端风）。界面语言切换仅影响界面文字，与「输出语言」（AI 回复语言）无关。': 'Page style: Classic (light) / LCARS (starship terminal). UI language only affects interface text, unrelated to "Output language" (the AI reply language).',
    '保存并重建 Agent': 'Save & rebuild Agent',

    // ---- 会话列表 ----
    '会话服务不可用': 'Session service unavailable',
    '暂无对话，点击右上角新建': 'No conversations yet — click New on the right',
    '会话菜单': 'Session menu',
    '切换到此对话': 'Switch to this chat',
    '删除对话': 'Delete chat',
    '删除该对话？': 'Delete this conversation?',

    // ---- 语音状态 ----
    '开启语音监听': 'Enable voice listening',
    '关闭语音监听': 'Disable voice listening',
    '监听开启失败（语音服务不可用）': 'Failed to enable listening (voice service unavailable)',
    '监听开启失败': 'Failed to enable listening',
    '监听中：等待唤醒词': 'Listening: waiting for wake word',
    '监听中（绿色）· 点击关闭': 'Listening (green) · click to stop',
    '关闭失败，后端仍在监听（绿色）': 'Stop failed; backend still listening (green)',
    '后端仍在监听': 'Backend still listening',
    '监听未开启': 'Listening off',
    '监听已停止': 'Listening stopped',
    '麦克风不可用': 'Microphone unavailable',
    '唤醒模型未加载': 'Wake model not loaded',
    '语音服务未连接': 'Voice service not connected',
    '语音已禁用（设置里开启）': 'Voice disabled (enable in Settings)',
    '语音监听中': 'Voice listening',
    '语音播报中（蓝色）': 'Announcing (blue)',
    '语音播报中…': 'Announcing…',
    '语音播报': 'Voice',
    '主 Agent 处理中（紫色）': 'Main Agent working (purple)',
    '已唤醒·录音处理中（黄色）': 'Awake · processing audio (yellow)',
    '多轮对话模式已开启 · 点击结束': 'Continuous mode on · click to end',
    '操作失败：语音服务不可用': 'Action failed: voice service unavailable',

    // ---- 确认 / 提示 ----
    '确定清空全部记忆？': 'Clear all memory?',
    '删除该定时任务？': 'Delete this schedule?',

    // ---- 对话流 / 工具 ----
    '新的对话。输入指令，或开启监听后说唤醒词「computer」开始语音交互。': 'New conversation. Type a command, or enable listening and say the wake word "computer" to start voice interaction.',
    '切换会话失败：后端未确认（会话可能已被删除）': 'Switch failed: backend did not confirm (the conversation may have been deleted)',
    '消息加载失败': 'Failed to load messages',
    '发送失败': 'Send failed',
    '正在思考': 'Thinking',
    '正在思考…': 'Thinking…',
    '正在处理': 'Working',
    '正在处理…': 'Working…',
    '正在停止…': 'Stopping…',
    '暂停（打断 AI）': 'Pause (interrupt AI)',
    '处理中': 'Processing',
    '执行中…': 'Running…',
    '未命中': 'miss',
    '（无结果）': '(no results)',
    '（无模型配置）': '(no model configs)',
    '（无说明）': '(no description)',
    '备用 API': 'Backup API',
    '主 API': 'Main API',
    '本地 · Agent 工具': 'Local · Agent tools',
    '编辑文本': 'Edit text',
    '查找文件': 'Find files',
    '读取文件': 'Read file',
    '写入文件': 'Write file',
    '修改文件': 'Edit file',
    '抓取网页内容': 'Fetch web page',
    '搜索网页': 'Search web',
    '联网搜索': 'Web search',
    '搜索文件内容': 'Search file contents',
    '运行 Python 代码': 'Run Python code',
    '执行终端命令': 'Run terminal command',
    '执行结果': 'Result',
    '输入参数': 'Input',
    '复制结果': 'Copy result',
    '已复制': 'Copied',
    '点击在访达中显示：': 'Click to reveal in Finder:',
    '登记交付物': 'Register deliverable',
    '从未': 'Never',
    '错误': 'Error',
    '处理': 'Process',
    '启用': 'Enable',
    '停用': 'Disable',
    '打开失败': 'Open failed',
    '删除该 Key': 'Delete this key',
    '添加一个同配置 API Key': 'Add another API key with the same config',
    '＋ 添加一个 Key': '＋ Add key',
    '同服务商 API Key': 'API key (same provider)',
    '名称': 'Name',
    '模型名': 'Model name',
    '每个 Key 限额 RPM': 'Per-key RPM limit',
    '如 主 API / 本地 Ollama': 'e.g. Main API / local Ollama',
    '如 gpt-4o / qwen2.5 / deepseek-chat': 'e.g. gpt-4o / qwen2.5 / deepseek-chat',
    'OpenAI 兼容端点，如 https://api.openai.com/v1（带不带 /v1 均可）': 'OpenAI-compatible endpoint, e.g. https://api.openai.com/v1 (/v1 optional)',
    '还没有模型配置，点「＋ 新建」添加第一套。': 'No model config yet — click "＋ New" to add one.',
    '暂无唤醒检测记录 —— 请开启监听并尝试说唤醒词，这里会显示每次检测的样本量 / 音量 / 耗时。': 'No wake detections yet — enable listening and try saying the wake word; each detection\'s sample count / volume / latency shows here.',
  };

  // 前缀规则：动态拼接（如 '删除失败: ' + msg）时只翻译前缀。
  const PREFIX = [
    ['删除失败: ', 'Delete failed: '],
    ['切换会话失败: ', 'Switch failed: '],
    ['新建会话失败: ', 'New session failed: '],
    ['发送失败: ', 'Send failed: '],
    ['保存失败: ', 'Save failed: '],
    ['创建失败: ', 'Create failed: '],
    ['导入失败: ', 'Import failed: '],
    ['会话消息加载失败：', 'Failed to load session messages: '],
    ['已完成，用时 ', 'Done in '],
    ['已交付 ', 'Delivered '],
    ['耗时 ', 'Time '],
    ['模型 ', 'Model '],
    ['新模型 ', 'New model '],
  ];

  // 正则规则：带数字的拼接文本。
  const RULES = [
    [/^(\d+) 分 (\d+) 秒$/, (m) => `${m[1]} min ${m[2]} s`],
    [/^(\d+) 分$/, (m) => `${m[1]} min`],
    [/^(\d+) 秒$/, (m) => `${m[1]} s`],
    [/^(\d+) 个文件$/, (m) => `${m[1]} files`],
  ];

  const ATTRS = ['title', 'placeholder', 'aria-label'];
  let lang = localStorage.getItem('st_ui_lang') === 'en' ? 'en' : 'zh';
  let applying = false;
  let observer = null;

  function translate(s) {
    if (lang !== 'en' || !s) return s;
    const key = s.trim();
    if (!key) return s;
    if (Object.prototype.hasOwnProperty.call(EN, key)) return s.replace(key, EN[key]);
    for (const [zh, en] of PREFIX) {
      if (key.startsWith(zh)) return s.replace(zh, en);
    }
    for (const [re, fn] of RULES) {
      const m = key.match(re);
      if (m) return s.replace(key, fn(m));
    }
    return s;
  }

  function doText(node) {
    const raw = node.nodeValue;
    if (!raw || !raw.trim()) return;
    const out = translate(raw);
    if (out === raw) return;
    if (node.__i18nOrig === undefined) node.__i18nOrig = raw;
    node.nodeValue = out;
  }
  function restoreText(node) {
    if (node.__i18nOrig !== undefined) { node.nodeValue = node.__i18nOrig; }
  }

  function doEl(el) {
    for (const a of ATTRS) {
      const v = el.getAttribute && el.getAttribute(a);
      if (!v) continue;
      const out = translate(v);
      if (out === v) continue;
      if (!el.__i18nOrigAttr) el.__i18nOrigAttr = {};
      if (el.__i18nOrigAttr[a] === undefined) el.__i18nOrigAttr[a] = v;
      el.setAttribute(a, out);
    }
  }
  function restoreEl(el) {
    if (el.__i18nOrigAttr) {
      for (const a in el.__i18nOrigAttr) el.setAttribute(a, el.__i18nOrigAttr[a]);
    }
  }

  function walk(root, fn) {
    const tw = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT, null);
    let n = tw.currentNode;
    while (n) {
      if (n.nodeType === 3) fn.text(n);
      else fn.el(n);
      n = tw.nextNode();
    }
  }

  function applyAll() {
    if (applying) return;
    applying = true;
    try {
      document.documentElement.lang = lang === 'en' ? 'en' : 'zh-CN';
      if (lang === 'en') walk(document.body, { text: doText, el: doEl });
      else walk(document.body, { text: restoreText, el: restoreEl });
    } finally {
      applying = false;
    }
  }

  function startObserver() {
    if (observer) return;
    observer = new MutationObserver((muts) => {
      if (lang !== 'en') return;
      applying = true;
      try {
        for (const m of muts) {
          if (m.type === 'characterData') doText(m.target);
          else if (m.type === 'attributes') doEl(m.target);
          else m.addedNodes.forEach((n) => {
            if (n.nodeType === 3) doText(n);
            else if (n.nodeType === 1) walk(n, { text: doText, el: doEl });
          });
        }
      } finally { applying = false; }
    });
    observer.observe(document.body, {
      childList: true, subtree: true, characterData: true,
      attributes: true, attributeFilter: ATTRS,
    });
  }

  function setLang(next) {
    lang = next === 'en' ? 'en' : 'zh';
    localStorage.setItem('st_ui_lang', lang);
    applyAll();
    updateToggle();
  }

  function updateToggle() {
    const sel = document.getElementById('ui-lang');
    if (sel) sel.value = lang;
  }

  window.getUiLang = () => lang;
  window.setUiLang = setLang;

  function init() {
    applyAll();
    startObserver();
    updateToggle();
    const sel = document.getElementById('ui-lang');
    if (sel) sel.addEventListener('change', () => setLang(sel.value));
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
