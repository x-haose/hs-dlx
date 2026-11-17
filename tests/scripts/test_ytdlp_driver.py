"""yt-dlp 驱动脚本：对着本地起的网页跑真的 yt-dlp（通用解析器），不连外网。"""

from __future__ import annotations

import http.server
import importlib
import importlib.metadata
import io
import json
import threading
from functools import partial
from pathlib import Path
from typing import TYPE_CHECKING

import pytest

import driver

if TYPE_CHECKING:
    from collections.abc import Iterator, Mapping
    from types import ModuleType

# yt-dlp 装在开发环境的 site-packages 里：驱动脚本按目录导入
SITE = str(Path(importlib.import_module("yt_dlp").__file__ or "").parent.parent)
REQUEST_ID = 7


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


def serve(requests: list[Mapping[str, object]], tmp_path: Path) -> list[dict[str, object]]:
    """把 `requests` 逐行交给驱动脚本的常驻解析，返回逐行的回应。"""
    replies = io.StringIO()
    lines = "".join(json.dumps(request) + "\n" for request in requests)
    driver.serve(SITE, "deno", str(tmp_path / "cache"), io.StringIO(lines), replies)
    return [json.loads(line) for line in replies.getvalue().splitlines()]


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

    def flaky(yt_dlp: ModuleType, request: Mapping[str, object], deno: str, cache: str) -> dict[str, object]:
        calls.append(str(request["url"]))
        if len(calls) == 1:
            message = "网站改版"
            raise TypeError(message)
        return real(yt_dlp, request, deno, cache)

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


def test_lone_surrogates_and_nan_still_make_valid_json() -> None:
    class Jar:
        def get_cookie_header(self, url: str) -> str | None:
            return url if not url else None

    info = driver.trimmed({"title": "半个表情\ud83d", "duration": float("nan"), "id": "x"}, Jar(), [])
    line = json.dumps(info, ensure_ascii=True, allow_nan=False)
    assert json.loads(line)["title"] == "半个表情�"
    assert json.loads(line)["duration"] is None
