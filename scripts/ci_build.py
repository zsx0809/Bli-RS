#!/usr/bin/env python3
# ==============================================================================
# CI 编译遥控器 —— 让 GitHub Actions 干活，本地手机只负责递代码和收结果
#
# 为什么要有这个脚本：
#   Bli-RS 依赖 Slint + Skia + ring + resvg，本地 aarch64 Termux 上
#   `cargo build` 要几十分钟、CPU 满载、手机发烫降频，而且 Skia 预编译包
#   的体积和链接开销在移动端纯属自虐。
#   GitHub 的 ubuntu runner 有官方 NDK、有缓存、几分钟出包 —— 用它。
#
# 用法：
#   python3 scripts/ci_build.py                 # 推送当前分支并等待结果
#   python3 scripts/ci_build.py --no-push       # 只等当前最新 workflow
#   python3 scripts/ci_build.py --logs          # 只拉最近一次失败日志
#   python3 scripts/ci_build.py --apk           # 只下载最近一次成功的 APK
#   python3 scripts/ci_build.py -m "提交说明"    # 指定 commit message
#
# 输出：
#   dist/ci/Bli-RS-arm64-v8a.apk      构建成功时下载到这里
#   dist/ci/build-<run_id>.log        失败时完整日志
#
# 设计取舍：
#   · 只用标准库 + 系统 curl，不引入 PyGithub 之类的依赖。
#   · 失败时把日志 tail 直接打屏，不用再去开浏览器 —— 这就是"和本地编译
#     一样能快速定位问题"的那部分。
#   · token 从 git remote 里读，不硬编码、不落盘。
# ==============================================================================

import argparse
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DIST = REPO_ROOT / "dist" / "ci"
API = "https://api.github.com"


def sh(cmd, cwd=REPO_ROOT, check=True):
    r = subprocess.run(cmd, cwd=cwd, shell=isinstance(cmd, str),
                       capture_output=True, text=True)
    if check and r.returncode != 0:
        sys.exit(f"[!] 命令失败: {cmd}\n{r.stdout}\n{r.stderr}")
    return r.stdout.strip()


def parse_remote():
    """从 git remote 里抠出 owner/repo 和 token"""
    url = sh(["git", "remote", "get-url", "origin"])
    m = re.match(r"https://([^:]+):([^@]+)@github\.com/([^/]+)/([^/.]+)", url)
    if m:
        return m.group(3), m.group(4), m.group(2)
    m = re.match(r"https://github\.com/([^/]+)/([^/.]+)", url)
    if m:
        return m.group(1), m.group(2), os.environ.get("GITHUB_TOKEN", "")
    m = re.match(r"git@github\.com:([^/]+)/([^/.]+)", url)
    if m:
        return m.group(1), m.group(2), os.environ.get("GITHUB_TOKEN", "")
    sys.exit(f"[!] 无法解析 remote: {url}")


def api(path, token, method="GET", raw=False):
    req = urllib.request.Request(f"{API}{path}", method=method)
    req.add_header("Authorization", f"Bearer {token}")
    req.add_header("Accept", "application/vnd.github+json")
    req.add_header("X-GitHub-Api-Version", "2022-11-28")
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            data = r.read()
            return data if raw else json.loads(data)
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", "replace")[:500]
        sys.exit(f"[!] GitHub API {path} -> {e.code}\n{body}")


def latest_run(owner, repo, token):
    runs = api(f"/repos/{owner}/{repo}/actions/runs?per_page=5", token)
    if not runs.get("workflow_runs"):
        return None
    return runs["workflow_runs"][0]


def watch(owner, repo, token, run_id, timeout=1800):
    """轮询 workflow 状态，返回 (conclusion, run)"""
    start = time.time()
    last = None
    while time.time() - start < timeout:
        run = api(f"/repos/{owner}/{repo}/actions/runs/{run_id}", token)
        status, concl = run["status"], run.get("conclusion")
        if status != last:
            print(f"    [{int(time.time()-start):>3}s] {status}"
                  + (f" / {concl}" if concl else ""))
            last = status
        if status == "completed":
            return concl, run
        time.sleep(10)
    sys.exit("[!] 等待超时")


