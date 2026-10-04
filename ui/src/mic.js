// mic.js — 语音监听控制（纯 Rust 后端采集架构，前端不采集音频）
// 架构：麦克风采集 / 唤醒词检测 / STT / TTS / 播放全部在纯 Rust 后端
// （voice-serve，cpal 常驻采集）完成。渲染进程（Electron）只做两件事：
//   1) 触发一次系统麦克风授权（getUserMedia，随后立即释放流）——
//      权限归属 Electron 应用本体，其子进程 core / voice-serve 继承
//      responsible-process 权限后，cpal 才能访问麦克风；
//   2) 开启/关闭监听：开启后重启后端常驻采集（/reinit_capture），
//      再把 /api/voice/listening 置 true，唤醒/识别/播报状态由 SSE 推送。
'use strict';

(() => {
  const VOICE = (window.star && window.star.voiceUrl) || 'http://127.0.0.1:8420';

  let state = 'idle'; // idle | standby（后端监听中） | err
  const listeners = {};

  function emit(ev, data) { (listeners[ev] || []).forEach((fn) => fn(data)); }
  function setState(s) { state = s; emit('state', { state: s }); }

  // 触发一次 getUserMedia 仅用于取得系统麦克风授权，随后立即释放流。
  // 若用户此前已在系统设置拒绝，返回 false，提示其手动授权。
  async function ensureMicPermission() {
    let stream = null;
    try {
      stream = await navigator.mediaDevices.getUserMedia({ audio: true });
      return true;
    } catch {
      return false;
    } finally {
      if (stream) stream.getTracks().forEach((t) => t.stop());
    }
  }

  // 开启监听：①授权麦克风 ②复用/重建后端常驻采集（失败则中止，不虚报监听）
  async function start() {
    const granted = await ensureMicPermission();
    if (!granted) {
      setState('err');
      emit('mic_error', { message: '麦克风未授权：请在「系统设置 → 隐私与安全性 → 麦克风」中为本应用开启权限后重试。' });
      return;
    }
    // 查询后端采集状态：采集已就绪则直接复用，避免反复重建采集导致设备句柄竞态。
    // 只有采集不可用（首次授权后 / 后端刚启动）才重建，且必须确认重建成功才继续。
    let needReinit = true;
    try {
      const st = await (await fetch(`${VOICE}/status`)).json();
      needReinit = !(st.mic_alive && st.cap_ok);
    } catch { needReinit = true; }
    if (needReinit) {
      let reinitOk = false;
      for (let attempt = 0; attempt < 2 && !reinitOk; attempt++) {
        try {
          const r = await fetch(`${VOICE}/reinit_capture`, { method: 'POST' });
          reinitOk = (await r.text()).includes('capture reinitialized');
        } catch { reinitOk = false; }
        if (!reinitOk) await new Promise((res) => setTimeout(res, 400));
      }
      if (!reinitOk) {
        setState('err');
        emit('mic_error', { message: '后端麦克风采集初始化失败，请稍后重试。' });
        return;
      }
    }
    setState('standby');
  }

  function stop() {
    setState('idle');
  }

  window.micEngine = {
    start,
    stop,
    on(ev, fn) { (listeners[ev] = listeners[ev] || []).push(fn); },
    getState: () => state,
  };
})();
