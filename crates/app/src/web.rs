//! 网页视频（yt-dlp 解析出的）的画质选项：每档清晰度挑一个视频、配一条音轨，另有只要音频的；每一路定下由谁下载。

use hs_dlx_ytdlp::{Format, FormatKey, Info, Protocol, Size, Track};

/// 视频的编码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
    Vp9,
    Av1,
    /// 其余，yt-dlp 的写法
    Other(String),
    /// 网站没说
    Unknown,
}

impl VideoCodec {
    fn of(track: &Track) -> VideoCodec {
        let Track::Codec(codec) = track else {
            return VideoCodec::Unknown;
        };
        let lower = codec.to_ascii_lowercase();
        if lower.starts_with("avc") || lower == "h264" {
            VideoCodec::H264
        } else if lower.starts_with("hev") || lower.starts_with("hvc") || lower == "h265" {
            VideoCodec::Hevc
        } else if lower.starts_with("vp09") || lower == "vp9" {
            VideoCodec::Vp9
        } else if lower.starts_with("av01") || lower == "av1" {
            VideoCodec::Av1
        } else {
            VideoCodec::Other(codec.clone())
        }
    }
}

/// 音频的编码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioCodec {
    Aac,
    Opus,
    Mp3,
    Ac3,
    Eac3,
    Flac,
    Alac,
    Vorbis,
    Other(String),
    Unknown,
}

impl AudioCodec {
    fn of(track: &Track) -> AudioCodec {
        let Track::Codec(codec) = track else {
            return AudioCodec::Unknown;
        };
        let lower = codec.to_ascii_lowercase();
        if lower.starts_with("mp4a") || lower == "aac" {
            AudioCodec::Aac
        } else if lower == "opus" {
            AudioCodec::Opus
        } else if lower == "mp3" || lower == "mp4a.40.34" {
            AudioCodec::Mp3
        } else if lower == "ac-3" || lower == "ac3" {
            AudioCodec::Ac3
        } else if lower == "ec-3" || lower == "eac3" {
            AudioCodec::Eac3
        } else if lower == "flac" {
            AudioCodec::Flac
        } else if lower == "alac" {
            AudioCodec::Alac
        } else if lower == "vorbis" {
            AudioCodec::Vorbis
        } else {
            AudioCodec::Other(codec.clone())
        }
    }
}

/// 一路由谁下载。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// 自己的引擎：http(s) 文件走分段下载，HLS 走 HLS 引擎
    Own,
    /// 交回 yt-dlp 自己下：要模拟浏览器 TLS 指纹的、可能有 DRM 的（yt-dlp 会试）、DASH 分片与其余 yt-dlp 能下而我们
    /// 没有引擎的协议
    YtDlp,
    /// 这一版下不了：原因
    Unsupported(&'static str),
}

/// 格式 `format` 由谁下载（确定有 DRM 的格式 yt-dlp 已滤掉，不在这里）。
pub fn route(format: &Format) -> Route {
    if format.drm_maybe {
        return Route::YtDlp;
    }
    match &format.protocol {
        Protocol::Other(protocol) if protocol.starts_with("rtmp") => {
            Route::Unsupported("RTMP 这一版还不支持")
        }
        Protocol::Http | Protocol::Hls if !format.impersonate => Route::Own,
        _ => Route::YtDlp,
    }
}

/// 合并后的容器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Mp4,
    /// MP4 放不下的编码（Vorbis 等）
    Matroska,
    /// 只有一路、不合并：保持原来的格式（扩展名见 [`Choice::ext`]）
    Original,
}

/// 一个画质选项。
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    /// 视频的格式；只要音频时为 None
    pub video: Option<FormatKey>,
    /// 另配的音频的格式；视频本身带音频、或没有可配的音频时为 None
    pub audio: Option<FormatKey>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub video_codec: Option<VideoCodec>,
    pub audio_codec: Option<AudioCodec>,
    /// 各路大小之和；有一路不知道时为 None，有一路是估算的则整体是估算的
    pub size: Option<Size>,
    pub container: Container,
    /// 输出的扩展名
    pub ext: String,
    /// HDR 等，SDR 为 None
    pub dynamic_range: Option<String>,
}

