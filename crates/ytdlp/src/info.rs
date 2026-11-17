//! 解析结果：驱动脚本回的 JSON 在这里翻译成自己的类型，yt-dlp 的字段名不往外传。

use serde::Deserialize;

/// 一个网页地址解析出的视频（或播放列表）。
#[derive(Debug, Clone, PartialEq)]
pub struct Info {
    /// 解析器的名字，如 `Youtube`、`BiliBili`
    pub extractor: String,
    /// 视频在这个网站上的编号；同一解析器下唯一
    pub id: String,
    pub title: String,
    /// 解析器整理过的网页地址；没有时为 None
    pub page: Option<String>,
    /// 时长，秒；不知道（直播等）时为 None
    pub duration: Option<f64>,
    pub thumbnail: Option<String>,
    pub live: Live,
    pub uploader: Option<String>,
    /// 可下的格式，按 yt-dlp 给的次序（大致由差到好）；播放列表为空
    pub formats: Vec<Format>,
    /// 有格式因 DRM 被滤掉（`formats` 里没有它们）
    pub has_drm: bool,
    /// 播放列表或分成几段的视频的条目；都不是时为 None
    pub entries: Option<Vec<Entry>>,
    /// yt-dlp 的警告，如缺 JavaScript 运行时、部分格式取不到
    pub warnings: Vec<String>,
}

/// 是不是直播。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Live {
    /// 不是直播，或网站没说
    No,
    /// 正在直播
    Now,
    /// 预告：还没开播
    Upcoming,
    /// 直播结束了，回放还在处理（可能不全）
    Processing,
    /// 直播的回放
    Recorded,
}

/// 一个格式里的一路视频或音频。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Track {
    /// 没有这一路
    Absent,
    /// 网站没说（单个文件的格式常这样，可能两路都有）
    Unknown,
    /// 编码，yt-dlp 的写法，如 `avc1.640028`、`vp09.00.40.08`、`opus`、`mp4a.40.2`
    Codec(String),
}

impl Track {
    /// 可能有这一路（没说时算可能有）。
    pub fn maybe(&self) -> bool {
        !matches!(self, Track::Absent)
    }
}

/// 下载地址的协议。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Protocol {
    /// 一个 http(s) 文件
    Http,
    /// HLS 播放列表
    Hls,
    /// DASH 分片（`http_dash_segments`）
    Dash,
    /// 其余，yt-dlp 的写法（如 `ism`、`f4m`、`rtmp`、`sabr`）
    Other(String),
}

/// 一个格式：下载它要用的全部。
#[derive(Debug, Clone, PartialEq)]
pub struct Format {
    /// 格式编号，同一视频下唯一，重新解析后仍对得上（如 YouTube 的 itag `137`）
    pub id: String,
    pub url: String,
    pub protocol: Protocol,
    /// 扩展名，如 `mp4`、`webm`、`m4a`
    pub ext: Option<String>,
    pub video: Track,
    pub audio: Track,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    /// 总码率，kbit/s
    pub bitrate: Option<f64>,
    /// 字节数；`exact` 为假时是估算的
    pub size: Option<Size>,
    /// 网站的说法，如 `1080p60`、`Premium`
    pub note: Option<String>,
    pub language: Option<String>,
    /// 音轨的语言是不是视频本来的语言：越大越是（yt-dlp 的约定：10 为本来的，-1 为不知道，-10 为别的语言；YouTube 的
    /// 默认音轨为 5）；None 为没说
    pub language_preference: Option<i32>,
    /// 动态范围，如 `SDR`、`HDR10`
    pub dynamic_range: Option<String>,
    /// 下载时原样带上的请求头（不含 Cookie）
    pub headers: Vec<(String, String)>,
    /// 发往 `url` 时带的 Cookie 请求头；没有时为 None
    pub cookie: Option<String>,
    /// 每个请求最多取多少字节（YouTube 超过 10 MiB 的请求会被限速）；None 为不限
    pub chunk_size: Option<u64>,
    /// 这个时刻（Unix 秒）之前不能开始下（YouTube 片头广告的时长）；None 为随时
    pub available_at: Option<i64>,
    /// 可能有 DRM（确定有的 yt-dlp 已滤掉）：yt-dlp 自己会试着下
    pub drm_maybe: bool,
    /// 要模拟浏览器的 TLS 指纹才下得了
    pub impersonate: bool,
}

