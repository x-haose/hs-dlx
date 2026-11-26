//! 编到应用发布的平台时，读 make runtime 备好的随应用带的 Python 与 QuickJS-NG（third_party/runtime/dist），把两个文件
//! 的完整路径与各项的 id、可执行文件、Python 的 ABI 交给编译（见 main.rs 的 `bundle`），并设 `bundled_runtime`；没备好、
//! 备的不是这个目标或不是当前的 scripts/runtime.py 备的时构建失败，不发出用不了网页视频的程序。编到 Windows 时另把 app.rc（应用图标）编进程序，编不了资源（缺资源编译器）时
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
    // CARGO_MANIFEST_DIR 是绝对路径；不 canonicalize：Windows 上它给出 \\?\ 开头的路径，再拼上 / 就打不开
    let root = Path::new(&std::env::var("CARGO_MANIFEST_DIR").expect("cargo 给 build.rs 设"))
        .join("../..");
    let dist = root.join("third_party/runtime/dist");
    let script = root.join("scripts/runtime.py");
    let marker = dist.join(".hs-dlx-runtime");
    let env_file = dist.join("bundle.env");
    for path in [&script, &marker, &env_file] {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    let read = |path: &Path| {
        std::fs::read(path).unwrap_or_else(|error| {
            panic!("读不出 {}（先运行 make runtime）：{error}", path.display())
        })
    };
    if read(&marker) != read(&script) {
        panic!(
            "{} 不是当前的 {} 备的：先运行 make runtime",
            dist.display(),
            script.display()
        );
    }
    let text = String::from_utf8(read(&env_file)).expect("bundle.env 由 runtime.py 写成 UTF-8");
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
    let python = dist.join("python.tar.zst");
    let quickjs = dist.join(get("QUICKJS_FILE"));
    for path in [&python, &quickjs] {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    // include_bytes! 直接用这两个完整路径（Path::join 拼的，各平台的分隔符都对）
    println!(
        "cargo:rustc-env=HS_DLX_RUNTIME_PYTHON_PATH={}",
        python.display()
    );
    println!(
        "cargo:rustc-env=HS_DLX_RUNTIME_QUICKJS_PATH={}",
        quickjs.display()
    );
    for key in [
        "PYTHON_ID",
        "PYTHON_EXECUTABLE",
        "PYTHON_ABI",
        "QUICKJS_ID",
        "QUICKJS_FILE",
    ] {
        println!("cargo:rustc-env=HS_DLX_RUNTIME_{key}={}", get(key));
    }
    println!("cargo:rustc-cfg=bundled_runtime");
}
