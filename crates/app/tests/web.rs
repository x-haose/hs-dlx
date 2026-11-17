//! 网页视频的组件与解析接进服务：组件目录里摆好「装好的」组件（Python 与 yt-dlp 链到仓库开发环境 .venv 里的），
//! 服务一开就认得、能解析；没装时解析报还没装。组件的下载与安装见 crates/ytdlp 的测试。符号链接只在类 Unix 系统上建。
#![cfg(unix)]

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use hs_dlx_app::web::Preference;
use hs_dlx_app::ytdlp::{Archive, ExtractError, Pinned, Pins, Sources};
use hs_dlx_app::{Error, Service, Store, TorrentNetwork, WebConfig, WebState};
use hs_m3u8_core::Url;

fn test_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("web")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn pins() -> Pins {
    let pinned = |executable: &str| Pinned {
        version: "test".to_owned(),
        url: "https://example.invalid/package".to_owned(),
        sha256: "0".repeat(64),
        archive: Archive::TarGz,
        executable: executable.to_owned(),
    };
    Pins {
        python: pinned("python/bin/python3"),
        deno: pinned("deno"),
    }
}

/// 在 `root` 下摆好装好的组件：Python 与 yt-dlp 链到开发环境的。
fn lay_out(root: &Path) {
    let venv = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.venv");
    let python = venv
        .join("bin/python")
        .canonicalize()
        .expect("先运行 uv sync");
    let site = std::fs::read_dir(venv.join("lib"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("python3")
        })
        .unwrap()
        .join("site-packages");
    std::fs::create_dir_all(root.join("python-test/python/bin")).unwrap();
    std::os::unix::fs::symlink(python, root.join("python-test/python/bin/python3")).unwrap();
    std::fs::create_dir_all(root.join("deno-test")).unwrap();
    std::fs::write(root.join("deno-test/deno"), "").unwrap();
    std::os::unix::fs::symlink(site, root.join("yt-dlp-dev")).unwrap();
    std::fs::write(
        root.join("state.json"),
        r#"{"yt_dlp":"dev","previous":null}"#,
    )
    .unwrap();
}

fn service(dir: &Path, root: PathBuf) -> Service {
    let (service, _events) = Service::new(
        Store::open(&dir.join("tasks.sqlite")).unwrap(),
        NonZeroUsize::new(4).unwrap(),
        TorrentNetwork::Loopback {
            public_trackers: Vec::new(),
        },
        hs_dlx_app::pikpak::Endpoints::official(),
        WebConfig {
            root,
            pins: Some(pins()),
            sources: Sources::official(),
        },
    )
    .unwrap();
    service.set_proxy(&hs_m3u8_core::Proxy::Direct).unwrap();
    service
}

#[test]
fn installed_components_are_ready_and_resolve_through_the_driver() {
    let dir = test_dir("ready");
    let root = dir.join("components");
    lay_out(&root);
    let service = service(&dir, root);
    assert_eq!(
        *service.web_state().borrow(),
        WebState::Ready("dev".to_owned())
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    // 只有通用解析器认得的地址：驱动脚本不发请求即回不支持（转嗅探页）
    let handle = service
        .resolve(
            Url::parse("http://127.0.0.1:9/page").unwrap(),
            Vec::new(),
            Preference::Compatible,
        )
        .unwrap();
    let result = runtime.block_on(handle).unwrap();
    assert!(
        matches!(result, Err(ExtractError::Unsupported(_))),
        "{result:?}"
    );
}

#[test]
fn resolving_before_the_components_are_installed_is_refused() {
    let dir = test_dir("missing");
    let service = service(&dir, dir.join("components"));
    assert_eq!(*service.web_state().borrow(), WebState::Missing);
    let refused = service.resolve(
        Url::parse("https://www.youtube.com/watch?v=x").unwrap(),
        Vec::new(),
        Preference::Compatible,
    );
    assert!(matches!(refused, Err(Error::WebNotReady)));
}
