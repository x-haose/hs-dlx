"""随应用带的运行时：python-build-standalone 的 Python（install_only_stripped）与 QuickJS-NG。

下载并按 SHA-256 核对，裁掉 yt-dlp 用不上的部分（Tcl/Tk、头文件、测试、idle、只给嵌入用的 libpython 动态库，与链了
Berkeley DB、按 Sleepycat 许可证分发的 `_dbm`），在本机跑一遍确认能用；Python 打成 tar（条目按名字排序、时间与属主
归零，同样的输入得到同样的字节）经 zstd 压缩，与 QuickJS-NG 一起放进 third_party/runtime/dist/，桌面应用的
build.rs 把它们编进程序。

只备本机（rustc 的 host）这个目标：应用在各平台上各自构建。dist 里另有 bundle.env（目标、各项的 id、可执行文件与
Python 的 ABI，给 build.rs）与本脚本的副本 .hs-dlx-runtime（`--present` 与 build.rs 据此判断 dist 是不是这份脚本备的）。
裁剪的每一条都要在本目标上匹配到东西，匹配不到（换了版本、包的结构变了）即失败：许可证要求裁掉的（`_dbm`）不能
悄悄留下。下载放在
third_party/runtime/build/，核对过的留着复用。随带的许可证原文在 third_party/runtime/licenses/（换版本时一并核对）。
"""

import hashlib
import io
import shutil
import subprocess
import sys
import tarfile
import time
import urllib.request
from pathlib import Path
from typing import NamedTuple

import zstandard

ROOT = Path(__file__).resolve().parent.parent
BUILD = ROOT / "third_party/runtime/build"
DIST = ROOT / "third_party/runtime/dist"
MARKER = DIST / ".hs-dlx-runtime"

# python-build-standalone 的发布（2026-11-10）与其中的 CPython 版本
PYTHON_RELEASE = "20261110"
PYTHON_VERSION = "3.13.18"
# QuickJS-NG 的版本（2026-11-01）
QUICKJS_VERSION = "0.18.0"
# CPython 的大小版本，进标准库的路径与 ABI
PYTHON_MINOR = ".".join(PYTHON_VERSION.split(".")[:2])
# 压缩级别：再高（20 以上）要更大的解压窗口，省下的不到 1%
ZSTD_LEVEL = 19
# 下载失败时最多试几次（GitHub 的下载偶尔断）
ATTEMPTS = 5


class Target(NamedTuple):
    """一个发布平台：Python 与 QuickJS-NG 的包（文件名与 SHA-256）、解开后的可执行文件、要裁掉的。"""

    python_sha256: str
    # 解开后的解释器，相对于包的根，以 / 分隔
    python_executable: str
    # 标准库所在的目录，相对于包的根
    stdlib: str
    # 要裁掉的，相对于包的根；可带通配符（Path.glob 的写法）
    trimmed: tuple[str, ...]
    quickjs_asset: str
    quickjs_sha256: str
    # QuickJS-NG 在 dist 与组件目录里的文件名
    quickjs_file: str


# 各平台都裁掉的标准库部分（相对于标准库目录）：图形界面、开发工具、测试、交互式命令行
STDLIB_TRIMMED = (
    "tkinter",
    "idlelib",
    "ensurepip",
    "pydoc_data",
    "turtledemo",
    "turtle.py",
    "_pyrepl",
)
# macOS 与 Linux 都裁掉的：头文件、文档、pkg-config、Tcl/Tk、idle 与 pydoc 等脚本、只给嵌入用的 libpython（解释器是
# 静态链接的）、构建配置、Tcl/Tk 与 Berkeley DB 的扩展模块
UNIX_TRIMMED = (
    "python/include",
    "python/share",
    "python/lib/pkgconfig",
    "python/lib/libpython3*",
    "python/lib/libtcl*",
    "python/lib/tcl*",
    "python/lib/tk*",
    "python/lib/itcl*",
    "python/lib/thread*",
    "python/bin/idle*",
    "python/bin/pydoc*",
    "python/bin/pip*",
    "python/bin/*-config",
    f"python/lib/python{PYTHON_MINOR}/config-*",
    f"python/lib/python{PYTHON_MINOR}/lib-dynload/_tkinter*",
    f"python/lib/python{PYTHON_MINOR}/lib-dynload/_dbm*",
)

