"""生成随桌面应用的安装包分发的声明（apps/desktop/LICENSES，打包时放进安装包）；`--check` 时只核对是否最新。

声明有 NOTICE.txt、THIRD-PARTY-RUST.txt 与随带的许可证全文的副本（本项目、FFmpeg、libtorrent、OpenSSL、Boost、字体、
gpui-webview、随带的 Python 与 QuickJS-NG 和它们链入的库）。libtorrent、OpenSSL、Boost 的许可证原文取自
third_party/libtorrent/build.sh 下载核对过的源码包，存在 third_party/libtorrent/licenses/（升级它们时一并换），
NOTICE.txt 里的版本取自 build.sh。随带的 Python 与 QuickJS-NG 和它们链入的库的许可证原文存在
third_party/runtime/licenses/（Python 的取自 python-build-standalone 完整包的 python/licenses；QuickJS-NG 的取自它的
仓库，它的发布构建静态链入的 mimalloc、musl（Linux 版）、mingw-w64 运行时（Windows 版）的取自各自的仓库；换版本时按
发布流程与可执行文件里的字符串一并核对），版本取自 scripts/runtime.py。

THIRD-PARTY-RUST.txt 收录桌面应用（hs-dlx）在各发布平台（deny.toml 的 `[graph] targets`）上依赖的 Rust crate：
按 `cargo tree` 解析特性后的正常依赖，过程宏与工作区成员不算。各 crate 根目录里的许可证文件原样收录，内容相同的只收
一份。收录的文件要对得上 crate 声明的许可证表达式：每个子句（可任选其一的几种许可证）至少有一种的原文在里面（按
LICENSE_MARKS 里各许可证条款的特征认）；包里的对不上时，加上 third_party/crate-licenses/<名字>-<版本>/ 下的再认，仍
对不上即失败。补的文件在 THIRD-PARTY-RUST.txt 里标出。NOTICE.txt 里 FFmpeg 的版本与源码包的 SHA-256 取自
编译它的 hs-m3u8 构建脚本（见 hs_m3u8.py），并列出只能按 MPL-2.0 取用的 crate 的源码出处。
"""

import argparse
import hashlib
import json
import re
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import NamedTuple, NoReturn

from hs_m3u8 import FfmpegScript, cargo, ffmpeg_script
from runtime import PYTHON_RELEASE, PYTHON_VERSION, QUICKJS_VERSION

ROOT = Path(__file__).resolve().parent.parent
SUPPLIED = ROOT / "third_party/crate-licenses"


class Product(NamedTuple):
    """一种发布包：根 crate 与声明目录。"""

    root: str
    out: Path


DESKTOP = Product("hs-dlx", ROOT / "apps/desktop/LICENSES")
# crate 根目录里算作许可证文件的名字（不区分大小写）
LICENSE_FILE = re.compile(r"^(licen[cs]e|copying|copyright|notice|unlicense)", re.IGNORECASE)
# SPDX 的许可证标识符（含 LicenseRef-…）
SPDX_ID = re.compile(r"^[A-Za-z0-9.+-]+$")
# `cargo tree --format {p}` 的一行：名字、版本，路径依赖另带路径，已列过的带 (*)
TREE_LINE = re.compile(r"^(\S+) v(\S+)(?: \([^)]*\))?(?: \(\*\))?$")
RULE = "-" * 80
# 许可证文件一行开头的注释记号
COMMENT_LEAD = re.compile(r"^\s*(?://+|#+|\*+)")


class Marks(NamedTuple):
    """一种许可证原文的特征：小写、空白合成一个空格后，`present` 里的句子都要出现，`absent` 里的都不能出现。"""

    present: tuple[str, ...]
    absent: tuple[str, ...] = ()


