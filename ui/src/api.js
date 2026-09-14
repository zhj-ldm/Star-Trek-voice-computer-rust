// 后端 API 轻封装（渲染进程直连本地 core，CORS 已由后端放行）
// 不依赖 preload：window.star 注入成功则用之，否则回退到默认本地端口，
// 保证即使 preload 未执行，前端 UI 也能正常工作。
'use strict';

const CORE = (window.star && window.star.coreUrl) || 'http://127.0.0.1:8410';

async function api(method, path, body) {
  const opts = { method, headers: {} };
  if (body !== undefined) {
    opts.headers['Content-Type'] = 'application/json';
    opts.body = JSON.stringify(body);
  }
  const res = await fetch(CORE + path, opts);
  const ct = res.headers.get('content-type') || '';
  let data = null;
  if (ct.includes('application/json')) data = await res.json();
  else data = await res.text();
  if (!res.ok) throw new Error(`HTTP ${res.status}: ${data}`);
  return data;
}

const apiGet = (p) => api('GET', p);
const apiPost = (p, b) => api('POST', p, b);
const apiDelete = (p) => api('DELETE', p);