TARGETS = {
    "aarch64-apple-darwin": Target(
        python_sha256="7a2384641584ab9984221fcb0fc9265df465922af71be71410529e2a9c3f0540",
        python_executable="python/bin/python3",
        stdlib=f"python/lib/python{PYTHON_MINOR}",
        trimmed=UNIX_TRIMMED,
        quickjs_asset="qjs-darwin-arm64",
        quickjs_sha256="91fe89916d3b0a6c01c4dc8382a99c3c1fd91ecfc214aa81ca051902bcce590d",
        quickjs_file="qjs",
    ),
    "x86_64-unknown-linux-gnu": Target(
        python_sha256="f2d2199eb25880e07863b391cf7124e7ffa1338b0d65600dfcec117c4223a7fb",
        python_executable="python/bin/python3",
        stdlib=f"python/lib/python{PYTHON_MINOR}",
        trimmed=UNIX_TRIMMED,
        quickjs_asset="qjs-linux-x86_64",
        quickjs_sha256="85ded23b9fabe8c1644fc40a8d2bbdd71157affcbd807359c618cc717ee24d7d",
        quickjs_file="qjs",
    ),
    # python.exe 要 python313.dll；python3.dll 给按稳定 ABI 编译的扩展模块（curl_cffi 等）用，都留着
    "x86_64-pc-windows-msvc": Target(
        python_sha256="2e0fed09d48989f64c9305da9ff2d37a41c0d13ab2aa6283405b2b620dfbf805",
        python_executable="python/python.exe",
        stdlib="python/Lib",
        trimmed=(
            "python/include",
            "python/libs",
            "python/Scripts",
            "python/tcl",
            "python/pythonw.exe",
            "python/DLLs/_tkinter.pyd",
            "python/DLLs/tcl86t.dll",
            "python/DLLs/tk86t.dll",
            # 只有 _tkinter 链它（PYTHON.json）；zlib 模块编在 python313.dll 里
            "python/DLLs/zlib1.dll",
            "python/DLLs/_test*.pyd",
            "python/DLLs/_ctypes_test.pyd",
        ),
        quickjs_asset="qjs-windows-x86_64.exe",
        quickjs_sha256="ff03398d74ea6e6bd3a8e58058f7003225abcac229d7d57775b7aaae812a49c7",
        quickjs_file="qjs.exe",
    ),
}

# 裁过的 Python 要能做的：解析与下载用到的模块（TLS、压缩、数据库、ctypes 等）与挑 wheel 用的 pip；裁掉的 _dbm 与
# _tkinter 导入不了
SMOKE = (
    "import bz2, ctypes, decimal, hashlib, json, lzma, sqlite3, ssl, uuid, zlib, xml.parsers.expat;"
    "import pip._vendor.packaging.tags, importlib.util;"
    "left = [name for name in ('_dbm', '_tkinter') if importlib.util.find_spec(name)];"
    "print('left: ' + ', '.join(left) if left else 'ok')"
)


def host() -> str:
    """取 rustc 的 host 目标。"""
    output = subprocess.run(["rustc", "-vV"], stdout=subprocess.PIPE, encoding="utf-8", check=True).stdout
    for line in output.splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ")
    message = "rustc -vV 里没有 host"
    raise RuntimeError(message)


def sha256(data: bytes) -> str:
    """算出 `data` 的 SHA-256，小写十六进制。"""
    return hashlib.sha256(data).hexdigest()


def download(url: str) -> bytes:
    """取 `url` 的内容，断了重来，至多 `ATTEMPTS` 次；都没成时抛最后一次的错误。"""
    request = urllib.request.Request(url, headers={"User-Agent": "hs-dlx-build"})
    for attempt in range(1, ATTEMPTS):
        try:
            with urllib.request.urlopen(request, timeout=120) as response:
                return bytes(response.read())
        except OSError:
            time.sleep(attempt * 2)
    with urllib.request.urlopen(request, timeout=120) as response:
        return bytes(response.read())


def fetch(url: str, expected: str, path: Path) -> bytes:
    """下载 `url` 到 `path`，内容的 SHA-256 须为 `expected`；`path` 上已有且对得上时直接用。对不上时失败。"""
    if path.exists():
        data = path.read_bytes()
        if sha256(data) == expected:
            return data
    data = download(url)
    actual = sha256(data)
    if actual != expected:
        message = f"{url} 的 SHA-256 是 {actual}，锁定的是 {expected}"
        raise RuntimeError(message)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    return data


def trim(root: Path, target: Target) -> None:
    """在解开的 Python（`root` 下有 python/）里删掉 `target` 要裁掉的与各处的 __pycache__；哪一条都没匹配到时失败。"""
    patterns = [*target.trimmed, *(f"{target.stdlib}/{name}" for name in STDLIB_TRIMMED)]
    unmatched = [pattern for pattern in patterns if not any(root.glob(pattern))]
    if unmatched:
        message = f"裁剪的这几条在包里没有匹配到（版本或包的结构变了）：{', '.join(unmatched)}"
        raise RuntimeError(message)
    for pattern in patterns:
        for path in root.glob(pattern):
            if path.is_dir() and not path.is_symlink():
                shutil.rmtree(path)
            else:
                path.unlink()
    for cache in sorted(root.rglob("__pycache__"), reverse=True):
        shutil.rmtree(cache)