/// 格式的大小。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub bytes: u64,
    pub exact: bool,
}

/// 播放列表的一个条目。
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub url: Option<String>,
    pub id: Option<String>,
    pub title: Option<String>,
    pub duration: Option<f64>,
}

/// 驱动脚本回的结果（见 driver.py 的 `trimmed`）。
#[derive(Deserialize)]
pub(crate) struct Wire {
    extractor: String,
    id: String,
    title: String,
    webpage_url: Option<String>,
    duration: Option<f64>,
    thumbnail: Option<String>,
    live_status: Option<String>,
    uploader: Option<String>,
    formats: Vec<WireFormat>,
    has_drm: bool,
    entries: Option<Vec<WireEntry>>,
    warnings: Vec<String>,
}

#[derive(Deserialize)]
struct WireFormat {
    format_id: String,
    url: String,
    protocol: String,
    ext: Option<String>,
    vcodec: Option<String>,
    acodec: Option<String>,
    width: Option<f64>,
    height: Option<f64>,
    fps: Option<f64>,
    tbr: Option<f64>,
    filesize: Option<f64>,
    filesize_approx: Option<f64>,
    format_note: Option<String>,
    language: Option<String>,
    language_preference: Option<f64>,
    dynamic_range: Option<String>,
    http_headers: std::collections::BTreeMap<String, String>,
    cookie: Option<String>,
    chunk_size: Option<f64>,
    available_at: Option<f64>,
    drm_maybe: bool,
    impersonate: bool,
}

#[derive(Deserialize)]
struct WireEntry {
    url: Option<String>,
    id: Option<String>,
    title: Option<String>,
    duration: Option<f64>,
}

impl Wire {
    pub(crate) fn into_info(self) -> Info {
        Info {
            extractor: self.extractor,
            id: self.id,
            title: self.title,
            page: self.webpage_url,
            duration: self.duration,
            thumbnail: self.thumbnail,
            live: match self.live_status.as_deref() {
                Some("is_live") => Live::Now,
                Some("is_upcoming") => Live::Upcoming,
                Some("post_live") => Live::Processing,
                Some("was_live") => Live::Recorded,
                _ => Live::No,
            },
            uploader: self.uploader,
            formats: self
                .formats
                .into_iter()
                .map(WireFormat::into_format)
                .collect(),
            has_drm: self.has_drm,
            entries: self.entries.map(|entries| {
                entries
                    .into_iter()
                    .map(|entry| Entry {
                        url: entry.url,
                        id: entry.id,
                        title: entry.title,
                        duration: entry.duration,
                    })
                    .collect()
            }),
            warnings: self.warnings,
        }
    }
}

impl WireFormat {
    fn into_format(self) -> Format {
        let size = match (
            self.filesize.and_then(whole),
            self.filesize_approx.and_then(whole),
        ) {
            (Some(bytes), _) => Some(Size { bytes, exact: true }),
            (None, Some(bytes)) => Some(Size {
                bytes,
                exact: false,
            }),
            (None, None) => None,
        };
        Format {
            id: self.format_id,
            url: self.url,
            protocol: match self.protocol.as_str() {
                "http" | "https" => Protocol::Http,
                "m3u8" | "m3u8_native" => Protocol::Hls,
                "http_dash_segments" => Protocol::Dash,
                other => Protocol::Other(other.to_owned()),
            },
            ext: self.ext,
            video: track(self.vcodec),
            audio: track(self.acodec),
            width: self
                .width
                .and_then(whole)
                .and_then(|w| u32::try_from(w).ok()),
            height: self
                .height
                .and_then(whole)
                .and_then(|h| u32::try_from(h).ok()),
            fps: self.fps,
            bitrate: self.tbr,
            size,
            note: self.format_note,
            language: self.language,
            language_preference: self
                .language_preference
                .filter(|preference| preference.is_finite())
                .map(|preference| preference.round() as i32),
            dynamic_range: self.dynamic_range,
            headers: self
                .http_headers
                .into_iter()
                .filter(|(name, _)| !name.eq_ignore_ascii_case("cookie"))
                .collect(),
            cookie: self.cookie.filter(|cookie| !cookie.is_empty()),
            chunk_size: self.chunk_size.and_then(whole).filter(|size| *size > 0),
            available_at: self
                .available_at
                .filter(|at| at.is_finite())
                .map(|at| at as i64),
            drm_maybe: self.drm_maybe,
            impersonate: self.impersonate,
        }
    }
}

