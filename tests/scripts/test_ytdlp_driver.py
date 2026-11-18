"""yt-dlp 驱动脚本：对着本地起的网页跑真的 yt-dlp（通用解析器），不连外网。"""

from __future__ import annotations

import http.server
import importlib
import importlib.metadata
import io
import json
import socket
import ssl
import struct
import sys
import threading
from functools import partial
from pathlib import Path
from typing import TYPE_CHECKING, Protocol

import pytest

import driver

if TYPE_CHECKING:
    from collections.abc import Callable, Iterator, Mapping
    from types import ModuleType


class Warns(Protocol):
    """yt-dlp 读浏览器 Cookie 时给的日志里用到的部分。"""

    def warning(self, message: str, *, only_once: bool = False) -> None:
        """记一条警告。"""
        ...


# yt-dlp 装在开发环境的 site-packages 里：驱动脚本按目录导入
SITE = str(Path(importlib.import_module("yt_dlp").__file__ or "").parent.parent)
REQUEST_ID = 7
# 有专门解析器的网址：读浏览器 Cookie 失败时不发请求，测试不联网
YOUTUBE = "https://www.youtube.com/watch?v=V8dKO3snMG8"


@pytest.fixture
def site(tmp_path: Path) -> Iterator[str]:
    """本地网页：`/video.html` 里嵌了 `/clip.mp4`，`/empty.html` 里什么都没有。"""
    (tmp_path / "video.html").write_text(
        '<html><head><title>本地视频</title></head><body><video src="/clip.mp4"></video></body></html>',
        encoding="utf-8",
    )
    (tmp_path / "empty.html").write_text("<html><body>没有视频</body></html>", encoding="utf-8")
    (tmp_path / "clip.mp4").write_bytes(b"\x00\x00\x00\x18ftypisom" + bytes(64))
    handler = partial(http.server.SimpleHTTPRequestHandler, directory=str(tmp_path))
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    yield f"http://127.0.0.1:{server.server_address[1]}"
    server.shutdown()
    server.server_close()


# 测试用的 Cookie 过期时刻（Unix 秒，2100 年）
EXPIRES = 4102444800


def serve_lines(requests: list[Mapping[str, object]], tmp_path: Path) -> list[dict[str, object]]:
    """把 `requests` 逐行交给驱动脚本的常驻解析，返回它写出的每一行（含开始解析网页的那行）。"""
    replies = io.StringIO()
    lines = "".join(json.dumps(request) + "\n" for request in requests)
    driver.serve(SITE, "qjs", str(tmp_path / "cache"), io.StringIO(lines), replies)
    return [json.loads(line) for line in replies.getvalue().splitlines()]


def serve(requests: list[Mapping[str, object]], tmp_path: Path) -> list[dict[str, object]]:
    """同 `serve_lines`，只要回应（结果或失败）。"""
    return [line for line in serve_lines(requests, tmp_path) if "stage" not in line]


def test_a_page_with_a_video_is_extracted_with_its_format(site: str, tmp_path: Path) -> None:
    cookies = "# Netscape HTTP Cookie File\n127.0.0.1\tFALSE\t/\tFALSE\t0\tsession\tabc\n"
    request = {"id": REQUEST_ID, "url": f"{site}/video.html", "proxy": "", "cookies": cookies, "generic": True}
    [reply] = serve([request], tmp_path)
    assert reply["id"] == REQUEST_ID
    info = driver.mapping(reply["ok"]) or {}
    [fmt] = [driver.mapping(item) or {} for item in driver.items(info["formats"])]
    assert fmt["url"] == f"{site}/clip.mp4"
    assert fmt["protocol"] == "http"
    # 发往视频地址该带的 Cookie 按 Cookie 罐算出
    assert fmt["cookie"] == "session=abc"
    assert info["entries"] is None


def test_a_page_without_a_video_is_unsupported_and_the_next_request_still_runs(site: str, tmp_path: Path) -> None:
    replies = serve(
        [
            {"id": 1, "url": f"{site}/empty.html", "proxy": "", "cookies": None, "generic": True},
            {"id": 2, "url": f"{site}/video.html", "proxy": "", "cookies": None, "generic": True},
        ],
        tmp_path,
    )
    assert [reply["id"] for reply in replies] == [1, 2]
    error = driver.mapping(replies[0]["error"]) or {}
    assert error["kind"] == "unsupported"
    assert "ok" in replies[1]


