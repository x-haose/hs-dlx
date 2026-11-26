# 参与开发

## 环境

- Rust：版本与组件由 `rust-toolchain.toml` 固定。
- [uv](https://docs.astral.sh/uv/)：版本见 `pyproject.toml` 的 `required-version`，运行仓库里的 Python 脚本（第三方声明、图标、
  找 FFmpeg 的构建脚本、发版）。
- Linux：WebKitGTK 4.1、GTK 3、xkbcommon 与 fontconfig 的开发包，包名见 `.github/workflows/check.yml`。
- 编译 libtorrent 与 OpenSSL：CMake、Ninja、Perl。
- Windows：带 VC 工具的 Visual Studio 与 MSYS2（`make`、编译 FFmpeg 与 libtorrent 用），OpenSSL 的配置要 Windows 原生的
  Perl（Strawberry Perl），做法见 `.github/actions/msvc-env` 与 `.github/workflows/check.yml`。
- `make check` 另要 cargo-deny 与 cargo-llvm-cov，`make package` 另要 cargo-packager；版本分别见 `check.yml` 与
  `desktop.yml`。

FFmpeg 用 hs-m3u8（`Cargo.lock` 固定的提交）里的构建脚本编译成静态库，装到 `third_party/ffmpeg/dist`。第一次与升级
hs-m3u8 之后运行 `make ffmpeg`；装的不是当前那份脚本编出来的时，构建前的检查会失败并提示。

BT 引擎（`crates/torrent`）用的 libtorrent、OpenSSL 静态库与 Boost 的头文件由 `third_party/libtorrent/build.sh` 编译，装到
`third_party/libtorrent/dist`（十几分钟）。第一次与改了这个脚本之后运行 `make libtorrent`，检查同样核对。

网页视频要的 Python（python-build-standalone）与 QuickJS-NG 由 `scripts/runtime.py` 下载、核对、裁剪后放到
`third_party/runtime/dist`，桌面应用构建时编进程序。第一次与改了这个脚本之后运行 `make runtime`，检查同样核对。

## 检查

```sh
make check
```

| 目标 | 内容 |
|---|---|
| `rs_fmt` | `cargo fmt --check` |
| `rs_lint` | 依赖方向（`scripts/check_deps.sh`）、不许用属性或注释压掉检查（`scripts/check_no_suppression.sh`）、cargo-deny（安全公告、许可证、来源）、随安装包分发的第三方声明是否最新 |
| `rs_clippy` | 全部目标，告警即失败 |
| `rs_doc` | 文档注释里的链接都能解析，告警即失败 |
| `rs_test` | 全部测试 |
| `rs_cov` | `crates/app`（任务存储与生命周期）行覆盖不低于 80% |
| `py_check`、`py_test` | 仓库里 Python 脚本的格式、ruff、mypy、basedpyright 与测试 |

CI 在 macOS、Windows、Linux 上跑格式、clippy、文档与测试；与平台无关的几项只在 Linux 上跑。

依赖或 FFmpeg 版本变了之后运行 `make licenses` 重新生成 `apps/desktop/LICENSES`。

## 分支

- `dev`：日常开发。直接提交，或从 `dev` 拉分支再提 PR 回 `dev`。推送与 PR 都跑检查。
- `main`：发布线，只接受来自 `dev` 的 PR。分支保护要求走 PR、三个平台的检查都通过，管理员也不能直接推送。仓库只
  开了 merge commit 一种合并方式：`main` 与 `dev` 的历史保持一致，下一次从 `dev` 提 PR 时不会出现重复的提交。

提交信息写成 `<type>: <描述>`，type 为 `feat`、`fix`、`refactor`、`docs`、`test`、`chore`、`perf`、`ci`。
提交信息（含正文）里不要出现 `[skip ci]`、`[ci skip]`、`[no ci]`、`[skip actions]`、`[actions skip]`：GitHub 见到就不为
这次推送与 PR 跑工作流，PR 上没有检查、合并不了，推到 main 上的也不会发版。

## 发版

版本号只写在 `Cargo.toml` 的 `[workspace.package] version` 一处，形如 `X.Y.Z`；每个版本的说明写在 `CHANGELOG.md`。

1. 平时把面向使用者的变更写进 `CHANGELOG.md` 的 `## [Unreleased]`。
2. 发版时在 `dev` 上：把 `[Unreleased]` 下的内容移到新的一节 `## [X.Y.Z] - YYYY-MM-DD`（写给用户看，这一节原样成为
   GitHub Release 的描述）；改版本号并运行 `cargo update --workspace` 让 `Cargo.lock` 跟上（打包用 `--locked` 构建）；
   提交 `chore: 发版 vX.Y.Z`。版本号改了而更新日志里没有这一节时，检查里的测试就会失败。
3. 从 `dev` 向 `main` 提 PR，标题写 `release: vX.Y.Z <一句话概括>`。检查通过后合并。
4. `main` 收到推送后 `.github/workflows/release.yml` 接手：
   - 这个版本号已被 `main` 上后来的推送改掉（含重跑一次旧的运行）时不发，免得发出旧版本并标成最新。
   - 核对版本号：`vX.Y.Z` 标签已有对应的公开 Release 时到此为止，不发版；标签在而没有公开的 Release 时失败，要人看。
   - 确认更新日志里有这个版本的一节，再由 `desktop.yml` 在三个平台上打安装包：`.dmg`（Apple 芯片）、`-setup.exe`
     （x64）、`.deb`（amd64）。
   - 全部成功后建 Release：先建草稿，用仓库的 Secret `MINISIGN_SECRET_KEY`（minisign 私钥文件的全文，私钥用
     `minisign -G -W` 生成、不带口令）给每个安装包
     签名并用 `apps/desktop/update.pub` 验一遍，再上传安装包、签名（`.minisig`）与 `SHA256SUMS`，最后公开；公开时
     GitHub 在这个提交上打 `vX.Y.Z` 标签。应用的自动更新按这些签名验安装包，私钥丢了已装的版本就验不了之后的更新。
5. 任一步失败时没有公开的 Release、也没有标签：
   - 外部原因（网络、GitHub 故障）：在 Actions 里对这次运行点「Re-run failed jobs」，上一次留下的草稿会先删掉。
   - 要改代码或更新日志：在 `dev` 上修好，再提一个 PR 合并。新的推送读到的版本号仍没有标签，照常发版，说明取自
     那次提交里的 `CHANGELOG.md`。

没改版本号的 PR 合并进 `main` 时，标签已在，不发版。

安装包没有 Apple 与 Windows 的代码签名与公证（需要开发者账号，到时在 `desktop.yml` 里加）；防篡改靠上面的 minisign
签名。

## 手动打包

想在发版之外拿到安装包：Actions → desktop → Run workflow，选分支后运行，安装包在这次运行的产物里。本机打包用
`make package`，产物在 `target/release`。
