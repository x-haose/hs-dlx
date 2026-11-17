"""hs-dlx 的 yt-dlp 驱动脚本。

由应用以 `python -I -X utf8 driver.py <命令> ...` 运行，环境变量 YTDLP_NO_PLUGINS 不加载插件。命令：

- `requirements`：标准输入为 yt-dlp 某个版本的 Requires-Dist 列表（JSON），标准输出为它锁定的依赖（`pin` 与
  `pin-curl-cffi` 两组，按本机的解释器与平台取舍，不要 mutagen）：`[{"name": 名字, "version": 版本}]`。
- `pick`：标准输入为 `{包名: [{"filename": 文件名, ...}]}`（PyPI 上这个版本的各个文件），标准输出为每个包本机能装
  的、最合适的那个 wheel 的文件名：`{包名: 文件名}`；有包没有能装的时退出码非 0。
- `selftest <site> <quickjs>`：导入 site 目录里的 yt-dlp 与 yt-dlp-ejs、核对它认得 QuickJS-NG，标准输出打出
  `{"yt_dlp": 规范化的版本}`。
- `serve <site> <quickjs> <cache>`：常驻解析。标准输入每行一个请求，标准输出每行一个回应，一次只处理一个：
  - 请求 `{"id": 整数, "url": 网址, "proxy": 代理地址（空串为直连）, "cookies": Netscape 格式的 Cookie 或 null,
    "generic": 只有通用解析器认得时也解析}`；`generic` 为假时只认得通用解析器的网址不发请求，直接回 `unsupported`；
    视频没有格式时照网站说的原因（要登录、人机验证、限流、地区限制、不可用）回错误
  - 回应 `{"id": 同请求, "ok": 结果}` 或 `{"id": 同请求, "error": {"kind": 种类, "message": 说明}}`，种类见
    `classify`。yt-dlp 自己的输出都转到标准错误，标准输出只有回应（纯 ASCII 的 JSON）。
  - 结果见 `trimmed`。
- `download <site> <quickjs> <cache>`：用 yt-dlp 自己的下载器下一个格式（要模拟浏览器 TLS 指纹的、DASH 分片等应用的引擎
  下不了的）。标准输入的第一行为请求 `{"url": 网页地址, "format": 要的格式（`key_of` 的各项）,
  "dir": 输出所在的目录, "name": 输出的文件名, "proxy", "cookies"}`；标准输出先是若干行进度
  `{"progress": {"bytes": 已下字节, "total": 总字节或估算，不知道时为 null}}`，最后一行为 `{"ok": {}}` 或
  `{"error": ...}`（同 `serve`，格式编号找不到或属性对不上时种类为 `changed`）。中断后再下同一输出时接着 `.part` 续传；
  缺了的片段不跳过，报错。读到标准输入的结尾（应用退出了）即退出。
"""

from __future__ import annotations

import importlib
import io
import json
import math
import os
import sys
import threading
import time
import traceback
from typing import TYPE_CHECKING, Any, Protocol, TextIO, cast

if TYPE_CHECKING:
    from collections.abc import Mapping
    from types import ModuleType

# yt-dlp 的解析结果：任意结构的 JSON 式字典
Info = dict[str, Any]

# 网络请求的等待上限（秒）：一次解析要发多个请求，整次的时限由应用掐
SOCKET_TIMEOUT = 20
# 请求太频繁的 HTTP 状态
TOO_MANY_REQUESTS = 429
# 播放列表最多列多少条：大的频道一页页翻完要很久
PLAYLIST_LIMIT = 200
# 意外异常的调用栈留多少字符
TRACEBACK_TAIL = 2000
# yt-dlp 白名单里它内部用的扩展名：不当作格式的扩展名（`part` 会与下载中的文件撞名）
INTERNAL_EXTENSIONS = frozenset(
    {"description", "json", "meta", "orig", "part", "temp", "uncut", "unknown_video", "ytdl"}
)
# 下载时 yt-dlp 不带原因报出的、算网络的失败：重试用尽，直链回了空内容（HTTP 下载器、分片下载器）
NO_DATA = ("Got error", "Did not get any data blocks", "The downloaded file is empty")
# 网站说资源没了的 HTTP 状态
GONE = frozenset({404, 410})
UNAUTHORIZED = 401
# 网站拦下了这个出口（风控）的 HTTP 状态
BLOCKED = frozenset({403, 412})
# 下载进度至多多久报一次（秒）
PROGRESS_INTERVAL = 0.2
# 用 yt-dlp 锁定版本的这两组依赖：默认的依赖与模拟浏览器 TLS 指纹的 curl-cffi
PIN_EXTRAS = ("pin", "pin-curl-cffi")
# 不装的：mutagen 只用于往文件里嵌封面（GPL）
EXCLUDED = frozenset({"mutagen"})