# 各许可证原文的特征，取自它的条款（不是标题，只给链接或名字的文件对不上）；相近的许可证靠 `absent` 分开：MIT 与
# MIT-0、ISC 与 0BSD、BSD-2-Clause 与 BSD-3-Clause、GPL 与 LGPL。依赖用到新的许可证时加上，并在
# tests/scripts/test_licenses.py 里加正例与反例
LICENSE_MARKS = {
    "0BSD": Marks(
        ("permission to use, copy, modify, and/or distribute this software for any purpose with or without fee",),
        ("provided that the above copyright notice",),
    ),
    "Apache-2.0": Marks(
        ("apache license", "version 2.0", "terms and conditions for use, reproduction, and distribution")
    ),
    "BSD-2-Clause": Marks(
        ("redistribution and use in source and binary forms", "redistributions of source code must retain"),
        ("neither the name",),
    ),
    "BSD-3-Clause": Marks(
        (
            "redistribution and use in source and binary forms",
            "redistributions of source code must retain",
            "neither the name",
        )
    ),
    "BSL-1.0": Marks(("boost software license", "to any person or organization obtaining a copy of the software")),
    "CC0-1.0": Marks(("cc0 1.0 universal", "statement of purpose")),
    "GPL-2.0-only": Marks(
        ("gnu general public license", "version 2, june 1991"),
        ("lesser general public license", "library general public license"),
    ),
    "ISC": Marks(
        (
            "permission to use, copy, modify, and/or distribute this software for any purpose with or without fee",
            "provided that the above copyright notice and this permission notice appear in all copies",
        )
    ),
    "LGPL-2.1-or-later": Marks(("gnu lesser general public license", "version 2.1, february 1999")),
    "MIT": Marks(
        (
            "permission is hereby granted, free of charge, to any person obtaining a copy of this software",
            "the above copyright notice and this permission notice shall be included",
        )
    ),
    "MIT-0": Marks(
        ("permission is hereby granted, free of charge, to any person obtaining a copy of this software",),
        ("the above copyright notice and this permission notice shall be included",),
    ),
    "MPL-2.0": Marks(("mozilla public license", "covered software", "larger work")),
    "Unicode-3.0": Marks(
        (
            "unicode license v3",
            "permission is hereby granted, free of charge, to any person obtaining a copy of data files",
        )
    ),
    "Unicode-DFS-2016": Marks(
        (
            "unicode, inc. license agreement - data files and software",
            "permission is hereby granted, free of charge, to any person obtaining a copy of the unicode data files",
        )
    ),
    "Unlicense": Marks(("this is free and unencumbered software released into the public domain",)),
    "WTFPL": Marks(("do what the fuck you want to public license", "you just do what the fuck you want to")),
    "Zlib": Marks(
        (
            "this software is provided 'as-is', without any express or implied warranty",
            "altered source versions must be plainly marked",
        )
    ),
}


@dataclass(frozen=True)
class Crate:
    """产物依赖的一个 crate。"""

    name: str
    version: str
    license: str
    repository: str
    directory: Path

    def label(self) -> str:
        """名字与版本。"""
        return f"{self.name} {self.version}"


def release_targets() -> list[str]:
    """deny.toml 里的发布平台；cargo-deny 允许写成目标三元组的字符串或带 `triple` 的表。"""
    config = tomllib.loads((ROOT / "deny.toml").read_text(encoding="utf-8"))
    entries: list[str | dict[str, str]] = config["graph"]["targets"]
    return [entry if isinstance(entry, str) else entry["triple"] for entry in entries]


def dependencies(root: str, target: str) -> set[tuple[str, str]]:
    """`target` 上 `root` 的依赖（含工作区成员）的名字与版本。"""
    output = cargo(
        "tree",
        "--locked",
        *("-p", root, "-e", "normal,no-proc-macro", "--target", target),
        *("--prefix", "none", "--format", "{p}"),
    )
    found: set[tuple[str, str]] = set()
    for line in output.splitlines():
        match = TREE_LINE.match(line)
        if match is None:
            message = f"认不出 cargo tree 的输出：{line!r}"
            raise SystemExit(message)
        found.add((match[1], match[2]))
    return found