def pack(root: Path) -> bytes:
    """把 `root` 下的 python/ 打成 tar：条目按名字排序，时间、属主归零，保留权限与符号链接。"""
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for path in sorted([root / "python", *(root / "python").rglob("*")]):
            info = tar.gettarinfo(path, arcname=path.relative_to(root).as_posix())
            info.mtime = 0
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            if info.isfile():
                with path.open("rb") as file:
                    tar.addfile(info, file)
            else:
                tar.addfile(info)
    return buffer.getvalue()


def smoke(command: list[str], expected: str) -> None:
    """运行 `command`，标准输出须为 `expected`（去掉首尾空白）。"""
    output = subprocess.run(command, stdout=subprocess.PIPE, encoding="utf-8", check=True).stdout
    if output.strip() != expected:
        message = f"{command[0]} 输出 {output.strip()!r}，要的是 {expected!r}"
        raise RuntimeError(message)


def present(triple: str) -> bool:
    """`triple` 用得上的 dist 已备好：不是发布的平台（没有随带的运行时，不用备），或 dist 是这份脚本为它备的。"""
    if triple not in TARGETS:
        return True
    env = DIST / "bundle.env"
    return (
        MARKER.exists()
        and MARKER.read_bytes() == Path(__file__).read_bytes()
        and env.exists()
        and f"TARGET={triple}" in env.read_text(encoding="utf-8").splitlines()
    )


def main() -> None:
    """为本机这个目标备好 dist；`--present` 时只看是否已备好（见 `present`），没有时退出码为 1。"""
    triple = host()
    if sys.argv[1:] == ["--present"]:
        if not present(triple):
            sys.stderr.write(f"{DIST} is not prepared by this {Path(__file__).name} for {triple}: run make runtime\n")
            raise SystemExit(1)
        return
    target = TARGETS.get(triple)
    if target is None:
        message = f"{triple} 不是应用发布的平台，没有随应用带的运行时"
        raise SystemExit(message)
    python_name = f"cpython-{PYTHON_VERSION}+{PYTHON_RELEASE}-{triple}-install_only_stripped.tar.gz"
    python_url = (
        f"https://github.com/astral-sh/python-build-standalone/releases/download/{PYTHON_RELEASE}/{python_name}"
    )
    quickjs_url = f"https://github.com/quickjs-ng/quickjs/releases/download/v{QUICKJS_VERSION}/{target.quickjs_asset}"
    python_package = fetch(python_url, target.python_sha256, BUILD / python_name)
    quickjs = fetch(quickjs_url, target.quickjs_sha256, BUILD / target.quickjs_asset)

    unpacked = BUILD / "python"
    shutil.rmtree(unpacked, ignore_errors=True)
    unpacked.mkdir(parents=True)
    with tarfile.open(fileobj=io.BytesIO(python_package), mode="r:gz") as tar:
        tar.extractall(unpacked, filter="data")
    trim(unpacked, target)
    # -B：不写 .pyc，包里不带构建机的路径，产物与在哪个目录构建无关
    smoke([str(unpacked / target.python_executable), "-I", "-B", "-c", SMOKE], "ok")

    shutil.rmtree(DIST, ignore_errors=True)
    DIST.mkdir(parents=True)
    quickjs_path = DIST / target.quickjs_file
    quickjs_path.write_bytes(quickjs)
    quickjs_path.chmod(0o755)
    smoke([str(quickjs_path), "-e", "print(6 * 7)"], "42")
    python = zstandard.ZstdCompressor(level=ZSTD_LEVEL).compress(pack(unpacked))
    (DIST / "python.tar.zst").write_bytes(python)
    lines = [
        f"TARGET={triple}",
        f"PYTHON_ID={PYTHON_VERSION}-{PYTHON_RELEASE}-{sha256(python)[:12]}",
        f"PYTHON_EXECUTABLE={target.python_executable}",
        f"PYTHON_ABI=cp{PYTHON_MINOR.replace('.', '')}-{triple}",
        f"QUICKJS_ID={QUICKJS_VERSION}-{sha256(quickjs)[:12]}",
        f"QUICKJS_FILE={target.quickjs_file}",
    ]
    (DIST / "bundle.env").write_text("\n".join(lines) + "\n", encoding="utf-8")
    shutil.copyfile(Path(__file__), MARKER)
    # 只写 ASCII：Windows 上标准输出是管道时按 ANSI 代码页编码，-I 下 PYTHONUTF8 不起作用
    sys.stdout.write(f"{triple}: Python {len(python) / 1e6:.1f} MB, QuickJS-NG {len(quickjs) / 1e6:.1f} MB\n")


if __name__ == "__main__":
    main()