class CookieJar(Protocol):
    """yt-dlp 的 Cookie 罐里用到的部分。"""

    def get_cookie_header(self, url: str) -> str | None:
        """发往 `url` 的请求该带的 Cookie 请求头；没有时为 None。"""
        ...


def clean(value: str) -> str:
    """单独的代理项字符（截断在 emoji 中间的标题等）换成替换符：JSON 的读方不认它们。"""
    return value.encode("utf-16", "surrogatepass").decode("utf-16", "replace")


def text(value: object) -> str | None:
    """字符串（去掉单独的代理项），其余为 None。"""
    return clean(value) if isinstance(value, str) else None


def mapping(value: object) -> dict[str, object] | None:
    """字典（键转成字符串），其余为 None。"""
    if not isinstance(value, dict):
        return None
    return {str(key): item for key, item in cast("dict[object, object]", value).items()}


def items(value: object) -> list[object]:
    """列表或元组的各项，其余为空。"""
    if isinstance(value, (list, tuple)):
        return list(cast("list[object] | tuple[object, ...]", value))
    return []


def number(value: object) -> float | None:
    """有限的数（不含布尔）为浮点，其余（含 NaN、无穷）为 None。"""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    result = float(value)
    return result if math.isfinite(result) else None


def safe_ext(value: object) -> str | None:
    """格式的扩展名：只放行 yt-dlp 白名单里的，其余为 None。

    白名单是 yt-dlp 为 CVE-2024-38519 定的（媒体与图片的，这里去掉它内部用的）。扩展名来自网站的数据，会拼进文件名：
    不在白名单里的可能带路径分隔符或是可执行文件的扩展名。
    """
    ext = text(value)
    if ext is None or not ext.isascii() or not ext.isalnum():
        return None
    utils = importlib.import_module("yt_dlp.utils._utils")
    allowed: frozenset[str] = utils._UnsafeExtensionError.ALLOWED_EXTENSIONS
    return ext if ext.lower() in allowed and ext.lower() not in INTERNAL_EXTENSIONS else None


def format_of(fmt: Mapping[str, object], jar: CookieJar) -> Info:
    """一个格式里下载要用的。

    地址、协议、编码与画面、大小、请求头；按地址算出的 Cookie 请求头（与 yt-dlp 自己下载时带的一样）；每个请求的字节
    上限（`chunk_size`，YouTube 为 10 MiB）；可开始下载的时刻（`available_at`，Unix 秒）；可能有 DRM（`drm_maybe`：确定
    有的 yt-dlp 已滤掉，可能有的它会试着下）；要不要模拟浏览器的 TLS 指纹（`impersonate`）。
    """
    url = text(fmt.get("url")) or ""
    options = mapping(fmt.get("downloader_options")) or {}
    headers = mapping(fmt.get("http_headers")) or {}
    return {
        "format_id": text(fmt.get("format_id")) or "",
        "url": url,
        "protocol": text(fmt.get("protocol")) or "",
        "ext": safe_ext(fmt.get("ext")),
        "vcodec": text(fmt.get("vcodec")),
        "acodec": text(fmt.get("acodec")),
        "width": number(fmt.get("width")),
        "height": number(fmt.get("height")),
        "fps": number(fmt.get("fps")),
        "tbr": number(fmt.get("tbr")),
        "filesize": number(fmt.get("filesize")),
        "filesize_approx": number(fmt.get("filesize_approx")),
        "format_note": text(fmt.get("format_note")),
        "language": text(fmt.get("language")),
        "language_preference": number(fmt.get("language_preference")),
        "dynamic_range": text(fmt.get("dynamic_range")),
        "http_headers": {clean(key): clean(str(value)) for key, value in headers.items()},
        "cookie": text(jar.get_cookie_header(url)) if url else None,
        "chunk_size": number(options.get("http_chunk_size")),
        "available_at": number(fmt.get("available_at")),
        "drm_maybe": fmt.get("has_drm") == "maybe",
        "impersonate": bool(fmt.get("impersonate")),
    }


