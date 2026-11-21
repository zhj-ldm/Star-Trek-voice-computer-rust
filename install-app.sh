#!/bin/bash
# 安装/升级到 /Applications。
# 关键约束：语音模型存放在用户目录，安装过程绝不搬动/嵌套 App 包内的模型，
# 避免重装后模型路径失效导致“语音监听不启动、TTS 不播报”。
set -euo pipefail
cd "$(dirname "$0")"

SRC="ui/dist/mac-arm64/Star Trek Computer.app"
DEST="/Applications/Star Trek Computer.app"
MODELS="$HOME/Library/Application Support/star-trek-assistant-ui/models"
KWS_CHECK="$MODELS/kws/encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx"

if [ ! -d "$SRC" ]; then
  echo "找不到 $SRC"
  echo "请先执行：cd ui && npx electron-builder --mac --arm64 --publish never"
  exit 1
fi

osascript -e 'quit app "Star Trek Computer"' 2>/dev/null || true
sleep 1
pkill -f "$DEST/Contents/MacOS" 2>/dev/null || true
pkill -f "star-trek-core --data-dir" 2>/dev/null || true
pkill -f "voice-serve" 2>/dev/null || true

# 仅当用户目录缺少模型时，从新包引导一次；已存在则完全不动。
if [ ! -f "$KWS_CHECK" ] && [ -d "$SRC/Contents/Resources/resources/models" ]; then
  mkdir -p "$MODELS"
  rsync -a "$SRC/Contents/Resources/resources/models/" "$MODELS/"
fi

rm -rf "$DEST"
cp -R "$SRC" /Applications/
codesign --force --deep --sign "Star Trek Dev Signing" "$DEST" 2>&1 | tail -1
xattr -dr com.apple.quarantine "$DEST" 2>/dev/null || true
open "$DEST"
echo "已安装并启动：$DEST"
echo "模型目录：$MODELS"
