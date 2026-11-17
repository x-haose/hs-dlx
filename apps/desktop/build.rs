//! 编到应用发布的平台时，读 make runtime 备好的随应用带的 Python 与 QuickJS-NG（third_party/runtime/dist），把各项的
//! id、可执行文件与所在目录交给编译（见 main.rs 的 `bundle`），并设 `bundled_runtime`；没备好或备的不是这个目标时构建
//! 失败，不发出用不了网页视频的程序。编到 Windows 时另把 app.rc（应用图标）编进程序，编不了资源（缺资源编译器）时
//! 构建失败，不发出没有图标的程序。

use std::collections::HashMap;
use std::path::Path;

/// 应用发布的平台（与 scripts/runtime.py 的 TARGETS 一致）。
const RELEASED: [&str; 3] = [
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
];

fn main() {
    println!("cargo:rustc-check-cfg=cfg(bundled_runtime)");
    runtime();
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=assets/icons/icon.ico");
    if let Err(error) = embed_resource::compile("app.rc", embed_resource::NONE).manifest_required()
    {
        panic!("{error}");
    }
}

fn runtime() {
    let target = std::env::var("TARGET").expect("cargo 给 build.rs 设 TARGET");
    if !RELEASED.contains(&target.as_str()) {
        return;
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo 给 build.rs 设");
    let dist = Path::new(&manifest_dir).join("../../third_party/runtime/dist");
    let env_file = dist.join("bundle.env");
    println!("cargo:rerun-if-changed={}", env_file.display());
    let text = std::fs::read_to_string(&env_file).unwrap_or_else(|error| {
        panic!(
            "读不出 {}（先运行 make runtime）：{error}",
            env_file.display()
        )
    });
    let values: HashMap<&str, &str> = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    let get = |key: &str| {
        *values
            .get(key)
            .unwrap_or_else(|| panic!("{} 里没有 {key}（先运行 make runtime）", env_file.display()))
    };
    if get("TARGET") != target {
        panic!(
            "{} 是为 {} 备的，现在编的是 {target}：先运行 make runtime",
            dist.display(),
            get("TARGET")
        );
    }
    let dist = dist
        .canonicalize()
        .unwrap_or_else(|error| panic!("{}：{error}", dist.display()));
    for file in ["python.tar.zst", get("QUICKJS_FILE")] {
        println!("cargo:rerun-if-changed={}", dist.join(file).display());
    }
    println!("cargo:rustc-env=HS_DLX_RUNTIME_DIR={}", dist.display());
    for key in [
        "PYTHON_ID",
        "PYTHON_EXECUTABLE",
        "QUICKJS_ID",
        "QUICKJS_FILE",
    ] {
        println!("cargo:rustc-env=HS_DLX_RUNTIME_{key}={}", get(key));
    }
    println!("cargo:rustc-cfg=bundled_runtime");
}