def entry_of(entry: Mapping[str, object]) -> Info:
    """播放列表的一个条目。"""
    return {
        "url": text(entry.get("url")) or text(entry.get("webpage_url")),
        "id": text(entry.get("id")),
        "title": text(entry.get("title")),
        "duration": number(entry.get("duration")),
    }


def trimmed(info: Mapping[str, object], jar: CookieJar, warnings: list[str]) -> Info:
    """应用要用的部分。

    `extractor`（解析器）、`id`、`title`、`webpage_url`、`duration`（秒）、`thumbnail`、`live_status`、`uploader`、
    `formats`（见 `format_of`；只有一个格式、没有格式列表时为它本身）、`has_drm`（有格式因 DRM 被滤掉）、`entries`
    （播放列表或分成几段的视频的条目，不是时为 null）、`warnings`（yt-dlp 的警告）。
    """
    formats = [format_of(fmt, jar) for fmt in map(mapping, items(info.get("formats"))) if fmt is not None]
    if not formats and info.get("url"):
        formats = [format_of(info, jar)]
    entries = None
    if info.get("_type") in ("playlist", "multi_video"):
        entries = [entry_of(entry) for entry in map(mapping, items(info.get("entries"))) if entry is not None]
    return {
        "extractor": text(info.get("extractor_key")) or "",
        "id": text(info.get("id")) or "",
        "title": text(info.get("title")) or "",
        "webpage_url": text(info.get("webpage_url")),
        "duration": number(info.get("duration")),
        "thumbnail": text(info.get("thumbnail")),
        "live_status": text(info.get("live_status")),
        "uploader": text(info.get("uploader")),
        "formats": formats,
        "has_drm": bool(info.get("_has_drm")),
        "entries": entries,
        "warnings": [clean(warning) for warning in warnings],
    }


class Logger:
    """接 yt-dlp 的输出：警告收进 `warnings`，其余丢掉（失败由异常带出）。"""

    def __init__(self) -> None:
        """从空的警告开始。"""
        self.warnings: list[str] = []

    def debug(self, message: str) -> None:
        """调试与普通信息：不要。"""

    def info(self, message: str) -> None:
        """普通信息：不要。"""

    def warning(self, message: str) -> None:
        """警告：收下。"""
        self.warnings.append(message)

    def error(self, message: str) -> None:
        """错误：由异常带出，这里不要。"""


def lineage(error: BaseException) -> set[str]:
    """`error` 的类与各个父类的名字：yt-dlp 是运行时才从 site 目录导入的，按类名判断。"""
    return {cls.__name__ for cls in type(error).__mro__}


def unwrapped(error: BaseException) -> BaseException:
    """`error` 包着的原始异常。

    DownloadError 包着解析器或下载器抛的（在 `exc_info` 里）；写文件失败时 yt-dlp 抛 UnavailableVideoError，引起它的
    OSError 在 `__context__` 里。
    """
    original = error
    exc_info = items(getattr(error, "exc_info", None))
    if "DownloadError" in lineage(error) and len(exc_info) > 1 and isinstance(exc_info[1], BaseException):
        original = exc_info[1]
    if "UnavailableVideoError" in lineage(original) and isinstance(original.__context__, OSError):
        original = original.__context__
    return original


def root_of(original: BaseException) -> BaseException:
    """解析器的错误（ExtractorError）沿 `cause` 追到的、引起它的错误（如网络错误）；不是时为它本身。"""
    root = original
    while "ExtractorError" in lineage(root):
        cause: object = getattr(root, "cause", None)
        if not isinstance(cause, BaseException):
            break
        root = cause
    return root


