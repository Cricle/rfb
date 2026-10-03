#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
failed=0
# src/ 下的内联测试一律拒绝。正则命中 test 门控的 cfg 属性变体：
#   #[cfg(test)]、# [cfg(test)]、#[ cfg(test)]、#[cfg(all(test, unix))]、
#   #[cfg(any(test, …))]，以及 test 处于任意谓词位置的嵌套变体（如
#   #[cfg(all(unix, test))]、#[cfg(all(any(test, x), y))]）：
#   `cfg[[:space:]]*\(` 消费 cfg 自身的左括号，可选前导组 `[^]]*[(,[:space:]]`
#   允许 test 之前还有任意属性文本并以 `(`/`,`/空白 收尾（简单形式
#   `cfg(test)` 则整体跳过该组、test 直接跟在 cfg 的左括号之后），随后要求
#   词法边界完整的 `test`，后随空白/`,`/`)`。
#   #[cfg_attr(…)] 不算（`cfg` 之后是 `_` 而非左括号，非测试模块门控）；
#   `feature = "…test…"` 字符串值因引号不属分隔类也不命中；行内 `]` 之后的
#   注释不参与匹配，避免误伤。已知限制：跨行的 cfg 属性按行匹配，覆盖不到。
# 范围含 ben/（rfb-ben）crate 的 src；tests/ 目录是内联测试的合法去处，不扫描。
while IFS= read -r -d '' file; do
  printf 'embedded test module found: %s\n' "$file" >&2
  failed=1
done < <(
  find "$ROOT_DIR/rfb" "$ROOT_DIR/rfb-adk" "$ROOT_DIR/rfb-runtime" "$ROOT_DIR/ben" \
    -path '*/src/*' -name '*.rs' -print0 |
    xargs -0 grep -lZE '#[[:space:]]*\[[[:space:]]*cfg[[:space:]]*\(([^]]*[(,[:space:]])?test[[:space:],)]' 2>/dev/null || true
)

for crate in rfb rfb-adk rfb-runtime; do
  test -d "$ROOT_DIR/$crate/tests" || { printf 'missing tests directory: %s\n' "$crate" >&2; failed=1; }
done

if (( failed )); then
  exit 1
fi
printf 'tests-folder check passed\n'