def crates(names: set[tuple[str, str]]) -> list[Crate]:
    """`names` 里工作区之外的 crate，按名字与版本排序；有 crate 没写 `license`（只写了 `license-file`）时失败。"""
    metadata = json.loads(cargo("metadata", "--format-version", "1", "--locked"))
    members = set(metadata["workspace_members"])
    found: list[Crate] = []
    for package in metadata["packages"]:
        if (package["name"], package["version"]) not in names or package["id"] in members:
            continue
        if not package["license"]:
            message = f"{package['name']} {package['version']} 没写 license（SPDX 表达式），要人工核对它的许可证"
            raise SystemExit(message)
        found.append(
            Crate(
                name=package["name"],
                version=package["version"],
                license=package["license"],
                repository=package["repository"] or "（未注明）",
                directory=Path(package["manifest_path"]).parent,
            )
        )
    return sorted(found, key=lambda crate: (crate.name, crate.version))


def is_text_of(license_id: str, text: str) -> bool:
    """`text` 是不是 `license_id` 的原文。

    按 LICENSE_MARKS 认，`WITH` 例外不另认，行首的注释记号不算；没有这种许可证的特征时为 False。
    """
    marks = LICENSE_MARKS.get(license_id.split(" WITH ", maxsplit=1)[0])
    if marks is None:
        return False
    # 写成源码注释的许可证（每行以 //、#、* 起头）去掉行首的注释记号再比
    flat = " ".join(" ".join(COMMENT_LEAD.sub("", line) for line in text.lower().splitlines()).split())
    return all(phrase in flat for phrase in marks.present) and not any(phrase in flat for phrase in marks.absent)


def uncovered(crate: Crate, files: list[Path]) -> list[frozenset[str]]:
    """`crate` 的许可证表达式里，`files` 里没有任一种许可证原文的子句（见 `is_text_of`）。"""
    texts = [read_text(path) for path in files]
    return [
        clause
        for clause in Clauses(crate.license).result
        if not any(is_text_of(license_id, text) for license_id in clause for text in texts)
    ]


def license_files(crate: Crate) -> list[Path]:
    """`crate` 的许可证文件，包里的在前、各按名字排序。

    包里的对得上它声明的许可证（见 `uncovered`）时只取包里的，否则加上 third_party/crate-licenses 下的；仍对不上时失败。
    """

    def found(directory: Path) -> list[Path]:
        return sorted(path for path in directory.iterdir() if path.is_file() and LICENSE_FILE.match(path.name))

    files = found(crate.directory)
    if not uncovered(crate, files):
        return files
    supplied = SUPPLIED / f"{crate.name}-{crate.version}"
    if supplied.is_dir():
        files += found(supplied)
    missing = uncovered(crate, files)
    if not missing:
        return files
    unknown = sorted(
        {lid.split(" WITH ", maxsplit=1)[0] for clause in missing for lid in clause} - LICENSE_MARKS.keys()
    )
    names = "；".join(" 或 ".join(sorted(clause)) for clause in missing)
    hint = f"（LICENSE_MARKS 里还没有 {'、'.join(unknown)} 的特征）" if unknown else ""
    message = (
        f"{crate.label()} 的许可证文件里没有 {names} 的原文{hint}；从它发布时的提交（包里 .cargo_vcs_info.json "
        f"记的）取许可证文件放到 {supplied.relative_to(ROOT)}，源码里也没有时取 SPDX 的标准文本，出处写进那里的 README"
    )
    raise SystemExit(message)


def read_text(path: Path) -> str:
    """许可证文件的原文；不是 UTF-8 时失败，不替换字符。"""
    try:
        return path.read_bytes().decode("utf-8")
    except UnicodeDecodeError as error:
        message = f"{path} 不是 UTF-8 文本：{error}"
        raise SystemExit(message) from error