def classify(error: BaseException, *, downloading: bool = False) -> tuple[str, str]:
    """yt-dlp 的失败归类：种类与说明。

    种类：`unsupported`（没有专门的解析器认得这个网址）、`geo`（地区限制）、`login`（要登录或 Cookie）、
    `rate_limited`（429）、`blocked`（网站拦下了这个出口）、`network`（连不上、超时、TLS；下载时服务器回的错误状态与
    没下完整）、`disk`（下载时写不了文件）、`unavailable`（网站说视频不可用：私密、删了、未开播等）、`other`（其余，
    多为网站改了、yt-dlp 要更新）。
    网站回的错误状态：429 为限流；解析时请求网页与接口回的 401 为要登录，403、412 为拦截（`blocked`：多为出口 IP 被
    风控，如 B 站对境外代理回 412；换直连或别的出口、登录后再试），404、410 为不可用；其余（5xx 等，与 `downloading` 时
    直链回的一切状态）算网络：多为一时的或直链过期，再试即重新解析、换新的直链。
    """
    original = unwrapped(error)
    message = clean(str(getattr(original, "orig_msg", None) or original))
    root = root_of(original)
    names, causes = lineage(original), lineage(root)
    status = getattr(root, "status", None) if "HTTPError" in causes else None
    kind = "other"
    if "UnsupportedError" in names:
        kind = "unsupported"
    elif "GeoRestrictedError" in names:
        kind = "geo"
    elif status == TOO_MANY_REQUESTS:
        kind = "rate_limited"
    elif status == UNAUTHORIZED and not downloading:
        kind = "login"
    elif status in BLOCKED and not downloading:
        kind = "blocked"
    elif status in GONE and not downloading:
        kind = "unavailable"
    elif status is not None or "TransportError" in causes or "ContentTooShortError" in names:
        kind = "network"
    elif isinstance(original, OSError):
        kind = "disk"
    elif "ExtractorError" in names and getattr(original, "expected", False) is True:
        kind = said(message)
    return kind, message


def said(message: str) -> str:
    """网站自己说的不能播的原因的种类：限流、要登录（含人机验证）、其余为不可用。"""
    lowered = message.lower()
    if any(word in lowered for word in ("rate-limited", "try again later", "too many requests")):
        return "rate_limited"
    login = ("sign in", "log in", "login", "cookies", "captcha", "not a bot", "members-only", "join this channel")
    if any(word in lowered for word in login):
        return "login"
    return "unavailable"


def params(request: Mapping[str, object], quickjs: str, cache: str, logger: Logger) -> Info:
    """一次解析或下载给 YoutubeDL 的参数，代理（空串为直连）与 Cookie 取自 `request`。

    只解析不下载；播放列表只列条目（至多 `PLAYLIST_LIMIT` 条），带视频的播放列表网址取视频；格式全要（应用自己从
    `formats` 里选）；视频没有格式时照网站说的原因抛出（yt-dlp 的默认，`classify` 按原因归类）；代理只照应用给的；
    Cookie 只在内存里；QuickJS-NG 解 YouTube 的 JavaScript 挑战；缓存放在应用的目录；不查自己是不是太旧。
    """
    cookies = text(request.get("cookies"))
    return {
        "quiet": True,
        "logger": logger,
        "noplaylist": True,
        "extract_flat": "in_playlist",
        "playlistend": PLAYLIST_LIMIT,
        "format": "all",
        "proxy": text(request.get("proxy")) or "",
        "socket_timeout": SOCKET_TIMEOUT,
        "cookiefile": io.StringIO(cookies) if cookies is not None else None,
        "js_runtimes": {"quickjs": {"path": quickjs}},
        "cachedir": cache,
        "warn_when_outdated": False,
    }


def matched(url: str) -> str:
    """认得 `url` 的解析器：同 yt-dlp 的挑法，按次序取第一个认得的启用了的解析器，通用解析器排在最后。"""
    extractors = importlib.import_module("yt_dlp.extractor")
    for extractor in extractors.gen_extractor_classes():
        if getattr(extractor, "_ENABLED", True) and extractor.suitable(url):
            return str(extractor.ie_key())
    return "Generic"