def test_a_url_only_the_generic_extractor_knows_is_unsupported_without_a_request(tmp_path: Path) -> None:
    # 没有服务器：不发请求即回
    [reply] = serve([{"id": 1, "url": "http://127.0.0.1:9/page.html", "proxy": "", "cookies": None}], tmp_path)
    error = driver.mapping(reply["error"]) or {}
    assert error["kind"] == "unsupported"


def test_a_site_extractor_is_matched_before_the_generic_one() -> None:
    assert driver.matched("https://www.youtube.com/watch?v=dQw4w9WgXcQ") == "Youtube"
    assert driver.matched("https://example.com/page.html") == "Generic"


def test_requirements_take_the_pinned_dependencies_without_mutagen() -> None:
    requires: list[str] = importlib.metadata.distribution("yt-dlp").requires or []
    found = {item["name"]: item["version"] for item in driver.requirements(list[object](requires))}
    for name in ("certifi", "requests", "urllib3", "yt-dlp-ejs", "curl-cffi", "cffi", "pycparser", "brotli"):
        assert name in found, f"缺 {name}：{found}"
    assert "mutagen" not in found
    # 别的解释器才要的不在
    assert "brotlicffi" not in found
    assert all(version and "*" not in version for version in found.values())


def test_pick_takes_the_wheel_this_interpreter_can_install(capsys: pytest.CaptureFixture[str]) -> None:
    files = {
        "pure": [
            {"filename": "pure-1.0.tar.gz"},
            {"filename": "pure-1.0-cp27-cp27m-win32.whl"},
            {"filename": "pure-1.0-py3-none-any.whl"},
        ]
    }
    assert driver.pick(files) == {"pure": "pure-1.0-py3-none-any.whl"}
    with pytest.raises(SystemExit):
        driver.pick({"old": [{"filename": "old-1.0-cp27-cp27m-win32.whl"}]})
    capsys.readouterr()


def test_an_unexpected_error_is_answered_and_the_next_request_still_runs(
    site: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    real = driver.extract
    calls: list[str] = []

    def flaky(
        yt_dlp: ModuleType, request: Mapping[str, object], quickjs: str, cache: str, resolving: Callable[[], None]
    ) -> dict[str, object]:
        calls.append(str(request["url"]))
        if len(calls) == 1:
            message = "网站改版"
            raise TypeError(message)
        return real(yt_dlp, request, quickjs, cache, resolving)

    monkeypatch.setattr(driver, "extract", flaky)
    replies = serve(
        [
            {"id": 1, "url": f"{site}/video.html", "proxy": "", "cookies": None, "generic": True},
            {"id": 2, "url": f"{site}/video.html", "proxy": "", "cookies": None, "generic": True},
        ],
        tmp_path,
    )
    error = driver.mapping(replies[0]["error"]) or {}
    assert error["kind"] == "other"
    assert "TypeError" in str(error["message"])
    assert "ok" in replies[1]


def test_resolving_is_announced_only_once_the_cookies_are_read(
    site: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # 读完 Cookie 才开始算解析的时限：先报开始解析，再回结果；读浏览器没成的没有这一行
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("APPDATA", str(tmp_path))
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path))
    page = {"id": 1, "url": f"{site}/video.html", "proxy": "", "cookies": None, "generic": True}
    stuck = {"id": 2, "url": YOUTUBE, "proxy": "", "browser": "firefox", "generic": False}
    lines = serve_lines([page, stuck], tmp_path)
    assert [(line["id"], sorted(line)) for line in lines] == [
        (1, ["id", "stage"]),
        (1, ["id", "ok"]),
        (2, ["error", "id"]),
    ]
    assert lines[0]["stage"] == "resolving"