def third_party(subject: str, crates: list[Crate]) -> str:
    """THIRD-PARTY-RUST.txt 的内容；`subject` 为产物的叫法。"""
    lines = [
        f"{subject}在各发布平台上依赖的 Rust crate（合计），及其许可证文件（包里没有原文的由本项目补，已标出）。",
        "由 scripts/licenses.py 生成。",
        "",
        "== 清单 ==",
        "",
    ]
    lines += [f"{crate.label()}  {crate.license}  {crate.repository}" for crate in crates]
    # 内容相同的文件只收一份：按内容归组，组的顺序按第一个用到它的 crate
    groups: dict[str, tuple[str, list[str]]] = {}
    for crate in crates:
        for path in license_files(crate):
            text = read_text(path).replace("\r\n", "\n").rstrip()
            digest = hashlib.sha256(text.encode()).hexdigest()
            supplied = (
                f"（本项目补的，出处见源码的 {SUPPLIED.relative_to(ROOT)}/README.md）"
                if path.is_relative_to(SUPPLIED)
                else ""
            )
            groups.setdefault(digest, (text, []))[1].append(f"{crate.label()}：{path.name}{supplied}")
    lines += ["", "== 许可证文件 =="]
    for text, users in groups.values():
        lines += ["", RULE, *users, RULE, "", text]
    return "\n".join(lines) + "\n"


class Clauses:
    """许可证表达式化成合取范式的各子句，每个子句为可任选其一的许可证；旧写法里的 `/` 即 OR。

    只认 OR 都在 AND 之内的写法（如 `(A OR B) AND C`），按 SPDX 的优先级 AND 先于 OR；别的写法失败。
    """

    def __init__(self, expression: str) -> None:
        """解析 `expression`；认不出时失败。"""
        self.expression = expression
        self.words: list[str] = re.findall(r"\(|\)|[^\s()]+", expression.replace("/", " OR "))
        self.position = 0
        self.result = self.either()
        if self.position != len(self.words):
            self.fail()

    def fail(self) -> NoReturn:
        """认不出这个表达式。"""
        message = f"认不出的许可证表达式：{self.expression}"
        raise SystemExit(message)

    def peek(self) -> str | None:
        """下一个词；读完了为 None。"""
        return self.words[self.position] if self.position < len(self.words) else None

    def take(self) -> str:
        """取下一个词；读完了即失败。"""
        if self.position >= len(self.words):
            self.fail()
        self.position += 1
        return self.words[self.position - 1]

    def either(self) -> set[frozenset[str]]:
        """以 OR 连接的部分；两边都须是单个子句。"""
        left = self.both()
        while self.peek() == "OR":
            self.take()
            right = self.both()
            if len(left) != 1 or len(right) != 1:
                self.fail()
            left = {next(iter(left)) | next(iter(right))}
        return left

    def both(self) -> set[frozenset[str]]:
        """以 AND 连接的部分。"""
        left = self.single()
        while self.peek() == "AND":
            self.take()
            left |= self.single()
        return left

    def single(self) -> set[frozenset[str]]:
        """一个许可证（可带 WITH 例外），或括号里的表达式。"""
        word = self.take()
        if word == "(":
            inner = self.either()
            if self.take() != ")":
                self.fail()
            return inner
        if word in {")", "AND", "OR", "WITH"} or not SPDX_ID.match(word):
            self.fail()
        if self.peek() == "WITH":
            self.take()
            exception = self.take()
            if not SPDX_ID.match(exception):
                self.fail()
            word = f"{word} WITH {exception}"
        return {frozenset([word])}


