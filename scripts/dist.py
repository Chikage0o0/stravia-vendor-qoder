#!/usr/bin/env python3
"""Build and sign the Qoder v2 marketplace distribution."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parent.parent
CATALOG = "stravia-plugin-catalog.json"
PUBLIC = "stravia-plugin-catalog.pub"
SIGNATURE = CATALOG + ".minisig"
LICENSES = ("LICENSE", "NOTICE", "rust_decimal.LICENSE")


def required(name):
    value = os.environ.get(name, "")
    if not value.strip():
        raise ValueError(f"必须设置 {name}")
    return value


def run(*args, **kwargs):
    subprocess.run(args, check=True, **kwargs)


def main():
    os.chdir(ROOT)
    tool = required("STRAVIA_TOOLS")
    host = required("HOST_VERSION_REQ")
    repository = required("REPOSITORY_ID")
    tag = required("RELEASE_TAG")
    timestamp = required("PUBLISHED_AT")
    key = Path(required("PLUGIN_SIGNING_KEY_FILE")).resolve(strict=True)
    public = Path(required("PLUGIN_PUBLIC_KEY_FILE")).resolve(strict=True)
    password = required("PLUGIN_SIGNING_PASSWORD")
    version = tomllib.loads(Path("Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
    if repository != "1403877632" or tag != "v" + version:
        raise ValueError("仓库 ID 或 Release tag 与本项目不匹配")
    component = Path("target/wasm32-wasip2/release/stravia_vendor_qoder.wasm").resolve(strict=True)
    tool_path = shutil.which(tool)
    if tool_path is None:
        raise ValueError(f"找不到 STRAVIA_TOOLS: {tool}")
    tool_path = str(Path(tool_path).resolve())
    with tempfile.TemporaryDirectory(prefix="qoder-dist-") as tmp:
        tmp = Path(tmp)
        declarations = tmp / "input.json"
        declarations.write_text(json.dumps([{"file": str(component), "vendor_id": "qoder", "version": version, "display_name": "Qoder"}]), encoding="utf-8")
        output = tmp / "output"
        run(tool_path, "plugin-catalog", "--repository-id", repository, "--release-tag", tag,
            "--host-version-req", host, "--published-at", timestamp, "--input", str(declarations), "--output", str(output))
        catalog = json.loads((output / CATALOG).read_bytes())
        if catalog["repository_id"] != int(repository) or catalog["release_tag"] != tag or len(catalog["releases"]) != 1:
            raise ValueError("目录身份不匹配")
        artifact = catalog["releases"][0]["artifact"]
        name = artifact["filename"]
        if name != f"stravia-vendor-qoder-v{version}.wasm.zst":
            raise ValueError("目录资产文件名不匹配")
        data = (output / name).read_bytes()
        if len(data) != artifact["size"] or hashlib.sha256(data).hexdigest() != artifact["sha256"]:
            raise ValueError("压缩资产摘要或大小不匹配")
        shutil.copyfile(public, output / PUBLIC)
        run("minisign", "-S", "-s", str(key), "-m", str(output / CATALOG), "-x", str(output / SIGNATURE), input=(password + "\n").encode())
        run("minisign", "-V", "-p", str(output / PUBLIC), "-m", str(output / CATALOG), "-x", str(output / SIGNATURE))
        for license_name in LICENSES:
            source = Path("vendor") / license_name if license_name == "rust_decimal.LICENSE" else Path(license_name)
            shutil.copyfile(source, output / license_name)
        dist = Path("dist")
        dist.mkdir(exist_ok=True)
        # 只清理旧打包入口生成的原始组件与校验清单，保留其他文件。
        for obsolete in dist.glob("stravia-vendor-qoder-v*.wasm"):
            obsolete.unlink()
        (dist / "SHA256SUMS").unlink(missing_ok=True)
        for filename in (name, CATALOG, PUBLIC, SIGNATURE, *LICENSES):
            shutil.copyfile(output / filename, dist / filename)
            print(f"dist/{filename}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as exc:
        print(f"发布打包失败: {exc}", file=sys.stderr)
        sys.exit(1)
