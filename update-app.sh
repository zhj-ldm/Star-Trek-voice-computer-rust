#!/bin/bash
# 一键更新桌面 Star Trek Computer.app
# 用法: ./update-app.sh           # 编译 release 并覆盖桌面 App（含重签）
#       ./update-app.sh --skip-build  # 跳过编译，仅用现有 target/release 产物覆盖
set -e
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
PROJ="$(cd "$(dirname "$0")" && pwd)"
APP="$HOME/Desktop/Star Trek Computer.app"
KC="/Users/zhj/Library/Application Support/com.tencent.mac.marvis/MarvisData/User/oAN1i2dt5zvZcj2tA1NaVA_sIhQg/workspace/conv_f168ca45e84d4280b3cbcc090c294a9e/temp/st-sign2.keychain-db"

if [ ! -d "$APP" ]; then
  echo "未找到桌面 App: $APP" >&2
  exit 1
fi

if [ "$1" != "--skip-build" ]; then
  echo "[1/4] cargo build --release (voice-serve + star-core) ..."
  cd "$PROJ"
  cargo build --release -p voice-serve -p star-core
else
  echo "[1/4] 跳过编译，使用现有 target/release 产物"
fi

echo "[2/4] 替换 bin -> $APP/Contents/Resources/bin/"
cp -f "$PROJ/target/release/star-trek-core" "$APP/Contents/Resources/bin/"
cp -f "$PROJ/target/release/voice-serve"    "$APP/Contents/Resources/bin/"

echo "[3/4] 重签 (Star Trek Dev Signing) ..."
security unlock-keychain -p "" "$KC"
codesign --force --deep --sign "Star Trek Dev Signing" --keychain "$KC" "$APP"

echo "[4/4] 验证签名 ..."
codesign -dv --verbose=2 "$APP" 2>&1 | grep -E "Identifier|Authority" | head -3

echo "完成。请完全退出 App（Cmd+Q）后重新打开桌面副本。"
