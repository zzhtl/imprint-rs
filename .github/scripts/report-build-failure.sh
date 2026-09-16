#!/usr/bin/env bash
# 把构建失败的原因送进 GitHub annotation。
#
# 仓库的 Actions 日志需要 admin 权限才能下载，annotation 却是公开可读的，
# 所以排查只能靠它。两个坑：
#   1. CARGO_TERM_COLOR=always 会让每行以 ANSI 转义序列开头，必须先剥掉；
#   2. `make -j8` 的输出高度交错，只取尾部往往抓不到真正的报错，
#      因此先按关键词捞，再补上尾部作为兜底。
set -uo pipefail
LOG="${1:?用法: report-build-failure.sh <日志路径>}"
PLAIN=$(mktemp)
sed -e 's/\x1b\[[0-9;]*m//g' "$LOG" > "$PLAIN"

echo "::group::错误关键词命中"
grep -nEi 'error|fatal|cannot find|no such file|not found|undefined reference|Assertion|panicked|未通过|失败' \
  "$PLAIN" | head -40 || echo "(无关键词命中)"
echo "::endgroup::"

echo "::group::日志尾部"
tail -60 "$PLAIN"
echo "::endgroup::"

# annotation 数量有限，只送最有信息量的若干行。
grep -nEi 'error|fatal|cannot find|no such file|undefined reference|panicked' "$PLAIN" \
  | head -12 | while IFS= read -r line; do
    echo "::error::$line"
  done
tail -8 "$PLAIN" | while IFS= read -r line; do
  echo "::error::[tail] $line"
done
