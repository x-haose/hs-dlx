//! 随应用带的运行时：裁过的 Python（python-build-standalone 去掉 Tcl/Tk、头文件等，tar 经 zstd 压缩）与
//! QuickJS-NG（yt-dlp 解 YouTube 的 JavaScript 挑战用，单个可执行文件）。构建时由 scripts/runtime.py 备好、打进应用；
//! 第一次用时解进组件目录（见 [`crate::Components`]），不联网。它们跟应用一起发版时换；yt-dlp 另从 PyPI 单独更新。

use std::borrow::Cow;

/// 随应用带的一样运行时。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundled {
    /// 认出这一份的名字，进目录名（`<名>-<id>`）：换了内容即换了名字。只含字母、数字、`.`、`-`、`+`
    pub id: String,
    /// Python 为 tar 经 zstd 压缩（条目以 `python/` 开头），QuickJS-NG 为可执行文件本身
    pub bytes: Cow<'static, [u8]>,
    /// 解开后可执行文件相对于目录的路径，以 `/` 分隔
    pub executable: String,
}

/// 本平台随应用带的 Python 与 QuickJS-NG。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    pub python: Bundled,
    pub quickjs: Bundled,
}
