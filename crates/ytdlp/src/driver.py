"""hs-dlx 的 yt-dlp 驱动脚本，由应用以 `python -I driver.py <命令> ...` 运行，环境变量 YTDLP_NO_PLUGINS 不加载插件。

- `selftest <site>`：导入 site 目录里的 yt-dlp 与 yt-dlp-ejs，标准输出打出 `{"yt_dlp": "<版本>"}`。
- `serve <site> <deno> <cache>`：常驻解析。标准输入每行一个请求，标准输出每行一个回应，一次只处理一个：
  - 请求 `{"id": 整数, "url": 网址, "proxy": 代理地址（空串为直连）, "cookies": Netscape 格式的 Cookie 或 null,
    "generic": 只有通用解析器认得时也解析}`；`generic` 为假时只认得通用解析器的网址不发请求，直接回 `unsupported`
  - 回应 `{"id": 同请求, "ok": 结果}` 或 `{"id": 同请求, "error": {"kind": 种类, "message": 说明}}`，种类见
    `classify`。yt-dlp 自己的输出都转到标准错误，标准输出只有回应。
  - 结果见 `trimmed`。
"""

from __future__ import annotations

import importlib
import io
import json
import sys
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


class CookieJar(Protocol):
    """yt-dlp 的 Cookie 罐里用到的部分。"""

    def get_cookie_header(self, url: str) -> str | None:
        """发往 `url` 的请求该带的 Cookie 请求头；没有时为 None。"""
        ...


def text(value: object) -> str | None:
    """字符串原样，其余为 None。"""
    return value if isinstance(value, str) else None


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
    """数（不含布尔）为浮点，其余为 None。"""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


def format_of(fmt: Mapping[str, object], jar: CookieJar) -> Info:
    """一个格式里下载要用的。

    地址、协议、编码与画面、大小、请求头；按地址算出的 Cookie 请求头（与 yt-dlp 自己下载时带的一样）；每个请求的字节
    上限（`chunk_size`，YouTube 为 10 MiB）；可开始下载的时刻（`available_at`，Unix 秒）；有没有 DRM、要不要模拟
    浏览器的 TLS 指纹（`impersonate`）。
    """
    url = text(fmt.get("url")) or ""
    options = mapping(fmt.get("downloader_options")) or {}
    headers = mapping(fmt.get("http_headers")) or {}
    return {
        "format_id": text(fmt.get("format_id")) or "",
        "url": url,
        "protocol": text(fmt.get("protocol")) or "",
        "ext": text(fmt.get("ext")),
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
        "dynamic_range": text(fmt.get("dynamic_range")),
        "http_headers": {key: str(value) for key, value in headers.items()},
        "cookie": jar.get_cookie_header(url) if url else None,
        "chunk_size": number(options.get("http_chunk_size")),
        "available_at": number(fmt.get("available_at")),
        "has_drm": bool(fmt.get("has_drm")),
        "impersonate": bool(fmt.get("impersonate")),
    }


def entry_of(entry: Mapping[str, object]) -> Info:
    """播放列表的一个条目（只列出、没解析）。"""
    return {
        "url": text(entry.get("url")) or text(entry.get("webpage_url")),
        "id": text(entry.get("id")),
        "title": text(entry.get("title")),
        "duration": number(entry.get("duration")),
    }


def trimmed(info: Mapping[str, object], jar: CookieJar, warnings: list[str]) -> Info:
    """应用要用的部分。

    `extractor`（解析器，通用解析器为 `Generic`）、`id`、`title`、`webpage_url`、`duration`（秒）、`thumbnail`、
    `live_status`、`uploader`、`formats`（见 `format_of`；只有一个格式、没有格式列表时为它本身）、`entries`（播放列表的
    条目，不是播放列表时为 null）、`warnings`（yt-dlp 的警告）。
    """
    formats = [format_of(fmt, jar) for fmt in map(mapping, items(info.get("formats"))) if fmt is not None]
    if not formats and info.get("url"):
        formats = [format_of(info, jar)]
    entries = None
    if info.get("_type") == "playlist":
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
        "entries": entries,
        "warnings": warnings,
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