def extract(yt_dlp: ModuleType, request: Mapping[str, object], quickjs: str, cache: str) -> Info:
    """解析请求里的网址，返回回应（结果或归了类的失败）。"""
    url = text(request.get("url")) or ""
    if request.get("generic") is not True and matched(url) == "Generic":
        return {"error": {"kind": "unsupported", "message": "没有专门认这个网址的解析器"}}
    logger = Logger()
    options = params(request, quickjs, cache, logger)
    with yt_dlp.YoutubeDL(options) as ydl:
        try:
            info = ydl.extract_info(url, download=False)
        except yt_dlp.utils.YoutubeDLError as error:
            kind, message = classify(error)
            return {"error": {"kind": kind, "message": message}}
        sanitized = yt_dlp.YoutubeDL.sanitize_info(info)
        return {"ok": trimmed(sanitized, ydl.cookiejar, logger.warnings)}


def serve(site: str, quickjs: str, cache: str, requests: TextIO, replies: TextIO) -> None:
    """逐行读 `requests` 里的请求、解析，回应逐行写进 `replies`，读到结尾即返回。

    解析器抛出 yt-dlp 没包住的异常（网站改版后常见的 TypeError、AttributeError 等）时回 `other`，带异常与调用栈的
    末尾，接着处理下一个请求。
    """
    sys.path.insert(0, site)
    yt_dlp = importlib.import_module("yt_dlp")
    for line in requests:
        if not line.strip():
            continue
        request = json.loads(line)
        try:
            reply = extract(yt_dlp, request, quickjs, cache)
        except Exception as error:
            stack = traceback.format_exc()[-TRACEBACK_TAIL:]
            reply = {"error": {"kind": "other", "message": clean(f"{type(error).__name__}：{error}\n{stack}")}}
        reply["id"] = request.get("id")
        replies.write(json.dumps(reply, ensure_ascii=True, allow_nan=False) + "\n")
        replies.flush()


def requirements(requires_dist: list[object]) -> list[Info]:
    """yt-dlp 锁定的依赖：`PIN_EXTRAS` 各组里条件对本机成立的、版本写死（`==`）的，去掉 `EXCLUDED`，按名字去重。"""
    packaging = importlib.import_module("pip._vendor.packaging.requirements")
    utils = importlib.import_module("pip._vendor.packaging.utils")
    found: dict[str, str] = {}
    for line in requires_dist:
        if not isinstance(line, str):
            continue
        requirement = packaging.Requirement(line)
        marker = requirement.marker
        wanted = marker is not None and any(marker.evaluate({"extra": extra}) for extra in PIN_EXTRAS)
        name = str(utils.canonicalize_name(requirement.name))
        if not wanted or name in EXCLUDED:
            continue
        specifiers = list(requirement.specifier)
        if len(specifiers) != 1 or specifiers[0].operator != "==":
            sys.exit(f"{line} 的版本没写死")
        found[name] = str(specifiers[0].version)
    return [{"name": name, "version": version} for name, version in sorted(found.items())]


def pick(files: Mapping[str, object]) -> dict[str, str]:
    """每个包本机能装的最合适的 wheel：按本机支持的标签（pip 的次序）取最靠前的。"""
    tags = importlib.import_module("pip._vendor.packaging.tags")
    utils = importlib.import_module("pip._vendor.packaging.utils")
    order = {str(tag): rank for rank, tag in enumerate(tags.sys_tags())}
    chosen: dict[str, str] = {}
    for name, listed in files.items():
        best: tuple[int, str] | None = None
        for item in map(mapping, items(listed)):
            filename = text((item or {}).get("filename")) or ""
            if not filename.endswith(".whl"):
                continue
            ranks = [order[str(tag)] for tag in utils.parse_wheel_filename(filename)[3] if str(tag) in order]
            if ranks and (best is None or min(ranks) < best[0]):
                best = (min(ranks), filename)
        if best is None:
            sys.exit(f"{name} 没有本机能装的预编译包")
        chosen[name] = best[1]
    return chosen


def key_of(fmt: Mapping[str, object]) -> Info:
    """认出一个格式用的属性（与应用的 `FormatKey` 一一对应）：格式编号、扩展名、视频与音频编码、高度、语言、确切大小。

    格式编号可能是 yt-dlp 排序后加的序号（同名的格式加 `-0`、`-1`，如 YouTube 同一 itag 的多条配音），网站多给或少给一个
    格式就移位，只比编号会认错。对 yt-dlp 的原始格式与 `format_of` 的结果算出的相同。
    """
    return {
        "format_id": text(fmt.get("format_id")) or "",
        "ext": safe_ext(fmt.get("ext")),
        "vcodec": text(fmt.get("vcodec")),
        "acodec": text(fmt.get("acodec")),
        "height": number(fmt.get("height")),
        "language": text(fmt.get("language")),
        "filesize": number(fmt.get("filesize")),
    }


