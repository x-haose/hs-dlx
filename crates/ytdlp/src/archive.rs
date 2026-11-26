//! 下载的核对与随应用带的 Python 的解包（阻塞的文件读写，调用方放在阻塞线程上）。

use std::fs::File;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::Error;

/// `bytes` 的 SHA-256，小写十六进制。
pub(crate) fn sha256_bytes(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 文件 `path` 的 SHA-256，小写十六进制。
pub(crate) fn sha256_of(path: &Path) -> Result<String, Error> {
    let mut file = File::open(path).map_err(|error| Error::disk(path, &error))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| Error::disk(path, &error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

/// 把 tar 经 zstd 压缩的 `bytes`（随应用带的 Python）解进 `into`（已存在的空目录），保留可执行权限。条目的路径越出
/// `into`（绝对路径、`..`）时报 [`Error::Archive`]，解了一半的留在 `into` 里由调用方删。
pub(crate) fn unpack_tar_zst(bytes: &[u8], into: &Path) -> Result<(), Error> {
    let broken =
        |error: &dyn std::fmt::Display| Error::Archive(format!("随应用带的 Python：{error}"));
    // 写文件出错（磁盘满、没有权限、被杀毒软件拦下）是本机的事，链里有系统的错误码；别的是包的内容不对
    let failed = |error: std::io::Error| {
        let (described, os) = chain(&error);
        if os {
            Error::Disk(format!(
                "解出随应用带的 Python 到 {}：{described}",
                into.display()
            ))
        } else {
            broken(&described)
        }
    };
    let decoder = ruzstd::decoding::StreamingDecoder::new(bytes).map_err(|error| broken(&error))?;
    let mut tar = tar::Archive::new(decoder);
    tar.set_preserve_permissions(true);
    for entry in tar.entries().map_err(failed)? {
        let mut entry = entry.map_err(failed)?;
        // unpack_in 拒绝越出 `into` 的条目时返回 false
        if !entry.unpack_in(into).map_err(failed)? {
            let name = entry.path().map_err(|error| broken(&error))?.into_owned();
            return Err(broken(&format!("条目 {} 越出了解包目录", name.display())));
        }
    }
    Ok(())
}

/// `error` 连同它包着的各层错误的说明（以「：」接起来），与其中有没有一层是带系统错误码的 `io::Error`。tar 把写文件的
/// 错误包成自己的错误再包进 `io::Error`：最外层没有错误码，说明里也没有系统的原因。
fn chain(error: &std::io::Error) -> (String, bool) {
    let mut described = error.to_string();
    let mut os = error.raw_os_error().is_some();
    // io::Error 的 source() 跳过它包着的那一层（直接给那一层的 source），所以先取 get_ref
    let mut next: Option<&(dyn std::error::Error + 'static)> = error
        .get_ref()
        .map(|inner| inner as &(dyn std::error::Error + 'static));
    while let Some(inner) = next {
        let text = inner.to_string();
        if !described.ends_with(&text) {
            described = format!("{described}：{text}");
        }
        let io = inner.downcast_ref::<std::io::Error>();
        os |= io.is_some_and(|io| io.raw_os_error().is_some());
        next = match io {
            Some(io) => io
                .get_ref()
                .map(|inner| inner as &(dyn std::error::Error + 'static)),
            None => inner.source(),
        };
    }
    (described, os)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hs-dlx-ytdlp-archive-{name}-{}",
            std::process::id()
        ));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sha256_matches_the_known_digest() {
        let dir = dir("sha");
        let path = dir.join("abc");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_of(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// 一个条目 `name`（内容 `ok`、权限 0o755）的 tar 经 zstd 压缩。
    fn tar_zst(name: &str) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(2);
        header.set_mode(0o755);
        // set_path 拒绝 `..`，越界的名字直接写进头里
        header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
        header.set_cksum();
        tar.append(&header, &b"ok"[..]).unwrap();
        let tar = tar.into_inner().unwrap();
        ruzstd::encoding::compress_to_vec(
            tar.as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        )
    }

    #[test]
    fn tar_zst_entries_unpack_with_their_mode_and_escapes_are_refused() {
        let dir = dir("tar");
        let into = dir.join("out");
        std::fs::create_dir(&into).unwrap();
        unpack_tar_zst(&tar_zst("python/bin/python3"), &into).unwrap();
        assert_eq!(
            std::fs::read(into.join("python/bin/python3")).unwrap(),
            b"ok"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(into.join("python/bin/python3"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }

        let into = dir.join("evil-out");
        std::fs::create_dir(&into).unwrap();
        assert!(matches!(
            unpack_tar_zst(&tar_zst("../escaped"), &into),
            Err(Error::Archive(_))
        ));
        assert!(!dir.join("escaped").exists());
        assert!(matches!(
            unpack_tar_zst(b"not zstd", &into),
            Err(Error::Archive(_))
        ));
    }

    /// 写不进去（这里是只读目录）是本机的事：报磁盘，说明里有系统给的原因。
    #[cfg(unix)]
    #[test]
    fn a_write_that_the_system_refuses_is_a_disk_error_with_its_reason() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = dir("readonly");
        let into = dir.join("out");
        std::fs::create_dir(&into).unwrap();
        std::fs::set_permissions(&into, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = unpack_tar_zst(&tar_zst("python/bin/python3"), &into);
        std::fs::set_permissions(&into, std::fs::Permissions::from_mode(0o755)).unwrap();
        match result {
            Err(Error::Disk(message)) => assert!(
                message.contains(&std::io::Error::from_raw_os_error(13).to_string()),
                "{message}"
            ),
            other => panic!("{other:?}"),
        }
    }
}
