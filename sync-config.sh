#!/usr/bin/env bash
# 配置文件同步工具：运行实例（UI/App 实际使用） ↔ 项目仓库（源码/开发态）
# 用法：
#   ./sync-config.sh           查看两处配置差异
#   ./sync-config.sh --sync    把运行实例配置同步到项目仓库（备份原文件）
#   ./sync-config.sh --push    把项目仓库配置同步到运行实例（备份原文件，需 core 未运行或重启生效）
set -euo pipefail

RUN_CFG="$HOME/Library/Application Support/star-trek-assistant-ui/data/config.json"
PROJ_CFG="$(cd "$(dirname "$0")" && pwd)/data/config.json"

if [ ! -f "$RUN_CFG" ]; then
  echo "运行实例配置不存在: $RUN_CFG"
  echo "（App 尚未运行过，或路径已变化；项目配置即唯一配置）"
  exit 1
fi

diff_json() {
  python3 - "$1" "$2" <<'PY'
import json, sys
a = json.load(open(sys.argv[1]))
b = json.load(open(sys.argv[2]))
keys = sorted(set(a) | set(b))
diffs = [(k, a.get(k), b.get(k)) for k in keys if a.get(k) != b.get(k)]
if not diffs:
    print("两处配置完全一致")
else:
    for k, av, bv in diffs:
        print(f"  {k}:\n    运行实例: {av}\n    项目仓库: {bv}")
PY
}

case "${1:-}" in
  --sync)
    cp "$PROJ_CFG" "$PROJ_CFG.bak.$(date +%Y%m%d%H%M%S)"
    cp "$RUN_CFG" "$PROJ_CFG"
    echo "已同步：运行实例 -> 项目仓库 ($PROJ_CFG)"
    ;;
  --push)
    cp "$RUN_CFG" "$RUN_CFG.bak.$(date +%Y%m%d%H%M%S)"
    cp "$PROJ_CFG" "$RUN_CFG"
    echo "已推送：项目仓库 -> 运行实例（重启 core 后生效）"
    ;;
  *)
    echo "配置差异（运行实例 vs 项目仓库）："
    diff_json "$RUN_CFG" "$PROJ_CFG"
    echo
    echo "提示：./sync-config.sh --sync  /  --push 可双向同步"
    ;;
esac