/// 画质的取向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preference {
    /// 同一清晰度先选 H.264 + AAC、SDR：合出的 MP4 在各平台自带的播放器里都能放
    Compatible,
    /// 同一清晰度选码率最高的（多为 AV1 或 VP9 + Opus）
    Quality,
}

/// `info` 的画质选项：每档清晰度一个（高的在前），最后是只要音频的（有可单独下的音频时）。没有能下的格式时为空。
pub fn choices(info: &Info, preference: Preference) -> Vec<Choice> {
    let usable: Vec<&Format> = info
        .formats
        .iter()
        .filter(|format| !matches!(route(format), Route::Unsupported(_)))
        .collect();
    let audio_only: Vec<&Format> = usable
        .iter()
        .copied()
        .filter(|format| !format.video.maybe() && format.audio.maybe())
        .collect();
    let with_video: Vec<&Format> = usable
        .iter()
        .copied()
        .filter(|format| format.video.maybe())
        .collect();
    let mut heights: Vec<Option<u32>> = with_video.iter().map(|format| format.height).collect();
    heights.sort_unstable_by(|a, b| b.cmp(a));
    heights.dedup();
    let mut found = Vec::new();
    for height in heights {
        let best = with_video
            .iter()
            .copied()
            .filter(|format| format.height == height)
            .max_by(|a, b| video_rank(a, preference).total_cmp(&video_rank(b, preference)))
            .expect("这一档至少有一个格式");
        let codec = VideoCodec::of(&best.video);
        // 视频本身就带音频（或没说）时不另配
        let paired = if matches!(best.audio, Track::Absent) {
            best_audio(&audio_only, Some(&codec))
        } else {
            None
        };
        found.push(choice_of(best, paired));
    }
    if let Some(audio) = best_audio(&audio_only, None) {
        found.push(choice_of(audio, None));
    }
    found
}

/// 同一档清晰度里比较视频的分数：自己下的先于交回 yt-dlp 的；兼容优先时 H.264、SDR 在前；再比帧率与码率。
fn video_rank(format: &Format, preference: Preference) -> f64 {
    let own = if route(format) == Route::Own {
        1e12
    } else {
        0.0
    };
    let compatible = match preference {
        Preference::Compatible => {
            let h264 = if VideoCodec::of(&format.video) == VideoCodec::H264 {
                1e10
            } else {
                0.0
            };
            let sdr = if format
                .dynamic_range
                .as_deref()
                .is_none_or(|range| range.eq_ignore_ascii_case("SDR"))
            {
                1e9
            } else {
                0.0
            };
            h264 + sdr
        }
        Preference::Quality => 0.0,
    };
    let fps = format.fps.unwrap_or(0.0).min(999.0) * 1e5;
    own + compatible + fps + format.bitrate.unwrap_or(0.0).min(99_999.0)
}

/// 配给 `video` 编码的音频（`video` 为 None 时为只要音频的选项）：本来的语言在前；配 H.264、HEVC 或只要音频时 AAC 在前
/// （MP4 与各平台的播放器都认），配 VP9、AV1 时 Opus 在前（同码率下音质好）；自己下的先于交回 yt-dlp 的；再比码率。
fn best_audio<'a>(audio: &[&'a Format], video: Option<&VideoCodec>) -> Option<&'a Format> {
    let wants_aac = !matches!(video, Some(VideoCodec::Vp9 | VideoCodec::Av1));
    let rank = |format: &Format| {
        let language = f64::from(format.language_preference.unwrap_or(-1).clamp(-100, 100)) * 1e13;
        let own = if route(format) == Route::Own {
            1e12
        } else {
            0.0
        };
        let codec = match AudioCodec::of(&format.audio) {
            AudioCodec::Aac if wants_aac => 1e10,
            AudioCodec::Opus if !wants_aac => 1e10,
            _ => 0.0,
        };
        language + own + codec + format.bitrate.unwrap_or(0.0).min(99_999.0)
    };
    audio
        .iter()
        .copied()
        .max_by(|a, b| rank(a).total_cmp(&rank(b)))
}