def same_format(fmt: Mapping[str, object], wanted: Mapping[str, object]) -> bool:
    """`fmt` 是不是要的那个：`key_of` 的各项都与 `wanted` 里的相同（`wanted` 缺的项按 null 比）。"""
    return all(wanted.get(key) == value for key, value in key_of(fmt).items())


def download(yt_dlp: ModuleType, request: Mapping[str, object], quickjs: str, cache: str, replies: TextIO) -> Info:
    """用 yt-dlp 自己的下载器把请求里要的那个格式下到当前目录下的 `name`，进度逐行写进 `replies`，返回最后的回应。

    照 yt-dlp 自己的流程解析、选格式、下载，选格式时只选按格式编号与属性认出的那个（不用格式选择式：`mp4`、`b` 之类的
    编号会被当成选择式），认不出时报 `changed`；选中之后 yt-dlp 报的、没带原因的失败（重试用尽）算网络。不合并、不做
    修复、HLS 与 DASH 用它自己的下载器（没有 ffmpeg 程序，HLS 写出的是各分片接起来的）；已有的 `.part` 接着下，已下完
    的不覆盖；缺了的片段不跳过。
    输出写成相对当前目录的名字：yt-dlp 会展开输出模板里的 `$` 与 `~`，且没有转义的写法，目录可以含它们（由调用方
    切换过去），名字（应用给的 `<路>.<扩展名>`）不含它们与路径分隔符，含了是应用的缺陷，报错不下。
    """
    logger = Logger()
    wanted = mapping(request.get("format")) or {}
    name = text(request.get("name")) or ""
    if not name or any(char in name for char in "$~/\\") or name in {".", ".."}:
        return {"error": {"kind": "other", "message": f"输出的名字不能用：{name!r}"}}
    last = [0.0]

    def hook(status: object) -> None:
        state = mapping(status) or {}
        now = time.monotonic()
        if state.get("status") != "downloading" or now - last[0] < PROGRESS_INTERVAL:
            return
        last[0] = now
        total = number(state.get("total_bytes")) or number(state.get("total_bytes_estimate"))
        line = {"progress": {"bytes": number(state.get("downloaded_bytes")), "total": total}}
        replies.write(json.dumps(line, ensure_ascii=True, allow_nan=False) + "\n")
        replies.flush()

    options = params(request, quickjs, cache, logger)
    options.update(
        {
            "extract_flat": False,
            # 输出模板里百分号是占位符的开头，原样的写两个
            "outtmpl": {"default": name.replace("%", "%%")},
            "progress_hooks": [hook],
            "continuedl": True,
            "overwrites": False,
            "fixup": "never",
            "skip_unavailable_fragments": False,
            # HLS 与 DASH 用 yt-dlp 自己的下载器：没有 ffmpeg 程序（直播另走 ffmpeg，应用不交直播过来）
            "external_downloader": {"default": "native"},
        }
    )
    # 选格式时有没有认出要的：认出之后的失败出在下载
    picked: list[bool] = []

    def select(ctx: Mapping[str, object]) -> list[object]:
        chosen = [fmt for fmt in items(ctx.get("formats")) if same_format(mapping(fmt) or {}, wanted)][:1]
        picked.append(bool(chosen))
        return chosen

    options["format"] = select
    with yt_dlp.YoutubeDL(options) as ydl:
        try:
            ydl.extract_info(text(request.get("url")) or "", download=True)
        except yt_dlp.utils.YoutubeDLError as error:
            if picked == [False]:
                return {"error": {"kind": "changed", "message": f"网站给的格式里没有要的 {wanted.get('format_id')}"}}
            kind, message = classify(error, downloading=picked == [True])
            # 重试用尽（「Got error: …」）与直链回了空内容时 yt-dlp 在 except 之外报错，DownloadError 不带原因
            if (
                kind == "other"
                and picked == [True]
                and unwrapped(error) is error
                and any(marker in message for marker in NO_DATA)
            ):
                kind = "network"
            return {"error": {"kind": kind, "message": message}}
    skipped = [warning for warning in logger.warnings if "Skipping fragment" in warning]
    if skipped:
        return {"error": {"kind": "network", "message": clean("；".join(skipped))}}
    return {"ok": {}}