/// yt-dlp 的编码写法：`none` 为没有这一路，没写为没说。
fn track(codec: Option<String>) -> Track {
    match codec {
        None => Track::Unknown,
        Some(codec) if codec == "none" => Track::Absent,
        Some(codec) => Track::Codec(codec),
    }
}

/// 非负的有限数取整；其余为 None。
fn whole(value: f64) -> Option<u64> {
    (value.is_finite() && value >= 0.0).then(|| value.round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_formats_translate_to_tracks_sizes_and_limits() {
        let wire: Wire = serde_json::from_value(serde_json::json!({
            "extractor": "Youtube", "id": "abc", "title": "t", "webpage_url": null, "duration": 12.5,
            "thumbnail": null, "live_status": "was_live", "uploader": null, "has_drm": false, "entries": null, "warnings": [],
            "formats": [
                {
                    "format_id": "137", "url": "https://r.googlevideo.com/v", "protocol": "https", "ext": "mp4",
                    "vcodec": "avc1.640028", "acodec": "none", "width": 1920.0, "height": 1080.0, "fps": 30.0,
                    "tbr": 4000.0, "filesize": null, "filesize_approx": 1234.4, "format_note": "1080p",
                    "language": null, "language_preference": -1.0, "dynamic_range": "SDR",
                    "http_headers": {"User-Agent": "x", "Cookie": "leak=1"}, "cookie": "",
                    "chunk_size": 10485760.0, "available_at": 1763400000.0, "drm_maybe": false, "impersonate": false
                },
                {
                    "format_id": "hls-1", "url": "https://e/x.m3u8", "protocol": "m3u8_native", "ext": "mp4",
                    "vcodec": null, "acodec": null, "width": null, "height": null, "fps": null, "tbr": null,
                    "filesize": 99.0, "filesize_approx": null, "format_note": null, "language": "ja",
                    "language_preference": 10.0,
                    "dynamic_range": null, "http_headers": {}, "cookie": "a=b", "chunk_size": null,
                    "available_at": null, "drm_maybe": true, "impersonate": true
                }
            ]
        }))
        .unwrap();
        let info = wire.into_info();
        assert_eq!(info.live, Live::Recorded);
        let [video, hls] = &info.formats[..] else {
            panic!("两个格式：{:?}", info.formats);
        };
        assert_eq!(video.protocol, Protocol::Http);
        assert_eq!(video.video, Track::Codec("avc1.640028".to_owned()));
        assert_eq!(video.audio, Track::Absent);
        assert!(!video.audio.maybe());
        assert_eq!(
            video.size,
            Some(Size {
                bytes: 1234,
                exact: false
            })
        );
        assert_eq!(
            video.headers,
            vec![("User-Agent".to_owned(), "x".to_owned())]
        );
        assert_eq!(video.cookie, None);
        assert_eq!(video.chunk_size, Some(10 << 20));
        assert_eq!(video.available_at, Some(1_763_400_000));
        assert_eq!(hls.protocol, Protocol::Hls);
        assert_eq!(hls.video, Track::Unknown);
        assert!(hls.video.maybe());
        assert_eq!(
            hls.size,
            Some(Size {
                bytes: 99,
                exact: true
            })
        );
        assert_eq!(hls.cookie.as_deref(), Some("a=b"));
        assert_eq!(
            (video.language_preference, hls.language_preference),
            (Some(-1), Some(10))
        );
        assert!(hls.drm_maybe && hls.impersonate);
    }
}
