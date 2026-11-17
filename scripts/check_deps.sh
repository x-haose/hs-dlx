#!/usr/bin/env bash
# 依赖方向检查：各 crate 的正常依赖与构建依赖（全部目标平台）中不得出现
# 所列的包。测试依赖不在此列。命中时打印依赖路径并以 1 退出。
set -euo pipefail

# 每行：<包> <不得依赖的包>...，含传递依赖
rules=(
  "hs-dlx-app gpui-kit gpui-pre wry hs-dlx-webview hs-dlx-instance hs-dlx-macos hs-dlx-relay"
  # 桌面应用的平台 crate 互不依赖，只由 hs-dlx 组合
  "hs-dlx-webview hs-m3u8-core hs-dlx-app hs-dlx-instance hs-dlx-macos hs-dlx-relay hs-dlx-torrent"
  "hs-dlx-instance hs-m3u8-core hs-dlx-app gpui-kit gpui-pre wry hs-dlx-webview hs-dlx-macos hs-dlx-relay hs-dlx-torrent"
  "hs-dlx-macos hs-m3u8-core hs-dlx-app gpui-kit gpui-pre wry hs-dlx-webview hs-dlx-instance hs-dlx-relay hs-dlx-torrent"
  "hs-dlx-relay hs-m3u8-core hs-dlx-app gpui-kit gpui-pre wry hs-dlx-webview hs-dlx-instance hs-dlx-macos hs-dlx-torrent"
  # BT 引擎只包一层 libtorrent，不知道任务、界面与 HLS
  "hs-dlx-torrent hs-m3u8-core hs-dlx-app gpui-kit gpui-pre wry hs-dlx-webview hs-dlx-instance hs-dlx-macos hs-dlx-relay"
  # 云离线只管与网盘的接口（经 core 发请求），不知道任务、存储与界面
  "hs-dlx-cloud hs-dlx-app gpui-kit gpui-pre wry hs-dlx-webview hs-dlx-instance hs-dlx-macos hs-dlx-relay hs-dlx-torrent rusqlite"
  # yt-dlp 只管组件与解析（经 core 下载），不知道任务、存储与界面
  "hs-dlx-ytdlp hs-dlx-app gpui-kit gpui-pre wry hs-dlx-webview hs-dlx-instance hs-dlx-macos hs-dlx-relay hs-dlx-torrent hs-dlx-cloud rusqlite"
)

# 每行：<包> <不得直接依赖的包>...：只经由 core 的再导出（core::hls、core::remux）使用
direct_rules=(
  "hs-dlx-app hs-m3u8-hls hs-m3u8-remux hs-m3u8-sysproxy"
  "hs-dlx hs-m3u8-hls hs-m3u8-remux hs-m3u8-sysproxy"
)

tree() {
  cargo tree --locked -e normal,build --target all "$@"
}

status=0
for rule in "${rules[@]}"; do
  read -r -a words <<< "$rule"
  package=${words[0]}
  deps=$(tree -p "$package" --prefix none --format '{p}' | awk '{print $1}' | sort -u)
  for name in "${words[@]:1}"; do
    if grep -qx -- "$name" <<< "$deps"; then
      echo "${package} 不得依赖 ${name}，依赖路径：" >&2
      tree -p "$package" -i "$name" >&2
      status=1
    fi
  done
done
for rule in "${direct_rules[@]}"; do
  read -r -a words <<< "$rule"
  package=${words[0]}
  deps=$(tree -p "$package" --depth 1 --prefix none --format '{p}' | awk '{print $1}' | sort -u)
  for name in "${words[@]:1}"; do
    if grep -qx -- "$name" <<< "$deps"; then
      echo "${package} 不得直接依赖 ${name}，经由 hs-m3u8-core 的再导出使用" >&2
      status=1
    fi
  done
done
exit "$status"
