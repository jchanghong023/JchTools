#!/usr/bin/env python3
"""clippy 预算棘轮：存量告警受预算约束，新增告警即失败。

设计动机（对应 AGENTS.md 3.2 的对抗自证偏差纪律）：直接 `-D warnings` 会被
历史存量卡死；完全无门禁则告警无限累积。本脚本以「带位置告警计数 <= 预算」
为门禁，与 cargo-mutants 的存活预算同一模式：

- 新增告警 → 计数超预算 → 非零退出（CI 红）；
- 清理存量后应下调调用处的 --budget（脚本每次输出当前计数，按输出值收紧）；
- 编译错误或 deny 级 lint 直接按 cargo 退出码失败，与预算无关。

用法：
  python scripts/clippy_budget.py --budget N -- [cargo clippy 参数...]
示例：
  python scripts/clippy_budget.py --budget 315 -- --all-targets

计数口径（只数 stderr 渲染流；stdout 机器流不计）：
- 形如 `path:line:col: warning: ...` 的行数；路径可能为相对（src\\a.rs:1:2:）
  或绝对（含盘符），按整段无空白路径匹配。
- 排除 target 目录下构建脚本生成代码（Slint 的 out\\app.rs 等）：机器生成、
  修复无意义且计数随 UI 改版大幅波动；这类行也不回显，只输出汇总条数。
- 排除构建脚本通知（cargo:warning，无位置前缀）与汇总行。
- 同一告警在多个编译目标（lib / lib test / 集成测试）重复出现会重复计数，
  口径前后一致即可比较。
"""
from __future__ import annotations

import argparse
import re
import subprocess
import sys

_LOCATED_WARNING = re.compile(r'^(\S+):\d+:\d+: warning: ')
# 生成代码路径特征：位于任意 target 构建目录的 out/ 下（分隔符兼容 / 与 \）。
_GENERATED_PATH = re.compile(r'[\\/]target[\\/].*[\\/]out[\\/]')


def main() -> int:
    parser = argparse.ArgumentParser(description='clippy 预算棘轮检查')
    parser.add_argument('--budget', type=int, required=True, help='允许的带位置告警数量上限')
    parser.add_argument('clippy_args', nargs=argparse.REMAINDER, help='透传给 cargo clippy 的参数')
    args = parser.parse_args()
    rest = args.clippy_args[1:] if args.clippy_args and args.clippy_args[0] == '--' else args.clippy_args
    cmd = ['cargo', 'clippy', '--message-format=short', *rest]
    print(f'$ {" ".join(cmd)}', flush=True)
    proc = subprocess.run(cmd, capture_output=True, text=True, encoding='utf-8', errors='replace')
    if proc.stdout:
        sys.stdout.write(proc.stdout)
    count = 0
    generated = 0
    kept = []
    for line in proc.stderr.splitlines():
        match = _LOCATED_WARNING.match(line)
        if not match:
            sys.stderr.write(line + '\n')
            continue
        if '@0.1.0:' in line or _GENERATED_PATH.search(match.group(1)):
            generated += 1
            continue
        count += 1
        kept.append(line)
    for line in kept:
        sys.stderr.write(line + '\n')
    if proc.returncode != 0:
        return proc.returncode
    if generated:
        print(f'（另排除 target 生成代码告警 {generated} 行，不计入预算）')
    print(f'clippy 带位置告警计数：{count}（预算 {args.budget}）')
    if count > args.budget:
        print(
            f'::error::clippy 告警 {count} 个超出预算 {args.budget}：存在新增告警，'
            '请修复后重跑；存量清零后应下调预算。本地复现：运行与 CI 相同的 clippy_budget.py 命令。',
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
