.PHONY: check ffmpeg ffmpeg_present libtorrent libtorrent_present icons licenses package \
	rs_check rs_fmt rs_lint rs_clippy rs_doc rs_test rs_cov py_env py_check py_test

FFMPEG_DIST := third_party/ffmpeg/dist

# 仓库里 Python 脚本的开发环境：uv 把开发工具装进 .venv
export UV_PYTHON ?= 3.12

check: rs_check py_check py_test

# Rust 的全部检查；CI 按平台调用其中的子目标
rs_check: rs_fmt rs_lint rs_clippy rs_doc rs_test rs_cov

rs_fmt:
	cargo fmt --all --check

# 与平台无关的检查：依赖方向、压制属性与注释、依赖的安全公告与许可证、随安装包分发的声明是否最新
rs_lint: py_env
	scripts/check_deps.sh
	scripts/check_no_suppression.sh
	@cargo deny --version >/dev/null 2>&1 || { echo "缺少 cargo-deny，版本见 .github/workflows/check.yml"; exit 1; }
	cargo deny --locked check
	uv run --no-sync python scripts/licenses.py --check

# 依赖或 FFmpeg 版本变了之后重新生成随安装包分发的声明（apps/desktop/LICENSES）
licenses: py_env
	uv run --no-sync python scripts/licenses.py

# 桌面应用各尺寸的图标，由 apps/desktop/assets/icon.svg 渲出（要 librsvg 的 rsvg-convert）；改了 icon.svg 后运行。
# PNG 的文件名为点数，二倍屏的带 @2x（macOS 的 icns 按此认尺寸：32、128、256、512 点各有一倍与二倍）。icon.ico 是
# Windows 的（嵌进程序与安装程序，见 apps/desktop/build.rs），由 ICO_SIDES 各尺寸拼成，中间的 PNG 留在 target/icons
ICON_POINTS := 32 128 256 512
ICO_SIDES := 16 20 24 32 40 48 64 128 256
icons: py_env
	mkdir -p apps/desktop/assets/icons target/icons
	for points in $(ICON_POINTS); do \
		rsvg-convert -w $$points -h $$points apps/desktop/assets/icon.svg -o apps/desktop/assets/icons/$$points.png || exit 1; \
		rsvg-convert -w $$((points * 2)) -h $$((points * 2)) apps/desktop/assets/icon.svg \
			-o apps/desktop/assets/icons/$$points@2x.png || exit 1; \
	done
	for side in $(ICO_SIDES); do \
		rsvg-convert -w $$side -h $$side apps/desktop/assets/icon.svg -o target/icons/$$side.png || exit 1; \
	done
	uv run --no-sync python scripts/ico.py apps/desktop/assets/icons/icon.ico $(ICO_SIDES:%=target/icons/%.png)

# 桌面应用的安装包（配置见 apps/desktop/Cargo.toml 的 package.metadata.packager），放在 target/release。各平台的
# 格式与 .github/workflows/desktop.yml 的矩阵一致：Linux 只打 deb，不打 AppImage（它带上打包机器的 WebKitGTK，在别的
# 发行版上网页起不来）。cargo-packager 在哪个平台上都能打 .app 与 NSIS，所以格式要按平台指定
ifeq ($(OS),Windows_NT)
PACKAGE_FORMATS := nsis
else ifeq ($(shell uname -s),Darwin)
PACKAGE_FORMATS := app,dmg
else
PACKAGE_FORMATS := deb
endif
package: ffmpeg_present libtorrent_present runtime_present
	@cargo packager --version >/dev/null 2>&1 || { echo "缺少 cargo-packager，版本见 .github/workflows/desktop.yml"; exit 1; }
	cd apps/desktop && cargo packager --release --formats $(PACKAGE_FORMATS)

rs_clippy: ffmpeg_present libtorrent_present runtime_present
	cargo clippy --workspace --all-targets -- -D warnings

# 文档注释里的链接须都能解析（含私有项）：改名、删除后留下的断链在这里失败
rs_doc: ffmpeg_present libtorrent_present runtime_present
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items

rs_test: ffmpeg_present libtorrent_present runtime_present
	cargo test --workspace

