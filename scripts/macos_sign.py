"""给 macOS 的安装包签名、公证并钉上票据，再核对；设了 HS_DLX_SIGN_IDENTITY 才做，没设时什么也不做（本机开发用的包）。

- `hook <app>`：cargo-packager 每打一种格式之前调（`before-each-package-command`）。打 dmg 之前给已打好的 `<app>`
  签名（hardened runtime、带时间戳，由里到外：先签包里的每个 Mach-O，再签整个 .app），设了 HS_DLX_NOTARY_PROFILE 时
  公证并钉上票据，dmg 装的即是它；别的格式、别的平台什么也不做。
- `dmg <目录>`：cargo-packager 打完之后调。给 `<目录>` 里最新的 hs-dlx_*.dmg 签名，设了 HS_DLX_NOTARY_PROFILE 时公证并
  钉上票据，再核对里面的 .app 与 dmg 本身。

HS_DLX_SIGN_IDENTITY 为 codesign 认的签名身份（钥匙串里证书的名字或其中一段，如 `Developer ID Application`）；
HS_DLX_NOTARY_PROFILE 为 `xcrun notarytool store-credentials` 存进钥匙串的凭据的名字。只签名不公证的包只能在签名的
这台机器与认得这个证书的机器上直接打开。任何一步失败即以非 0 退出。
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import NoReturn

# Mach-O 与通用二进制（fat）文件开头的四个字节，大小端两种写法
MACHO_MAGICS = {
    bytes.fromhex(magic) for magic in ("feedface", "feedfacf", "cefaedfe", "cffaedfe", "cafebabe", "bebafeca")
}


def fail(message: str) -> NoReturn:
    """报错退出。"""
    sys.stderr.write(f"macos_sign: {message}\n")
    sys.exit(1)


def run(*args: str) -> str:
    """跑 `args`，返回标准输出；退出码不为 0 时带上它的输出报错退出。"""
    done = subprocess.run(args, capture_output=True, text=True, check=False)
    if done.returncode != 0:
        fail(f"{' '.join(args)} 失败（{done.returncode}）：{done.stderr.strip() or done.stdout.strip()}")
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
            "--output-format",
            "json",
        )
    result = json.loads(reply[reply.index("{") :])
    if result.get("status") != "Accepted":
        log = run("xcrun", "notarytool", "log", result.get("id", ""), "--keychain-profile", profile)
        fail(f"{path.name} 没通过公证（{result.get('status')}）：{log}")
    run("xcrun", "stapler", "staple", str(path))


def check_app(app: Path, *, notarized: bool) -> None:
    """核对 `app`：签名完整、开了 hardened runtime；`notarized` 时另核对钉了票据、Gatekeeper 认它是公证过的。"""
    run("codesign", "--verify", "--strict", "--deep", str(app))
    shown = run("codesign", "--display", "--verbose=2", str(app))
    if "(runtime)" not in shown:
        fail(f"{app.name} 没开 hardened runtime：{shown}")
    if notarized:
        run("xcrun", "stapler", "validate", str(app))
        assessed = run("spctl", "--assess", "--type", "execute", "--verbose=2", str(app))
        if "Notarized Developer ID" not in assessed:
            fail(f"Gatekeeper 不认 {app.name} 是公证过的：{assessed}")


def hook(app: Path, identity: str, profile: str | None) -> None:
    """见模块说明的 `hook`。"""
    if os.environ.get("CARGO_PACKAGER_FORMAT") != "dmg" or sys.platform != "darwin":
        return
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
        assessed = run(
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
    profile = os.environ.get("HS_DLX_NOTARY_PROFILE") or None
    if args.step == "hook":
        hook(args.target, identity, profile)
    else:
        dmg(args.target, identity, profile)


if __name__ == "__main__":
    main()