def classify(error: BaseException) -> tuple[str, str]:
    """yt-dlp 的失败归类：种类与说明。

    种类：`unsupported`（没有专门的解析器认得这个网址）、`geo`（地区限制）、`login`（要登录或 Cookie）、
    `rate_limited`（429）、`network`（连不上、超时、TLS）、`unavailable`（网站说视频不可用：私密、删了、未开播等）、
    `other`（其余，多为网站改了、yt-dlp 要更新）。
    """
    # DownloadError 包着解析器抛的原始异常
    original = error
    exc_info = items(getattr(error, "exc_info", None))
    if "DownloadError" in lineage(error) and len(exc_info) > 1 and isinstance(exc_info[1], BaseException):
        original = exc_info[1]
    message = str(getattr(original, "orig_msg", None) or original)
    # ExtractorError 的 cause 是引起它的网络错误
    root = original
    while "ExtractorError" in lineage(root):
        cause: object = getattr(root, "cause", None)
        if not isinstance(cause, BaseException):
            break
        root = cause
    kind = "other"
    if "UnsupportedError" in lineage(original):
        kind = "unsupported"
    elif "GeoRestrictedError" in lineage(original):
        kind = "geo"
    elif "HTTPError" in lineage(root) and getattr(root, "status", None) == TOO_MANY_REQUESTS:
        kind = "rate_limited"
    elif "TransportError" in lineage(root):
        kind = "network"
    elif "ExtractorError" in lineage(original) and getattr(original, "expected", False) is True:
        lowered = message.lower()
        login = any(word in lowered for word in ("sign in", "log in", "login", "cookies"))
        kind = "login" if login else "unavailable"
    return kind, message


def params(proxy: str, cookies: str | None, deno: str, cache: str, logger: Logger) -> Info:
    """一次解析给 YoutubeDL 的参数。

    只解析不下载；播放列表只列条目，带视频的播放列表网址取视频；不另选格式（应用自己从 `formats` 里选，yt-dlp 找不到
    ffmpeg 程序时的默认选择不影响）；代理只照应用给的；Cookie 只在内存里；deno 解 YouTube 的 JavaScript 挑战；缓存
    （yt-dlp 存的播放器数据等）放在应用的目录；不查自己是不是太旧（应用另查）。
    """
    return {
        "quiet": True,
        "logger": logger,
        "noplaylist": True,
        "extract_flat": "in_playlist",
        "proxy": proxy,
        "socket_timeout": SOCKET_TIMEOUT,
        "cookiefile": io.StringIO(cookies) if cookies is not None else None,
        "js_runtimes": {"deno": {"path": deno}},
        "cachedir": cache,
        "warn_when_outdated": False,
    }


def matched(url: str) -> str:
    """认得 `url` 的解析器：同 yt-dlp 的挑法，按解析器的次序取第一个认得的，通用解析器排在最后。"""
    extractors = importlib.import_module("yt_dlp.extractor")
    for extractor in extractors.gen_extractor_classes():
        if extractor.suitable(url):
            return str(extractor.ie_key())
    return "Generic"


def extract(yt_dlp: ModuleType, request: Mapping[str, object], deno: str, cache: str) -> Info:
    """解析请求里的网址，返回回应（结果或归了类的失败）。"""
    url = text(request.get("url")) or ""
    if request.get("generic") is not True and matched(url) == "Generic":
        return {"error": {"kind": "unsupported", "message": "没有专门认这个网址的解析器"}}
    logger = Logger()
    options = params(text(request.get("proxy")) or "", text(request.get("cookies")), deno, cache, logger)
    with yt_dlp.YoutubeDL(options) as ydl:
        try:
            info = ydl.extract_info(url, download=False)
        except yt_dlp.utils.YoutubeDLError as error:
            kind, message = classify(error)
            return {"error": {"kind": kind, "message": message}}
        sanitized = yt_dlp.YoutubeDL.sanitize_info(info)
        return {"ok": trimmed(sanitized, ydl.cookiejar, logger.warnings)}


def serve(site: str, deno: str, cache: str, requests: TextIO, replies: TextIO) -> None:
    """逐行读 `requests` 里的请求、解析，回应逐行写进 `replies`，读到结尾即返回。"""
    sys.path.insert(0, site)
    yt_dlp = importlib.import_module("yt_dlp")
    for line in requests:
        if not line.strip():
            continue
        request = json.loads(line)
        reply = extract(yt_dlp, request, deno, cache)
        reply["id"] = request.get("id")
        replies.write(json.dumps(reply, ensure_ascii=False) + "\n")
        replies.flush()


def selftest(site: str) -> None:
    """导入 yt-dlp 与解 YouTube 的 JavaScript 挑战用的 yt-dlp-ejs，报出 yt-dlp 的版本。"""
    # site 目录要先加进 sys.path 才导入得到，所以按名字导入
    sys.path.insert(0, site)
    version = importlib.import_module("yt_dlp.version")
    importlib.import_module("yt_dlp_ejs")
    json.dump({"yt_dlp": version.__version__}, sys.stdout)


def main() -> None:
    """按命令行的命令运行。"""
    match sys.argv[1:]:
        case ["selftest", site]:
            selftest(site)
        case ["serve", site, deno, cache]:
            # 回应独占标准输出：yt-dlp 与依赖往标准输出写的都转到标准错误
            replies = sys.stdout
            sys.stdout = sys.stderr
            serve(site, deno, cache, sys.stdin, replies)
        case _:
            sys.exit(f"用法：driver.py selftest <site> | serve <site> <deno> <cache>，收到 {sys.argv[1:]}")


if __name__ == "__main__":
    main()