def ffmpeg_release(script: FfmpegScript) -> tuple[str, str]:
    """静态链接的 FFmpeg 的版本与源码包的 SHA-256，取自编译它的构建脚本 `script`。"""
    text = script.path.read_text(encoding="utf-8")
    version = re.search(r"^VERSION=(\S+)$", text, re.MULTILINE)
    sha256 = re.search(r"^SHA256=(\S+)$", text, re.MULTILINE)
    if version is None or sha256 is None:
        message = f"{script.path} 里找不到 VERSION= 或 SHA256="
        raise SystemExit(message)
    return version[1], sha256[1]


LIBTORRENT_SCRIPT = ROOT / "third_party/libtorrent/build.sh"
LIBTORRENT_LICENSES = ROOT / "third_party/libtorrent/licenses"


def libtorrent_versions() -> dict[str, str]:
    """BT 引擎静态链接的 libtorrent、OpenSSL 与用到的 Boost 的版本，取自编译它们的构建脚本。"""
    text = LIBTORRENT_SCRIPT.read_text(encoding="utf-8")
    versions: dict[str, str] = {}
    for name in ("LIBTORRENT", "OPENSSL", "BOOST"):
        found = re.search(rf"^{name}=(\S+)$", text, re.MULTILINE)
        if found is None:
            message = f"{LIBTORRENT_SCRIPT} 里找不到 {name}="
            raise SystemExit(message)
        versions[name] = found[1]
    return versions


def mpl_crates(crates: list[Crate]) -> list[Crate]:
    """只能按 MPL-2.0 取用的 crate（许可证表达式里有一个子句只有 MPL-2.0）：分发时要告知它们的源码出处。"""
    return [crate for crate in crates if frozenset(["MPL-2.0"]) in Clauses(crate.license).result]


def ffmpeg_section() -> str:
    """NOTICE.txt 里 FFmpeg 的一段。"""
    script = ffmpeg_script()
    version, sha256 = ffmpeg_release(script)
    tarball = f"ffmpeg-{version}.tar.xz"
    return f"""\
1. FFmpeg {version}（https://ffmpeg.org），按 GNU Lesser General Public License（LGPL）2.1 或更高版本授权，全文见
   FFmpeg-LGPL-2.1.txt。FFmpeg 的源码未作修改，只编译了转封装用到的组件，configure 参数见 hs-m3u8 的构建脚本：
   {script.url()}
   对应的源码包 {tarball} 可从
   https://ffmpeg.org/releases/{tarball} 取得，SHA-256 为
   {sha256}。

   换用修改过的 FFmpeg：按 build.sh 里的 configure 参数编译修改过的 FFmpeg 源码并安装到某个目录，再在本项目源码目录
   里以 FFMPEG_DIR=<该目录> cargo build --release -p hs-dlx 重新构建。
"""


