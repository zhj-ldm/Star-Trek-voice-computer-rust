// 预加载脚本：安全地向渲染进程暴露后端地址等只读信息。
// 注意：Electron 渲染进程默认开启 sandbox，preload 只能 require('electron')，
// 不能 require('fs')/child_process 等 Node API——此前这里用了 fs 写日志，
// 导致脚本在 sandbox 下抛错、整段不执行、window.star 未注入（前端随之瘫痪）。
// 即使本脚本执行失败，渲染进程也会回退到默认本地端口，UI 仍可用。
'use strict';

const { contextBridge } = require('electron');

contextBridge.exposeInMainWorld('star', {
  coreUrl: `http://127.0.0.1:${process.env.CORE_PORT || '8410'}`,
  voiceUrl: `http://127.0.0.1:${process.env.VOICE_PORT || '8420'}`,
});
