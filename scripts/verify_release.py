#!/usr/bin/env python3
"""Verify every asset of a draft release against the signed local distribution."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile

from dist import CATALOG, LICENSES, PUBLIC, SIGNATURE


def run(*args):
    return subprocess.check_output(args, text=True)


def main():
    tag = os.environ["RELEASE_TAG"]
    release = json.loads(run("gh", "release", "view", tag, "--json", "isDraft,assets"))
    catalog = json.loads((Path("dist") / CATALOG).read_bytes())
    artifact = catalog["releases"][0]["artifact"]
    names = {CATALOG, SIGNATURE, PUBLIC, artifact["filename"], *LICENSES}
    actual = [asset["name"] for asset in release["assets"]]
    if not release["isDraft"] or len(actual) != len(names) or set(actual) != names:
        raise ValueError("draft 资产清单不匹配")
    with tempfile.TemporaryDirectory(prefix="qoder-release-") as tmp:
        run("gh", "release", "download", tag, "--dir", tmp)
        remote = Path(tmp)
        for name in names:
            if (remote / name).read_bytes() != (Path("dist") / name).read_bytes():
                raise ValueError(f"上传资产与本地文件不同: {name}")
        subprocess.run(["minisign", "-V", "-p", str(remote / PUBLIC), "-m", str(remote / CATALOG), "-x", str(remote / SIGNATURE)], check=True)
        data = (remote / artifact["filename"]).read_bytes()
        if len(data) != artifact["size"] or hashlib.sha256(data).hexdigest() != artifact["sha256"]:
            raise ValueError("上传资产摘要或大小不匹配")


if __name__ == "__main__":
    main()