def test_cookies_the_browser_cannot_give_fail_before_the_page_is_asked(
    site: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # 这台机器上（HOME 指到空目录）没有 Firefox 的 Cookie：说没找到，不发请求（网址是 YouTube 的，没联网）
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("APPDATA", str(tmp_path))
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path))
    request = {"id": REQUEST_ID, "url": YOUTUBE, "proxy": "", "browser": "firefox", "generic": False}
    [reply] = serve([request], tmp_path)
    error = driver.mapping(reply["error"]) or {}
    assert (error["kind"], error["reason"]) == ("cookies", "missing")

    # 同时带着嗅探页的 Cookie：退出时 yt-dlp 存 cookiefile 不再把读不出的又抛一遍
    cookies_txt = "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tFALSE\t0\tsession\tabc\n"
    [reply] = serve([{**request, "cookies": cookies_txt}], tmp_path)
    error = driver.mapping(reply["error"]) or {}
    assert (error["kind"], error["reason"]) == ("cookies", "missing")

    # 只有通用解析器认得的网址不读浏览器的 Cookie：照常解析出本地网页
    [reply] = serve([{**request, "url": f"{site}/video.html", "generic": True}], tmp_path)
    assert "ok" in reply

    # 加密的都解不开（钥匙串没给钥匙时 yt-dlp 只警告、跳过）：没读成；读的是请求里指定的用户资料
    cookies = importlib.import_module("yt_dlp.cookies")
    profiles: list[str | None] = []

    def keyless(_browser: str, profile: str | None, logger: Warns, **_options: object) -> object:
        profiles.append(profile)
        logger.warning("cannot decrypt v10 cookies: no key found", only_once=True)
        return cookies.YoutubeDLCookieJar()

    monkeypatch.setattr(cookies, "extract_cookies_from_browser", keyless)
    profile = str(tmp_path / "Chrome" / "Profile 2")
    [reply] = serve([{**request, "browser": "chrome", "profile": profile}], tmp_path)
    assert profiles == [profile]
    error = driver.mapping(reply["error"]) or {}
    assert error["kind"] == "cookies"
    assert error["reason"] == {"darwin": "keychain", "win32": "encrypted"}.get(sys.platform, "keyring")
    assert "no key found" in str(error["message"])


def test_warnings_that_leave_the_other_cookies_readable_do_not_fail(monkeypatch: pytest.MonkeyPatch) -> None:
    # 数据库版本较新、某一条解不开：其余照常用，缺的若要用网站自会说要登录
    cookies = importlib.import_module("yt_dlp.cookies")
    yt_dlp = importlib.import_module("yt_dlp")

    def partly(_browser: str, _profile: str | None, logger: Warns, **_options: object) -> object:
        logger.warning("Possibly unsupported firefox cookies database version: 18", only_once=True)
        logger.warning(
            "failed to decrypt cookie (AES-GCM) because the MAC check failed. Possibly the key is wrong?",
            only_once=True,
        )
        logger.warning("failed to parse Safari cookie because UTF-8 decoding failed", only_once=True)
        return cookies.YoutubeDLCookieJar()

    monkeypatch.setattr(cookies, "extract_cookies_from_browser", partly)
    logger = driver.Logger()
    options = driver.params({"url": YOUTUBE, "browser": "firefox"}, "qjs", "", logger)
    with yt_dlp.YoutubeDL(options) as ydl:
        assert driver.browser_cookies(yt_dlp, ydl, ("firefox", None), logger) is None


def test_only_the_cookies_of_the_site_go_back_to_the_app() -> None:
    # 这个网站各个子域、各个路径下的都交回（解析时请求的接口常在别的子域）；别的网站的不交出去，名字像的也不算
    cookies = importlib.import_module("yt_dlp.cookies")
    jar = cookies.YoutubeDLCookieJar()
    jar.load(
        io.StringIO(
            "# Netscape HTTP Cookie File\n"
            f".bilibili.com\tTRUE\t/\tTRUE\t{EXPIRES}\tSESSDATA\tabc\n"
            "api.bilibili.com\tFALSE\t/x/\tFALSE\t0\tbuvid\tdef\n"
            "notbilibili.com\tFALSE\t/\tFALSE\t0\tlookalike\tno\n"
            f"bank.example\tFALSE\t/\tTRUE\t{EXPIRES}\tsession\tsecret\n"
        )
    )
    handed = {cookie["name"]: cookie for cookie in driver.site_cookies(jar, "bilibili.com")}
    assert sorted(handed) == ["SESSDATA", "buvid"]
    assert handed["SESSDATA"] == {
        "domain": ".bilibili.com",
        "include_subdomains": True,
        "path": "/",
        "secure": True,
        "expires": EXPIRES,
        "name": "SESSDATA",
        "value": "abc",
    }
    assert (handed["buvid"]["domain"], handed["buvid"]["path"]) == ("api.bilibili.com", "/x/")
    assert driver.site_cookies(jar, "") == []