# 任务存储与生命周期（app）、网盘的登录与接口（cloud）、yt-dlp 组件的核对安装与解析（ytdlp）的行覆盖各不低于 80%
rs_cov: ffmpeg_present libtorrent_present runtime_present
	@cargo llvm-cov --version >/dev/null 2>&1 || { echo "缺少 cargo-llvm-cov，版本见 .github/workflows/check.yml"; exit 1; }
	# 源码改动后旧的插桩产物会混进统计，先清掉
	cargo llvm-cov clean --workspace
	cargo llvm-cov -p hs-dlx-app --fail-under-lines 80 --summary-only
	cargo llvm-cov -p hs-dlx-cloud --fail-under-lines 80 --summary-only
	cargo llvm-cov -p hs-dlx-ytdlp --fail-under-lines 80 --summary-only

# FFmpeg 用 hs-m3u8（Cargo.lock 固定的提交）的构建脚本编译，组件与核心库出自同一个提交；源码包与编译的中间文件放在
# third_party/ffmpeg/build。打印脚本路径的命令：
FFMPEG_SCRIPT := uv run --no-sync python scripts/hs_m3u8.py
# 装好的 FFmpeg 须是那份脚本装的（标记文件是装它的脚本的副本）：升级 hs-m3u8 后它改了组件而本地没重新构建时在这里失败
FFMPEG_MARKER := $(FFMPEG_DIST)/.hs-m3u8-ffmpeg

ffmpeg_present: py_env
	@test -d $(FFMPEG_DIST)/include || { echo "缺少 $(FFMPEG_DIST)，先运行 make ffmpeg"; exit 1; }
	@script=$$($(FFMPEG_SCRIPT)) && { cmp -s $(FFMPEG_MARKER) "$$script" || \
		{ echo "$(FFMPEG_DIST) 不是 $$script 装的，先运行 make ffmpeg"; exit 1; }; }

ffmpeg: py_env
	script=$$($(FFMPEG_SCRIPT)) && bash "$$script" $(FFMPEG_DIST) third_party/ffmpeg/build

# BT 引擎（crates/torrent）静态链接的 libtorrent、OpenSSL 与 Boost 的头文件，由 third_party/libtorrent/build.sh 编译；
# 装好的须是当前那份脚本装的（标记文件是脚本的副本）：改了脚本而本地没重新构建时在这里失败
LIBTORRENT_DIST := third_party/libtorrent/dist
LIBTORRENT_SCRIPT := third_party/libtorrent/build.sh

libtorrent_present:
	@test -d $(LIBTORRENT_DIST)/include || { echo "缺少 $(LIBTORRENT_DIST)，先运行 make libtorrent"; exit 1; }
	@cmp -s $(LIBTORRENT_DIST)/.hs-dlx-libtorrent $(LIBTORRENT_SCRIPT) || \
		{ echo "$(LIBTORRENT_DIST) 不是当前的 $(LIBTORRENT_SCRIPT) 装的，先运行 make libtorrent"; exit 1; }

libtorrent:
	bash $(LIBTORRENT_SCRIPT)

# 随应用带的 Python 与 QuickJS-NG（网页视频用），由 scripts/runtime.py 下载、核对、裁剪、压缩，桌面应用编进程序；备好
# 的须是当前那份脚本备的（标记文件是脚本的副本）：换了版本或裁剪而本地没重新备时在这里失败
RUNTIME_DIST := third_party/runtime/dist
RUNTIME_SCRIPT := scripts/runtime.py

# 不是发布的平台（没有随带的运行时）时算备好了
runtime_present: py_env
	@uv run --no-sync python -I $(RUNTIME_SCRIPT) --present

runtime: py_env
	uv run --no-sync python -I $(RUNTIME_SCRIPT)

py_env:
	uv sync --locked

# 仓库里 Python 脚本的格式、检查与类型检查
py_check: py_env
	uv run --no-sync ruff format --check scripts tests crates/ytdlp/src
	uv run --no-sync ruff check scripts tests crates/ytdlp/src
	uv run --no-sync mypy
	uv run --no-sync basedpyright

py_test: py_env
	uv run --no-sync pytest
