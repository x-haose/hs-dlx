//! 组件包的核对与解包（阻塞的文件读写，调用方放在阻塞线程上）。

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::Error;
use crate::pins::Archive;

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

/// 把组件包 `path` 解进 `into`（已存在的空目录）。条目的路径越出 `into`（绝对路径、`..`）时报 [`Error::Archive`]，
/// 解了一半的留在 `into` 里由调用方删。
pub(crate) fn unpack(path: &Path, archive: Archive, into: &Path) -> Result<(), Error> {
    let file = File::open(path).map_err(|error| Error::disk(path, &error))?;
    let broken =
        |error: &dyn std::fmt::Display| Error::Archive(format!("{}：{error}", path.display()));
    match archive {
        Archive::TarGz => {
            let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
            tar.set_preserve_permissions(true);
            for entry in tar.entries().map_err(|error| broken(&error))? {
                let mut entry = entry.map_err(|error| broken(&error))?;
                // unpack_in 拒绝越出 `into` 的条目时返回 false
                if !entry.unpack_in(into).map_err(|error| broken(&error))? {
                    let name = entry.path().map_err(|error| broken(&error))?.into_owned();
                    return Err(broken(&format!("条目 {} 越出了解包目录", name.display())));
                }
            }
            Ok(())
        }
        Archive::Zip => {
            let mut zip = zip::ZipArchive::new(file).map_err(|error| broken(&error))?;
            for index in 0..zip.len() {
                let mut entry = zip.by_index(index).map_err(|error| broken(&error))?;
                let Some(relative) = entry.enclosed_name() else {
                    let name = entry
                        .name()
                        .map(|name| name.into_owned())
                        .unwrap_or_default();
                    return Err(broken(&format!("条目 {name:?} 越出了解包目录")));
                };
                let target = into.join(relative);
                if entry.is_dir() {
                    std::fs::create_dir_all(&target)
                        .map_err(|error| Error::disk(&target, &error))?;
                    continue;
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| Error::disk(parent, &error))?;
                }
                let mut out =
                    File::create(&target).map_err(|error| Error::disk(&target, &error))?;
                io::copy(&mut entry, &mut out).map_err(|error| broken(&error))?;
                #[cfg(unix)]
                if let Some(mode) = entry.unix_mode() {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(
                        &target,
                        std::fs::Permissions::from_mode(mode & 0o777),
                    )
                    .map_err(|error| Error::disk(&target, &error))?;
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

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

    #[test]
    fn zip_entries_unpack_with_their_mode_and_escapes_are_refused() {
        let dir = dir("zip");
        let path = dir.join("a.zip");
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o755);
        zip.start_file("bin/tool", options).unwrap();
        zip.write_all(b"#!/bin/sh\n").unwrap();
        zip.finish().unwrap();
        let into = dir.join("out");
        std::fs::create_dir(&into).unwrap();
        unpack(&path, Archive::Zip, &into).unwrap();
        assert_eq!(
            std::fs::read(into.join("bin/tool")).unwrap(),
            b"#!/bin/sh\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(into.join("bin/tool"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }

        let evil = dir.join("evil.zip");
        let mut zip = zip::ZipWriter::new(File::create(&evil).unwrap());
        zip.start_file("../escaped", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.finish().unwrap();
        let into = dir.join("evil-out");
        std::fs::create_dir(&into).unwrap();
        assert!(matches!(
            unpack(&evil, Archive::Zip, &into),
            Err(Error::Archive(_))
        ));
        assert!(!dir.join("escaped").exists());
    }

    #[test]
    fn tar_gz_entries_unpack_and_escapes_are_refused() {
        let dir = dir("tar");
        let build = |path: &Path, name: &str| {
            let gz = flate2::write::GzEncoder::new(
                File::create(path).unwrap(),
                flate2::Compression::fast(),
            );
            let mut tar = tar::Builder::new(gz);
            let mut header = tar::Header::new_gnu();
            header.set_size(2);
            header.set_mode(0o644);
            // set_path 拒绝 `..`，越界的名字直接写进头里
            header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
            header.set_cksum();
            tar.append(&header, &b"ok"[..]).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        };
        let good = dir.join("good.tar.gz");
        build(&good, "python/bin/python3");
        let into = dir.join("out");
        std::fs::create_dir(&into).unwrap();
        unpack(&good, Archive::TarGz, &into).unwrap();
        assert_eq!(
            std::fs::read(into.join("python/bin/python3")).unwrap(),
            b"ok"
        );

        let evil = dir.join("evil.tar.gz");
        build(&evil, "../escaped");
        let into = dir.join("evil-out");
        std::fs::create_dir(&into).unwrap();
        assert!(matches!(
            unpack(&evil, Archive::TarGz, &into),
            Err(Error::Archive(_))
        ));
        assert!(!dir.join("escaped").exists());
    }
}
