// 后端 API 轻封装（渲染进程直连本地 core，CORS 已由后端放行）
// 不依赖 preload：window.star 注入成功则用之，否则回退到默认本地端口，
// 保证即使 preload 未执行，前端 UI 也能正常工作。
'use strict';

const CORE = (window.star && window.star.coreUrl) || 'http://127.0.0.1:8410';

// 带状态的错误：status=HTTP 状态码（网络错误/超时为 0），message 保持原格式供 UI 展示
class ApiError extends Error {
  constructor(status, message, body) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.body = body;
  }
}

async function api(method, path, body, timeoutMs = 15000) {
  const opts = {
    method,
    headers: {},
    // 本地后端不应超过阈值；超时/后端未启动时统一抛 ApiError(status=0)
    signal: AbortSignal.timeout(timeoutMs),
  };
  if (body !== undefined) {
    opts.headers['Content-Type'] = 'application/json';
    opts.body = JSON.stringify(body);
  }
  let res;
  try {
    res = await fetch(CORE + path, opts);
  } catch (e) {
    const msg = e && e.name === 'TimeoutError' ? '请求超时（后端无响应）' : '后端未连接';
    throw new ApiError(0, msg, null);
  }
  const ct = res.headers.get('content-type') || '';
  let data = null;
  if (ct.includes('application/json')) data = await res.json();
  else data = await res.text();
  if (!res.ok) {
    const detail = typeof data === 'string' && data ? data : (data && data.error) || '';
    throw new ApiError(res.status, `HTTP ${res.status}${detail ? ' - ' + detail : ''}`, data);
  }
  return data;
}

const apiGet = (p) => api('GET', p);
const apiPost = (p, b, t) => api('POST', p, b, t);
const apiDelete = (p) => api('DELETE', p);
