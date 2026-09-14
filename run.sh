#!/bin/bash
# Marvis 星际迷航语音助手一键启动
set -e
ROOT="$(cd "$(dirname "$0")" && pwd)"
export PATH="/usr/local/bin:$PATH"

cd "$ROOT"

echo "[run] 编译后端（增量，确保使用最新二进制）..."
cargo build

if [ ! -d ui/node_modules/electron ]; then
  echo "[run] 安装 UI 依赖 ..."
  cd ui && npm install && cd "$ROOT"
fi

echo "[run] 启动 Marvis UI（关闭窗口即终止后端与语音进程）..."
cd ui && npm start