/// 由视频（或只要音频时的音频）`main` 与另配的音频 `audio` 组成的选项。
fn choice_of(main: &Format, audio: Option<&Format>) -> Choice {
    let has_video = main.video.maybe();
    let video_codec = has_video.then(|| VideoCodec::of(&main.video));
    let audio_codec = match audio {
        Some(audio) => Some(AudioCodec::of(&audio.audio)),
        None if main.audio.maybe() => Some(AudioCodec::of(&main.audio)),
        None => None,
    };
    let size = match audio {
        Some(audio) => main.size.zip(audio.size).map(|(a, b)| Size {
            bytes: a.bytes + b.bytes,
            exact: a.exact && b.exact,
        }),
        None => main.size,
    };
    let (container, ext) = match audio {
        // 合并：MP4 放不下的编码出 MKV
        Some(_) => {
            let mp4 = !matches!(
                (&video_codec, &audio_codec),
                (Some(VideoCodec::Other(_) | VideoCodec::Unknown), _)
                    | (
                        _,
                        Some(AudioCodec::Vorbis | AudioCodec::Other(_) | AudioCodec::Unknown)
                    )
            );
            if mp4 {
                (Container::Mp4, "mp4".to_owned())
            } else {
                (Container::Matroska, "mkv".to_owned())
            }
        }
        // 自己的 HLS 引擎写的是 MP4；yt-dlp 给 HLS 的扩展名可能是 `m3u8`
        None if main.protocol == Protocol::Hls && route(main) == Route::Own => {
            (Container::Original, "mp4".to_owned())
        }
        None => (
            Container::Original,
            main.ext.clone().unwrap_or_else(|| "bin".to_owned()),
        ),
    };
    Choice {
        video: has_video.then(|| main.key()),
        audio: match audio {
            Some(audio) => Some(audio.key()),
            None if !has_video => Some(main.key()),
            None => None,
        },
        height: main.height,
        fps: main.fps,
        video_codec,
        audio_codec,
        size,
        container,
        ext,
        dynamic_range: main
            .dynamic_range
            .clone()
            .filter(|range| !range.eq_ignore_ascii_case("SDR")),
    }
}

#[cfg(test)]
mod tests {
    use hs_dlx_ytdlp::Live;

    use super::*;

    fn format(
        id: &str,
        video: Option<&str>,
        audio: Option<&str>,
        height: Option<u32>,
        tbr: f64,
    ) -> Format {
        let track = |codec: Option<&str>| match codec {
            Some("none") => Track::Absent,
            Some(codec) => Track::Codec(codec.to_owned()),
            None => Track::Unknown,
        };
        Format {
            id: id.to_owned(),
            url: format!("https://v.example/{id}"),
            protocol: Protocol::Http,
            ext: Some(if audio == Some("opus") { "webm" } else { "mp4" }.to_owned()),
            video: track(video),
            audio: track(audio),
            width: None,
            height,
            fps: Some(30.0),
            bitrate: Some(tbr),
            size: Some(Size {
                bytes: 1000,
                exact: true,
            }),
            note: None,
            language: None,
            language_preference: None,
            dynamic_range: None,
            headers: Vec::new(),
            cookie: None,
            chunk_size: None,
            available_at: None,
            drm_maybe: false,
            impersonate: false,
        }
    }

    fn info(formats: Vec<Format>) -> Info {
        Info {
            extractor: "Youtube".to_owned(),
            id: "x".to_owned(),
            title: "t".to_owned(),
            page: None,
            duration: Some(60.0),
            thumbnail: None,
            live: Live::No,
            uploader: None,
            formats,
            has_drm: false,
            entries: None,
            warnings: Vec::new(),
        }
    }

    /// YouTube 式的格式：1080p 有 H.264 与码率更高的 VP9、AV1，音频有 AAC 与 Opus。
    fn youtube() -> Vec<Format> {
        vec![
            format("140", Some("none"), Some("mp4a.40.2"), None, 129.0),
            format("251", Some("none"), Some("opus"), None, 135.0),
            format("137", Some("avc1.640028"), Some("none"), Some(1080), 4000.0),
            format(
                "248",
                Some("vp09.00.40.08"),
                Some("none"),
                Some(1080),
                5000.0,
            ),
            format(
                "399",
                Some("av01.0.08M.08"),
                Some("none"),
                Some(1080),
                4500.0,
            ),
            format("136", Some("avc1.4d401f"), Some("none"), Some(720), 2000.0),
            format(
                "18",
                Some("avc1.42001E"),
                Some("mp4a.40.2"),
                Some(360),
                500.0,
            ),
        ]
    }

