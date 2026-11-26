"""给 macOS 的安装包签名、公证并钉上票据，再核对；设了 HS_DLX_SIGN_IDENTITY 才做，没设时什么也不做（本机开发用的包）。

- `hook <app>`：cargo-packager 每打一种格式之前调（`before-each-package-command`）。打 dmg 之前给已打好的 `<app>`
  签名（hardened runtime、带时间戳，由里到外：先签包里的每个 Mach-O，再签整个 .app）、公证并钉上票据，dmg 装的即是它；
  这次打包的格式里没有 app 时失败（cargo-packager 会在打 dmg 时重建 .app，签的就不是装进去的那个）。别的格式、别的
  平台什么也不做。
- `dmg <目录>`：cargo-packager 打完之后调（Makefile 的 package 与 CI 的打包都调）。给 `<目录>` 里最新的 hs-dlx_*.dmg
  签名、公证并钉上票据，再核对里面的 .app 与 dmg 本身。

HS_DLX_SIGN_IDENTITY 为 codesign 认的签名身份（钥匙串里证书的名字或其中一段，如 `Developer ID Application`），没设时
什么也不做（本机开发用的包，没签名）。设了时 HS_DLX_NOTARY_PROFILE 必须也设：为 `xcrun notarytool store-credentials`
存进钥匙串的凭据的名字；为 `none` 时只签名不公证（本机试用：包只能在签名的这台机器与认得这个证书的机器上直接打开），
不是发版的包。任何一步失败、超时即以非 0 退出。
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import NoReturn

# 一条命令的时限（秒）：签名、核对、挂载都在几秒内
STEP_TIMEOUT = 300
# 等公证的时限：苹果的队列通常几分钟，积压时可到一两个小时
NOTARY_WAIT = "2h"
NOTARY_TIMEOUT = 2 * 3600 + STEP_TIMEOUT
# HS_DLX_NOTARY_PROFILE 取这个值时只签名不公证
NO_NOTARY = "none"

# Mach-O 与通用二进制（fat）文件开头的四个字节，大小端两种写法
MACHO_MAGICS = {
    bytes.fromhex(magic) for magic in ("feedface", "feedfacf", "cefaedfe", "cffaedfe", "cafebabe", "bebafeca")
}


def fail(message: str) -> NoReturn:
    """报错退出。"""
    sys.stderr.write(f"macos_sign: {message}\n")
    sys.exit(1)


def run(*args: str, timeout: int = STEP_TIMEOUT) -> subprocess.CompletedProcess[str]:
    """跑 `args`（`timeout` 秒为限）；退出码不为 0 或超时时带上它的输出报错退出。"""
    try:
        done = subprocess.run(args, capture_output=True, text=True, check=False, timeout=timeout)
    except subprocess.TimeoutExpired:
        fail(f"{' '.join(args)} 超过 {timeout} 秒没完成")
    if done.returncode != 0:
        fail(f"{' '.join(args)} 失败（{done.returncode}）：{done.stderr.strip() or done.stdout.strip()}")
    return done


def said(*args: str) -> str:
    """跑 `args`，返回它说的（标准输出与标准错误：codesign、spctl 的结果写在标准错误上）。"""
    done = run(*args)
    return done.stdout + done.stderr


def sign(path: Path, identity: str, *, runtime: bool) -> None:
    """用 `identity` 签 `path`，带安全时间戳；`runtime` 时开 hardened runtime（公证要求可执行代码都开）。"""
    args = ["codesign", "--force", "--timestamp", "--sign", identity]
    if runtime:
        args += ["--options", "runtime"]
    run(*args, str(path))


def machos(app: Path) -> list[Path]:
    """`app` 里的 Mach-O 文件，深的在前（由里到外签）。"""
    found: list[Path] = []
    for path in app.rglob("*"):
        if path.is_file() and not path.is_symlink():
            with path.open("rb") as file:
                if file.read(4) in MACHO_MAGICS:
                    found.append(path)
    return sorted(found, key=lambda path: len(path.parts), reverse=True)


def notarize(path: Path, profile: str) -> None:
    """把 `path`（.app 或 .dmg）交给苹果公证、等结果，通过了即钉上票据；没通过时带上苹果的日志报错退出。"""
    with tempfile.TemporaryDirectory() as scratch:
        submitted = path
        if path.suffix == ".app":
            # .app 是目录，要先压成 zip；ditto 保留扩展属性与符号链接，同 Finder 的压缩
            submitted = Path(scratch) / f"{path.stem}.zip"
            run("ditto", "-c", "-k", "--keepParent", str(path), str(submitted))
        reply = run(
            "xcrun",
            "notarytool",
            "submit",
            str(submitted),
            "--keychain-profile",
            profile,
            "--wait",
            "--timeout",
            NOTARY_WAIT,
            "--output-format",
            "json",
            timeout=NOTARY_TIMEOUT,
        ).stdout
    try:
        result = json.loads(reply)
    except json.JSONDecodeError:
        fail(f"notarytool 的回应不是 JSON：{reply}")
    status = result.get("status")
    if status != "Accepted":
        # 等到时限还没结论时 status 为 In Progress，苹果那边照样在做
        log = said("xcrun", "notarytool", "log", str(result.get("id")), "--keychain-profile", profile)
        fail(f"{path.name} 没通过公证（{status}）：{log}")
    run("xcrun", "stapler", "staple", str(path))


def check_app(app: Path, *, notarized: bool) -> None:
    """核对 `app`：签名完整、开了 hardened runtime；`notarized` 时另核对钉了票据、Gatekeeper 认它是公证过的。"""
    run("codesign", "--verify", "--strict", "--deep", str(app))
    shown = said("codesign", "--display", "--verbose=2", str(app))
    if "(runtime)" not in shown:
        fail(f"{app.name} 没开 hardened runtime：{shown}")
    if notarized:
        run("xcrun", "stapler", "validate", str(app))
        assessed = said("spctl", "--assess", "--type", "execute", "--verbose=2", str(app))
        if "Notarized Developer ID" not in assessed:
            fail(f"Gatekeeper 不认 {app.name} 是公证过的：{assessed}")


def hook(app: Path, identity: str, profile: str | None) -> None:
    """见模块说明的 `hook`。"""
    if os.environ.get("CARGO_PACKAGER_FORMAT") != "dmg" or sys.platform != "darwin":
        return
    if "app" not in os.environ.get("CARGO_PACKAGER_FORMATS", "").split(","):
        fail("打 dmg 时格式里要有 app（--formats app,dmg）：不然 dmg 装的是打 dmg 时重建的、没签名的 .app")
    if not app.is_dir():
        fail(f"没有要签的 {app}：dmg 之前应已打好 .app")
    for path in machos(app):
        sign(path, identity, runtime=True)
    sign(app, identity, runtime=True)
    if profile:
        notarize(app, profile)
    check_app(app, notarized=bool(profile))


def dmg(out: Path, identity: str, profile: str | None) -> None:
    """见模块说明的 `dmg`。"""
    dmgs = sorted(out.glob("hs-dlx_*.dmg"), key=lambda path: path.stat().st_mtime)
    if not dmgs:
        fail(f"{out} 里没有 hs-dlx_*.dmg")
    image = dmgs[-1]
    sign(image, identity, runtime=False)
    if profile:
        notarize(image, profile)
        run("xcrun", "stapler", "validate", str(image))
        assessed = said(
            "spctl", "--assess", "--type", "open", "--context", "context:primary-signature", "--verbose=2", str(image)
        )
        if "Notarized Developer ID" not in assessed:
            fail(f"Gatekeeper 不认 {image.name} 是公证过的：{assessed}")
    with tempfile.TemporaryDirectory() as mount:
        run("hdiutil", "attach", "-nobrowse", "-readonly", "-mountpoint", mount, str(image))
        try:
            check_app(Path(mount) / "hs-dlx.app", notarized=bool(profile))
        finally:
            run("hdiutil", "detach", mount)
    sys.stdout.write(f"macos_sign: {image.name} 已签名{'、公证' if profile else '（没公证）'}\n")


def main() -> None:
    """按命令行做 `hook` 或 `dmg`。"""
    parser = argparse.ArgumentParser(description="给 macOS 的安装包签名、公证并核对")
    parser.add_argument("step", choices=["hook", "dmg"])
    parser.add_argument("target", type=Path, help="hook 时为 .app，dmg 时为放 dmg 的目录")
    args = parser.parse_args()
    identity = os.environ.get("HS_DLX_SIGN_IDENTITY")
    if not identity:
        return
    profile = os.environ.get("HS_DLX_NOTARY_PROFILE")
    if not profile:
        fail(f"设了 HS_DLX_SIGN_IDENTITY 就要设 HS_DLX_NOTARY_PROFILE（只签名不公证时设为 {NO_NOTARY}）")
    if profile == NO_NOTARY:
        profile = None
    if args.step == "hook":
        hook(args.target, identity, profile)
    else:
        dmg(args.target, identity, profile)


if __name__ == "__main__":
    main()