def watch_stdin() -> None:
    """读到标准输入的结尾（应用退出了、管道断了）即退出整个进程：下载不留成没人管的进程。"""

    def wait() -> None:
        sys.stdin.read()
        os._exit(1)

    threading.Thread(target=wait, daemon=True).start()


def selftest(site: str, quickjs: str) -> None:
    """导入 yt-dlp 与 yt-dlp-ejs，核对扩展名白名单还在、yt-dlp 认得 `quickjs`，报出 yt-dlp 的版本。

    yt-dlp-ejs 解 YouTube 的 JavaScript 挑战；`quickjs` 是随应用带的 QuickJS-NG，yt-dlp 认不出或判为不支持时 YouTube
    只少格式、不报错。版本取自安装元数据，按 PEP 440 规范化，与 PyPI 上的写法一致；`yt_dlp.version.__version__` 的写法
    不同（补零、不带 `.dev0`）。
    """
    # site 目录要先加进 sys.path 才导入得到，所以按名字导入
    sys.path.insert(0, site)
    importlib.import_module("yt_dlp")
    importlib.import_module("yt_dlp_ejs")
    # 白名单是 yt-dlp 的内部名字：新版改掉了就装不上，而不是装上后每次解析都出错
    if safe_ext("mp4") != "mp4" or safe_ext("exe") is not None:
        message = "yt-dlp 的扩展名白名单认不出"
        raise RuntimeError(message)
    yt_dlp = importlib.import_module("yt_dlp")
    runtimes = yt_dlp.YoutubeDL({"quiet": True, "js_runtimes": {"quickjs": {"path": quickjs}}})._js_runtimes
    runtime = runtimes.get("quickjs")
    info = runtime.info if runtime is not None else None
    if info is None or not info.supported:
        message = f"yt-dlp 认不出随应用带的 QuickJS-NG（{quickjs}）"
        raise RuntimeError(message)
    metadata = importlib.import_module("importlib.metadata")
    version = importlib.import_module("pip._vendor.packaging.version")
    found = metadata.distribution("yt-dlp").version
    json.dump({"yt_dlp": str(version.Version(found))}, sys.stdout)


def main() -> None:
    """按命令行的命令运行。"""
    match sys.argv[1:]:
        case ["requirements"]:
            json.dump(requirements(items(json.load(sys.stdin))), sys.stdout)
        case ["pick"]:
            json.dump(pick(mapping(json.load(sys.stdin)) or {}), sys.stdout)
        case ["selftest", site, quickjs]:
            selftest(site, quickjs)
        case ["download", site, quickjs, cache]:
            replies = sys.stdout
            sys.stdout = sys.stderr
            sys.path.insert(0, site)
            # 只读一行：应用写完请求不关管道，之后读到结尾即应用不在了
            request = mapping(json.loads(sys.stdin.readline())) or {}
            watch_stdin()
            os.chdir(text(request.get("dir")) or ".")
            try:
                reply = download(importlib.import_module("yt_dlp"), request, quickjs, cache, replies)
            except Exception as error:
                stack = traceback.format_exc()[-TRACEBACK_TAIL:]
                reply = {"error": {"kind": "other", "message": clean(f"{type(error).__name__}：{error}\n{stack}")}}
            replies.write(json.dumps(reply, ensure_ascii=True, allow_nan=False) + "\n")
            replies.flush()
        case ["serve", site, quickjs, cache]:
            # 回应独占标准输出：yt-dlp 与依赖往标准输出写的都转到标准错误
            replies = sys.stdout
            sys.stdout = sys.stderr
            serve(site, quickjs, cache, sys.stdin, replies)
        case _:
            sys.exit(
                "用法：driver.py requirements | pick | selftest <site> | serve|download <site> <quickjs> <cache>，"
                f"收到 {sys.argv[1:]}"
            )


if __name__ == "__main__":
    main()