    /// 选项的清晰度、视频与音频的格式编号、容器。
    type Summary<'a> = (Option<u32>, Option<&'a str>, Option<&'a str>, Container);

    #[test]
    fn compatible_choices_pair_h264_with_aac_into_mp4() {
        let found = choices(&info(youtube()), Preference::Compatible);
        let summary: Vec<Summary<'_>> = found
            .iter()
            .map(|c| {
                (
                    c.height,
                    c.video.as_ref().map(|key| key.id.as_str()),
                    c.audio.as_ref().map(|key| key.id.as_str()),
                    c.container,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (Some(1080), Some("137"), Some("140"), Container::Mp4),
                (Some(720), Some("136"), Some("140"), Container::Mp4),
                // 自带音频的不另配
                (Some(360), Some("18"), None, Container::Original),
                // 只要音频：AAC
                (None, None, Some("140"), Container::Original),
            ]
        );
        assert_eq!(
            found[0].size,
            Some(Size {
                bytes: 2000,
                exact: true
            })
        );
    }

    #[test]
    fn quality_choices_take_the_highest_bitrate_and_pair_opus() {
        let found = choices(&info(youtube()), Preference::Quality);
        assert_eq!(
            found[0].video.as_ref().map(|key| key.id.as_str()),
            Some("248")
        );
        assert_eq!(found[0].video_codec, Some(VideoCodec::Vp9));
        assert_eq!(
            found[0].audio.as_ref().map(|key| key.id.as_str()),
            Some("251")
        );
        assert_eq!(found[0].container, Container::Mp4);
        assert_eq!(found[0].ext, "mp4");
    }

    #[test]
    fn the_original_language_wins_over_a_dub() {
        let mut formats = youtube();
        // 配音的 AAC 码率更高，原声的 AAC 标着本来的语言
        let mut dub = format("140-dub", Some("none"), Some("mp4a.40.2"), None, 192.0);
        dub.language_preference = Some(-10);
        formats.push(dub);
        formats[0].language_preference = Some(10);
        let found = choices(&info(formats), Preference::Compatible);
        assert_eq!(
            found[0].audio.as_ref().map(|key| key.id.as_str()),
            Some("140")
        );
    }

    #[test]
    fn routes_follow_protocol_possible_drm_and_impersonation() {
        let mut own = format("a", Some("avc1"), Some("none"), Some(720), 1.0);
        assert_eq!(route(&own), Route::Own);
        own.protocol = Protocol::Hls;
        assert_eq!(route(&own), Route::Own);
        own.impersonate = true;
        assert_eq!(route(&own), Route::YtDlp);
        own.impersonate = false;
        own.protocol = Protocol::Dash;
        assert_eq!(route(&own), Route::YtDlp);
        own.protocol = Protocol::Other("rtmp".to_owned());
        assert!(matches!(route(&own), Route::Unsupported(_)));
        // 只有下不了的格式时没有选项
        assert_eq!(
            choices(&info(vec![own.clone()]), Preference::Compatible),
            []
        );
        own.protocol = Protocol::Http;
        own.drm_maybe = true;
        assert_eq!(route(&own), Route::YtDlp);
    }

    #[test]
    fn a_vorbis_pairing_goes_into_matroska_and_own_routes_win() {
        let mut slow = format("dash", Some("avc1"), Some("none"), Some(480), 9000.0);
        slow.protocol = Protocol::Dash;
        let fast = format("http", Some("avc1"), Some("none"), Some(480), 100.0);
        let vorbis = format("v", Some("none"), Some("vorbis"), None, 128.0);
        let found = choices(&info(vec![slow, fast, vorbis]), Preference::Quality);
        assert_eq!(
            found[0].video.as_ref().map(|key| key.id.as_str()),
            Some("http")
        );
        assert_eq!(found[0].container, Container::Matroska);
        assert_eq!(found[0].ext, "mkv");
    }
}