def desktop_notice(crates: list[Crate]) -> str:
    """桌面应用的 NOTICE.txt 的内容；`crates` 为它依赖的 crate。"""
    ffmpeg = ffmpeg_section()
    bt = libtorrent_versions()
    mpl = "".join(
        f"\n   - {crate.label()}：源码见 {crate.repository} 与 https://crates.io/crates/{crate.name}/{crate.version}"
        for crate in mpl_crates(crates)
    )
    mpl_part = f"\n   其中按 Mozilla Public License 2.0 授权、源码未作修改的：{mpl}" if mpl else ""
    return f"""hs-dlx 按 MIT 许可证发布，见 LICENSE.txt。由 scripts/licenses.py 生成。

hs-dlx 桌面应用静态链接或随带了以下软件：

{ffmpeg}
2. Rust crate，各自的许可证见 THIRD-PARTY-RUST.txt；Rust 标准库按 MIT 或 Apache-2.0 授权
   （https://github.com/rust-lang/rust）。{mpl_part}

3. 网页嵌入层（本项目源码的 crates/webview）有一部分改写自 gpui-webview 0.9.1（© Longbridge，
   https://github.com/longbridge/gpui-kit），按 Apache License 2.0 授权，全文见 gpui-webview-LICENSE-APACHE.txt。
   改写过的文件为 crates/webview/src/lib.rs 与 crates/webview/src/linux.rs：改为依赖官方 wry，只留不用窗口合成时
   用到的部分。

4. Geist 与 Geist Mono 字体 1.9.2（© The Geist Project Authors，https://github.com/vercel/geist-font），按 SIL Open
   Font License 1.1 授权，全文见 OFL.txt。

5. libtorrent {bt["LIBTORRENT"]}（https://libtorrent.org，© Arvid Norberg 等），按 BSD 3-Clause 授权；静态链接，源码
   未作修改。它的许可证与其中收录的别人的代码（puff、ed25519、SHA-1、SHA-256 等）的声明全文见
   libtorrent-LICENSE.txt。

6. OpenSSL {bt["OPENSSL"]}（https://www.openssl.org），按 Apache License 2.0 授权，全文见 OpenSSL-LICENSE.txt；
   libtorrent 用它连加密的 tracker 与 web seed、算哈希。静态链接，源码未作修改。

7. Boost {bt["BOOST"]}（https://www.boost.org），按 Boost Software License 1.0 授权，全文见 Boost-LICENSE_1_0.txt；
   libtorrent 用到它的头文件，未作修改。

8. Python {PYTHON_VERSION}（CPython，https://www.python.org，© Python Software Foundation），按 Python Software
   Foundation License Version 2 授权；取自 python-build-standalone {PYTHON_RELEASE}
   （https://github.com/astral-sh/python-build-standalone），去掉了 Tcl/Tk、头文件、测试、idle 等不用的部分，其余未作
   修改，网页视频用它运行 yt-dlp。它链入的 bzip2、OpenSSL、expat、libffi、liblzma（xz）、mpdecimal、SQLite、
   libuuid、zlib、zlib-ng（Linux 版另有 libedit、ncurses）的许可证全文与 CPython 的一起见 Python-LICENSES.txt；附带的
   pip 按 MIT 授权，许可证随它在包里。

9. QuickJS-NG {QUICKJS_VERSION}（https://github.com/quickjs-ng/quickjs，© Fabrice Bellard、Charlie Gordon、
   Ben Noordhuis 等），按 MIT 许可证授权；随带的可执行文件取自它的发布，未作修改，yt-dlp 用它解 YouTube 的
   JavaScript 挑战。它静态链入的 mimalloc（MIT）、musl（Linux 版，MIT）、mingw-w64 运行时（Windows 版）的许可证全文与
   QuickJS-NG 的一起见 QuickJS-NG-LICENSES.txt。
"""


RUNTIME_LICENSES = ROOT / "third_party/runtime/licenses"
# 随带的 Python 静态链入或随带的库（裁掉 Tcl/Tk 与 _dbm 之后，按 python-build-standalone 三个发布平台的 PYTHON.json
# 合计）与 third_party/runtime/licenses/ 里它的许可证文件（python-<名字>.txt）
PYTHON_LIBRARIES = (
    ("CPython", "cpython"),
    ("bzip2", "bzip2"),
    ("OpenSSL", "openssl-3"),
    ("expat", "expat"),
    ("libffi", "libffi"),
    ("liblzma（xz）", "liblzma"),
    ("mpdecimal", "mpdecimal"),
    ("SQLite", "sqlite"),
    ("libuuid", "libuuid"),
    ("zlib", "zlib"),
    ("zlib-ng", "zlib-ng"),
    ("libedit（Linux 版）", "libedit"),
    ("ncurses（Linux 版）", "ncurses"),
)


# 随带的 QuickJS-NG 与它的发布构建静态链入的库（三个发布平台合计）和 third_party/runtime/licenses/ 里的许可证文件
# （quickjs-<名字>.txt）
QUICKJS_LIBRARIES = (
    ("QuickJS-NG", "ng"),
    ("mimalloc", "mimalloc"),
    ("musl（Linux 版）", "musl"),
    ("mingw-w64 运行时（Windows 版）", "mingw-w64-runtime"),
)


