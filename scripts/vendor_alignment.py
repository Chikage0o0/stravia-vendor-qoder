#!/usr/bin/env python3
"""Run Qoder plugin alignment through the shared harness of the pinned host release."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tomllib

repo = Path(__file__).resolve().parent.parent
HARNESS = 'tests/common/vendor_alignment.py'


def pinned_host() -> tuple[str, str]:
    """Cargo.toml 是宿主版本的唯一来源；harness 必须与插件编译所用依赖同一 tag。"""
    manifest = tomllib.loads((repo / 'Cargo.toml').read_text(encoding='utf-8'))
    pins = {(spec['git'], spec['tag'])
            for table in ('dependencies', 'dev-dependencies')
            for spec in manifest.get(table, {}).values()
            if isinstance(spec, dict) and 'git' in spec}
    if len(pins) != 1:
        sys.exit(f'Cargo.toml must pin all host dependencies to exactly one git tag, found: {sorted(pins)}')
    return pins.pop()


def host_checkout(url: str, tag: str) -> Path:
    """按需浅克隆固定 tag，只检出 harness；已存在则复用。"""
    checkout = repo / 'target' / f'host-{tag}'
    if (checkout / HARNESS).is_file():
        return checkout
    # 先克隆到临时目录再改名，避免中断后留下残缺的缓存目录。
    partial = checkout.with_name(checkout.name + '.partial')
    shutil.rmtree(partial, ignore_errors=True)
    subprocess.run(['git', '-c', 'advice.detachedHead=false', 'clone', '--quiet', '--depth', '1', '--branch', tag, '--filter=blob:none', '--sparse', url, str(partial)], check=True)
    subprocess.run(['git', '-C', str(partial), 'sparse-checkout', 'set', '--no-cone', '/' + HARNESS], check=True)
    shutil.rmtree(checkout, ignore_errors=True)
    partial.rename(checkout)
    return checkout


parser = argparse.ArgumentParser(description=__doc__, add_help=False)
parser.add_argument('--component', default=str(repo / 'target/wasm32-wasip2/release/stravia_vendor_qoder.wasm'))
args, forwarded = parser.parse_known_args()
harness = host_checkout(*pinned_host()) / HARNESS
command = [sys.executable, str(harness), '--vendor', 'qoder', '--component', args.component, *forwarded,
           '--response-code-map', str(repo / 'tests/fixtures/response-code-map.json')]
os.execv(sys.executable, command)
