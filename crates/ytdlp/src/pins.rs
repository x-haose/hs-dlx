//! 随应用锁定的 Python 与 deno：版本、下载地址、SHA-256。它们不追上游最新，跟应用一起发版时改；yt-dlp 另从 PyPI
//! 单独更新（见 [`crate::Components`]）。

/// 组件包的格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Archive {
    TarGz,
    Zip,
}

/// 一个锁定的组件：解开后放在组件目录的 `<名字>-<version>` 下。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinned {
    /// 版本，进目录名；只含字母、数字、`.`、`-`、`+`
    pub version: String,
    pub url: String,
    /// 包的 SHA-256，小写十六进制
    pub sha256: String,
    pub archive: Archive,
    /// 解开后可执行文件相对于目录的路径，以 `/` 分隔
    pub executable: String,
}

/// 本平台要用的 Python 与 deno。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pins {
    /// python-build-standalone 的 install_only_stripped：带 pip，不带调试符号
    pub python: Pinned,
    /// deno：yt-dlp 解 YouTube 的 JavaScript 挑战用，yt-dlp 默认启用且唯一不给脚本文件与网络权限的运行时
    pub deno: Pinned,
}

/// python-build-standalone 的发布（2026-11-10）与其中的 CPython 版本。
const PYTHON_RELEASE: &str = "20261110";
const PYTHON_VERSION: &str = "3.13.18";
/// deno 的版本（2026-10-21）。
const DENO_VERSION: &str = "2.9.8";

impl Pins {
    /// 应用发布的平台（macOS Apple 芯片、Windows x64、Linux x64）上的；别的平台为 None。
    pub fn official() -> Option<Pins> {
        let (python_target, python_sha256, python_executable) = python_target()?;
        let (deno_target, deno_sha256, deno_executable) = deno_target()?;
        Some(Pins {
            python: Pinned {
                version: format!("{PYTHON_VERSION}-{PYTHON_RELEASE}"),
                url: format!(
                    "https://github.com/astral-sh/python-build-standalone/releases/download/\
                     {PYTHON_RELEASE}/cpython-{PYTHON_VERSION}+{PYTHON_RELEASE}-{python_target}\
                     -install_only_stripped.tar.gz"
                ),
                sha256: python_sha256.to_owned(),
                archive: Archive::TarGz,
                executable: python_executable.to_owned(),
            },
            deno: Pinned {
                version: DENO_VERSION.to_owned(),
                url: format!(
                    "https://github.com/denoland/deno/releases/download/v{DENO_VERSION}/deno-{deno_target}.zip"
                ),
                sha256: deno_sha256.to_owned(),
                archive: Archive::Zip,
                executable: deno_executable.to_owned(),
            },
        })
    }
}

/// 本平台的 python-build-standalone 目标、包的 SHA-256（发布里的 SHA256SUMS）与解开后的解释器。
fn python_target() -> Option<(&'static str, &'static str, &'static str)> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some((
            "aarch64-apple-darwin",
            "7a2384641584ab9984221fcb0fc9265df465922af71be71410529e2a9c3f0540",
            "python/bin/python3",
        ))
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some((
            "x86_64-pc-windows-msvc",
            "2e0fed09d48989f64c9305da9ff2d37a41c0d13ab2aa6283405b2b620dfbf805",
            "python/python.exe",
        ))
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some((
            "x86_64-unknown-linux-gnu",
            "f2d2199eb25880e07863b391cf7124e7ffa1338b0d65600dfcec117c4223a7fb",
            "python/bin/python3",
        ))
    } else {
        None
    }
}

/// 本平台的 deno 目标、包的 SHA-256（发布里各包的 .sha256sum）与解开后的可执行文件。
fn deno_target() -> Option<(&'static str, &'static str, &'static str)> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some((
            "aarch64-apple-darwin",
            "9f0bf660393da2eaff044cdc7444cb08c73f235849b61d224d825a194ce7c98e",
            "deno",
        ))
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some((
            "x86_64-pc-windows-msvc",
            "cba84b26e0df5ae7f74fc66b86a95b866872a86292ebd84fa1e959927edd93bf",
            "deno.exe",
        ))
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some((
            "x86_64-unknown-linux-gnu",
            "e890975bc2549807bbcc0b584ffafda72cc7c93349dd99c80483dd784f6c4609",
            "deno",
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 发布的三个平台都有锁定的组件；地址、哈希、版本号的写法对（改版本时抄错一位在这里报出）。
    #[test]
    fn the_official_pins_are_well_formed() {
        let pins = Pins::official().expect("应用发布的平台上都有");
        for (pinned, file) in [
            (&pins.python, "install_only_stripped.tar.gz"),
            (&pins.deno, ".zip"),
        ] {
            let url = hs_m3u8_core::Url::parse(&pinned.url).unwrap();
            assert_eq!(url.scheme(), "https");
            assert!(pinned.url.ends_with(file), "{}", pinned.url);
            assert_eq!(pinned.sha256.len(), 64);
            assert!(
                pinned
                    .sha256
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            );
            assert!(
                pinned
                    .version
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
            );
            assert!(!pinned.executable.starts_with('/') && !pinned.executable.contains(".."));
        }
        assert!(
            pins.python
                .url
                .contains(&format!("cpython-{PYTHON_VERSION}+{PYTHON_RELEASE}-"))
        );
        assert!(pins.deno.url.contains(&format!("/v{DENO_VERSION}/")));
    }
}
