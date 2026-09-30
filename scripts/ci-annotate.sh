#!/usr/bin/env bash
# RFB CI 注解发射器：把失败日志拆成公开可读的 ::error:: 注解。
# Actions 日志下载需要 token，而 check-runs annotations 是公开 API —— 所有
# 工作流的失败现场都按本脚本的格式带出。
#
# 用法: bash scripts/ci-annotate.sh <title> <file> [max_bytes]
#   max_bytes：默认 2000（取文件尾部）；0 表示不裁剪（输入已自行限长）。
#   文件不存在时发出 (missing log) 注解并以 0 退出（诊断路径不掩盖原错误）。
set -euo pipefail
title="$1"
file="$2"
max="${3:-2000}"
if [ ! -f "$file" ]; then
  echo "::error title=$title::(missing log: $file)"
  exit 0
fi
if [ "$max" = "0" ]; then
  data=$(cat "$file")
else
  data=$(tail -c "$max" "$file")
fi
printf '%s' "$data" \
  | tr '\r\n' '::' \
  | LC_ALL=C tr -d '%\000-\010\013\014\016-\037' \
  | LC_ALL=C fold -w 700 \
  | awk -v t="$title" '{print "::error title="t"-"NR"::"$0}'