def answer_once(how: str) -> int:
    """起一个只接一个连接的本地服务器，返回端口。

    读到客户端发来的第一段后：`reset` 以 RST 断开，`close` 正常断开，`plain` 回一段不是 TLS 的明文再断开。
    """
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)

    def serve() -> None:
        with listener:
            connection, _ = listener.accept()
            connection.recv(4096)
            if how == "reset":
                connection.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
            elif how == "plain":
                connection.sendall(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
            connection.close()

    threading.Thread(target=serve, daemon=True).start()
    return int(listener.getsockname()[1])


@pytest.mark.parametrize(
    ("scheme", "how", "kind"),
    [("http", "reset", "reset"), ("https", "close", "reset"), ("https", "plain", "network")],
)
def test_a_connection_cut_off_after_it_was_made_is_told_apart(scheme: str, how: str, kind: str) -> None:
    # 真的经 yt-dlp 的请求处理器（与应用装的一样是 requests）：读请求后连接被重置、TLS 握手时对方断开归 reset；
    # 对方回的不是 TLS（老站、协议不合一类）不算
    yt_dlp = importlib.import_module("yt_dlp")
    utils = importlib.import_module("yt_dlp.utils")
    port = answer_once(how)
    with yt_dlp.YoutubeDL({"quiet": True, "retries": 0}) as ydl:
        try:
            ydl.urlopen(f"{scheme}://127.0.0.1:{port}/")
        except utils.YoutubeDLError as error:
            refused = error
        else:
            pytest.fail("连接被掐断却拿到了回应")
    assert driver.classify(report_error(utils.ExtractorError("x", cause=refused)))[0] == kind


def test_the_cookies_handed_back_have_whole_second_expiries() -> None:
    # Firefox 142 起 yt-dlp 读出的过期时间是浮点数，应用按整数秒收
    cookies = importlib.import_module("yt_dlp.cookies")
    jar = cookies.YoutubeDLCookieJar()
    jar.load(io.StringIO("# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tSID\tabc\n"))
    [cookie] = list(jar)
    cookie.expires = EXPIRES + 0.5
    [handed] = driver.site_cookies(jar, "youtube.com")
    assert handed["expires"] == EXPIRES
    assert isinstance(handed["expires"], int)


def test_lone_surrogates_and_nan_still_make_valid_json() -> None:
    class Jar:
        def get_cookie_header(self, url: str) -> str | None:
            return url if not url else None

    info = driver.trimmed({"title": "半个表情\ud83d", "duration": float("nan"), "id": "x"}, Jar(), [])
    line = json.dumps(info, ensure_ascii=True, allow_nan=False)
    assert json.loads(line)["title"] == "半个表情�"
    assert json.loads(line)["duration"] is None


def test_yt_dlp_downloads_a_format_itself_with_progress(
    site: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    [reply] = serve([{"id": 1, "url": f"{site}/video.html", "proxy": "", "cookies": None, "generic": True}], tmp_path)
    info = driver.mapping(reply["ok"]) or {}
    [fmt] = [driver.mapping(item) or {} for item in driver.items(info["formats"])]
    # 目录里的 $HOME 与 % 原样保留
    folder = tmp_path / "下载 100% $HOME"
    folder.mkdir()
    monkeypatch.chdir(folder)
    replies = io.StringIO()
    wanted = driver.key_of(fmt)
    request = {"url": f"{site}/video.html", "format": wanted, "name": "video.mp4", "proxy": "", "cookies": None}
    final = driver.download(importlib.import_module("yt_dlp"), request, "qjs", str(tmp_path / "cache"), replies)
    assert final == {"ok": {}}
    assert (folder / "video.mp4").read_bytes() == (tmp_path / "clip.mp4").read_bytes()
    lines = [json.loads(line) for line in replies.getvalue().splitlines()]
    assert all("progress" in line for line in lines)


def test_a_format_not_found_or_not_the_same_is_changed(
    site: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.chdir(tmp_path)
    yt_dlp = importlib.import_module("yt_dlp")
    cache = str(tmp_path / "cache")
    request: dict[str, object] = {"url": f"{site}/video.html", "format": {"format_id": "no-such"}, "name": "x.mp4"}
    final = driver.download(yt_dlp, request, "qjs", cache, io.StringIO())
    assert (driver.mapping(final["error"]) or {})["kind"] == "changed"
    [reply] = serve([{"id": 1, "url": f"{site}/video.html", "proxy": "", "cookies": None, "generic": True}], tmp_path)
    [fmt] = [driver.mapping(item) or {} for item in driver.items((driver.mapping(reply["ok"]) or {})["formats"])]
    # 编号对得上、扩展名对不上：不是要的那个
    request["format"] = {**driver.key_of(fmt), "ext": "webm"}
    final = driver.download(yt_dlp, request, "qjs", cache, io.StringIO())
    assert (driver.mapping(final["error"]) or {})["kind"] == "changed"
    assert not (tmp_path / "x.mp4").exists()


def test_an_output_name_with_dollar_or_separator_is_refused(tmp_path: Path) -> None:
    for name in ("$HOME.mp4", "~x.mp4", "a/b.mp4", ".."):
        request = {"url": "http://127.0.0.1:9/", "format": {"format_id": "0"}, "name": name}
        final = driver.download(importlib.import_module("yt_dlp"), request, "qjs", str(tmp_path), io.StringIO())
        assert "error" in final, name


def test_only_media_extensions_on_the_allowlist_pass() -> None:
    assert driver.safe_ext("mp4") == "mp4"
    assert driver.safe_ext("M4A") == "M4A"
    for unsafe in ("exe", "bat", "part", "json", "../x", "a/b", "", None, 3):
        assert driver.safe_ext(unsafe) is None, unsafe


def report_error(error: BaseException) -> BaseException:
    """`error` 在 except 里被 yt-dlp 的 report_error 包成 DownloadError 的样子。"""
    utils = importlib.import_module("yt_dlp.utils")
    try:
        raise error
    except (utils.YoutubeDLError, OSError) as caught:
        wrapped: BaseException = utils.DownloadError(str(caught), sys.exc_info())
    return wrapped


def unavailable(cause: OSError) -> BaseException:
    """yt-dlp 写文件失败时抛的 UnavailableVideoError（引起它的错误在 `__context__` 里）。"""
    utils = importlib.import_module("yt_dlp.utils")
    try:
        raise cause
    except OSError:
        wrapped: BaseException = utils.UnavailableVideoError(cause)
        wrapped.__context__ = cause
    return wrapped


def test_failures_are_classified_by_what_the_site_said_and_where_they_happened() -> None:
    utils = importlib.import_module("yt_dlp.utils")
    bot = utils.ExtractorError("Sign in to confirm you're not a bot", expected=True)
    assert driver.classify(report_error(bot))[0] == "login"
    limited = utils.ExtractorError("This content isn't available, try again later. rate-limited", expected=True)
    assert driver.classify(report_error(limited))[0] == "rate_limited"
    gone = utils.ExtractorError("Private video", expected=True)
    assert driver.classify(report_error(gone))[0] == "unavailable"
    members = utils.ExtractorError("Join this channel to get access to members-only content", expected=True)
    assert driver.classify(report_error(members))[0] == "login"
    # 解析时接口回的错误状态：yt-dlp 包成 expected 的 ExtractorError，按状态归类
    networking = importlib.import_module("yt_dlp.networking")
    for status, kind in (
        (412, "blocked"),
        (403, "blocked"),
        (503, "network"),
        (404, "unavailable"),
        (429, "rate_limited"),
        (401, "login"),
    ):
        response = networking.Response(io.BytesIO(b""), "http://x/", {}, status=status)
        cause = networking.exceptions.HTTPError(response)
        refused = utils.ExtractorError(f"HTTP Error {status}", cause=cause, expected=True)
        assert driver.classify(report_error(refused))[0] == kind, status
        # 下载时直链回的：除了限流都算网络（再试即换新的直链）
        downloading = "rate_limited" if status == driver.TOO_MANY_REQUESTS else "network"
        assert driver.classify(report_error(cause), downloading=True)[0] == downloading, status
    assert driver.classify(report_error(utils.ContentTooShortError(1, 2)))[0] == "network"
    # 证书不受信任不算被掐断
    untrusted = networking.exceptions.CertificateVerifyError(cause=ssl.SSLCertVerificationError("bad"))
    assert driver.classify(report_error(utils.ExtractorError("x", cause=untrusted)))[0] == "network"
    full = OSError(28, "No space left on device")
    assert driver.classify(report_error(full))[0] == "disk"
    assert driver.classify(unavailable(full))[0] == "disk"