def fetch_logs(owner, repo, token, run_id):
    """拉取失败日志并打屏 tail"""
    jobs = api(f"/repos/{owner}/{repo}/actions/runs/{run_id}/jobs", token)
    log_all = []
    for job in jobs.get("jobs", []):
        if job.get("conclusion") not in ("failure", "cancelled"):
            continue
        try:
            raw = api(f"/repos/{owner}/{repo}/actions/jobs/{job['id']}/logs",
                      token, raw=True)
            text = raw.decode("utf-8", "replace")
        except SystemExit:
            continue
        log_all.append(f"===== JOB: {job['name']} ({job['conclusion']}) =====")
        log_all.append(text)

    if not log_all:
        return None

    full = "\n".join(log_all)
    DIST.mkdir(parents=True, exist_ok=True)
    out = DIST / f"build-{run_id}.log"
    out.write_text(full, encoding="utf-8")

    # 只打关键片段，避免刷屏
    print(f"\n===== 失败日志（完整版: {out}）=====")
    lines = full.splitlines()
    # 优先展示 error / error[ 附近
    hits = [i for i, l in enumerate(lines)
            if re.search(r"^error(\[|:)|ld\.lld: error|FAILED:|panicked at", l)]
    if hits:
        shown = set()
        for i in hits[:6]:
            for j in range(max(0, i - 2), min(len(lines), i + 8)):
                if j not in shown:
                    shown.add(j)
                    print(lines[j])
            print("  ...")
    else:
        print("\n".join(lines[-40:]))
    return out


def download_apk(owner, repo, token, run_id):
    arts = api(f"/repos/{owner}/{repo}/actions/runs/{run_id}/artifacts", token)
    if not arts.get("artifacts"):
        print("[!] 这次 run 没有 artifact")
        return None
    art = arts["artifacts"][0]
    print(f"    下载 artifact: {art['name']} ({art['size_in_bytes']//1024} KB)")

    DIST.mkdir(parents=True, exist_ok=True)
    zip_path = DIST / f"{art['name']}.zip"
    req = urllib.request.Request(art["archive_download_url"])
    req.add_header("Authorization", f"Bearer {token}")
    with urllib.request.urlopen(req, timeout=300) as r:
        zip_path.write_bytes(r.read())

    apk_path = DIST / "Bli-RS-arm64-v8a.apk"
    with zipfile.ZipFile(zip_path) as z:
        names = [n for n in z.namelist() if n.endswith(".apk")]
        if not names:
            print(f"[!] zip 里没有 apk: {z.namelist()}")
            return None
        with z.open(names[0]) as src, open(apk_path, "wb") as dst:
            dst.write(src.read())
    zip_path.unlink()
    return apk_path


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--no-push", action="store_true", help="不推送，只等最新一次")
    ap.add_argument("--logs", action="store_true", help="只拉最近一次失败日志")
    ap.add_argument("--apk", action="store_true", help="只下载最近一次成功 APK")
    ap.add_argument("-m", "--message", default=None, help="commit message")
    ap.add_argument("--branch", default=None)
    args = ap.parse_args()

    owner, repo, token = parse_remote()
    if not token:
        sys.exit("[!] remote 里没有 token，且未设置 GITHUB_TOKEN")
    branch = args.branch or sh(["git", "rev-parse", "--abbrev-ref", "HEAD"])
    print(f"[*] 仓库 {owner}/{repo}  分支 {branch}")

    if args.logs or args.apk:
        run = latest_run(owner, repo, token)
        if not run:
            sys.exit("[!] 没有任何 workflow run")
        if args.logs:
            fetch_logs(owner, repo, token, run["id"])
        if args.apk:
            p = download_apk(owner, repo, token, run["id"])
            if p:
                print(f"[+] APK: {p}")
        return

    if not args.no_push:
        dirty = sh(["git", "status", "--porcelain"])
        if dirty:
            msg = args.message or "ci: 触发构建"
            sh(["git", "add", "-A"])
            sh(["git", "commit", "-m", msg])
            print(f"[*] 已提交: {msg}")
        else:
            print("[*] 工作区干净，跳过提交")
        print("[*] 推送中…")
        sh(["git", "push", "origin", branch])
        print("[+] 已推送，等 CI 起来…")
        time.sleep(12)

    run = None
    for _ in range(10):
        run = latest_run(owner, repo, token)
        if run and run["head_branch"] == branch:
            break
        time.sleep(6)

    if not run:
        sys.exit("[!] 没等到 workflow run")

    print(f"[*] Run #{run['run_number']}  id={run['id']}")
    print(f"    {run['html_url']}")

    concl, run = watch(owner, repo, token, run["id"])

    if concl == "success":
        print(f"\n[+] 构建成功 ({int(run.get('updated_at') is not None)} )")
        p = download_apk(owner, repo, token, run["id"])
        if p:
            print(f"[+] APK: {p}")
            print(f"    安装: su -c 'pm install -r {p}'")
    else:
        print(f"\n[-] 构建失败: {concl}")
        fetch_logs(owner, repo, token, run["id"])
        sys.exit(1)


if __name__ == "__main__":
    main()