def runtime_licenses(prefix: str, libraries: tuple[tuple[str, str], ...]) -> str:
    """`libraries`（标题与 third_party/runtime/licenses/<prefix>-<名字>.txt 的名字）的许可证全文依次接起来。"""
    return "\n".join(
        f"{RULE}\n{label}\n{RULE}\n\n{read_text(RUNTIME_LICENSES / f'{prefix}-{name}.txt').strip()}\n"
        for label, name in libraries
    )


def python_licenses() -> str:
    """Python-LICENSES.txt：CPython 与它链入的各个库的许可证全文。"""
    return (
        f"随 hs-dlx 带的 Python {PYTHON_VERSION} 与它链入的库的许可证全文。由 scripts/licenses.py 生成。\n\n"
        + runtime_licenses("python", PYTHON_LIBRARIES)
    )


def quickjs_licenses() -> str:
    """QuickJS-NG-LICENSES.txt：QuickJS-NG 与它静态链入的各个库的许可证全文。"""
    return (
        f"随 hs-dlx 带的 QuickJS-NG {QUICKJS_VERSION} 与它链入的库的许可证全文。由 scripts/licenses.py 生成。\n\n"
        + runtime_licenses("quickjs", QUICKJS_LIBRARIES)
    )


def found_crates(root: str) -> list[Crate]:
    """`root` 在各发布平台上依赖的 crate（合计）。"""
    names: set[tuple[str, str]] = set()
    for target in release_targets():
        names |= dependencies(root, target)
    return crates(names)


def main() -> int:
    """生成或核对声明；`--check` 时不是最新的列到 stderr 并返回 1。"""
    parser = argparse.ArgumentParser(description="生成或核对随桌面应用分发的声明")
    parser.add_argument("--check", action="store_true", help="只核对，不写文件")
    args = parser.parse_args()
    desktop = found_crates(DESKTOP.root)
    out = DESKTOP.out
    expected = {
        out / "NOTICE.txt": desktop_notice(desktop),
        out / "THIRD-PARTY-RUST.txt": third_party("hs-dlx 桌面应用", desktop),
        out / "LICENSE.txt": read_text(ROOT / "LICENSE"),
        out / "FFmpeg-LGPL-2.1.txt": read_text(ROOT / "third_party/ffmpeg/LGPL-2.1.txt"),
        out / "OFL.txt": read_text(ROOT / "apps/desktop/assets/fonts/OFL.txt"),
        out / "gpui-webview-LICENSE-APACHE.txt": read_text(ROOT / "crates/webview/LICENSE-APACHE"),
        out / "libtorrent-LICENSE.txt": read_text(LIBTORRENT_LICENSES / "libtorrent-LICENSE.txt"),
        out / "OpenSSL-LICENSE.txt": read_text(LIBTORRENT_LICENSES / "OpenSSL-LICENSE.txt"),
        out / "Boost-LICENSE_1_0.txt": read_text(LIBTORRENT_LICENSES / "Boost-LICENSE_1_0.txt"),
        out / "Python-LICENSES.txt": python_licenses(),
        out / "QuickJS-NG-LICENSES.txt": quickjs_licenses(),
    }
    stale: list[Path] = []
    for path, content in expected.items():
        # 按字节比对与写出，换行不做转换：副本与原文逐字节相同
        current = path.read_bytes().decode("utf-8") if path.exists() else None
        if current == content:
            continue
        if args.check:
            stale.append(path)
        else:
            path.parent.mkdir(exist_ok=True)
            path.write_bytes(content.encode("utf-8"))
    for path in stale:
        sys.stderr.write(f"{path.relative_to(ROOT)} 不是最新的，运行 make licenses 重新生成\n")
    return 1 if stale else 0


if __name__ == "__main__":
    sys.exit(main())
