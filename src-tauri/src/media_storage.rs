use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub(crate) async fn with_deadline<T>(
    duration: std::time::Duration,
    work: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::time::timeout(duration, work)
        .await
        .map_err(|_| "媒体加载超时，已停止本次下载；请稍后手动重试".to_owned())?
}

/// Only session-specific hard links, never original archive paths, are granted
/// to the WebView. Revocation therefore does not permanently forbid the archive.
#[derive(Default)]
pub(crate) struct PreviewRegistry {
    files: std::collections::HashMap<PathBuf, PathBuf>,
}

impl PreviewRegistry {
    pub(crate) fn expose(
        &mut self,
        original: &Path,
        root: &Path,
        allow: impl FnOnce(&Path) -> Result<(), String>,
    ) -> Result<PathBuf, String> {
        if let Some(path) = self.files.get(original) {
            return Ok(path.clone());
        }
        if !original.symlink_metadata().is_ok_and(|m| m.is_file()) {
            return Err("本地媒体文件无效".into());
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let extension = original
            .extension()
            .and_then(|s| s.to_str())
            .ok_or("本地媒体格式无效")?;
        let path = root.join(format!("{}-{nonce}.{extension}", std::process::id()));
        fs::hard_link(original, &path).map_err(|_| "无法创建当前会话的本地预览".to_owned())?;
        if let Err(error) = allow(&path) {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        self.files.insert(original.to_owned(), path.clone());
        Ok(path)
    }

    pub(crate) fn revoke(
        &mut self,
        mut forbid: impl FnMut(&Path) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut failure = None;
        self.files.retain(|_, path| {
            if let Err(error) = forbid(path) {
                failure = Some(error);
                return true;
            }
            if let Err(error) = fs::remove_file(&*path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    failure = Some("预览权限已撤销，但临时预览清理失败".into());
                }
            }
            false
        });
        failure.map_or(Ok(()), Err)
    }
}

pub(crate) fn valid_mp4(prefix: &[u8], received: usize) -> bool {
    let Some(size) = prefix.get(..4) else {
        return false;
    };
    let size = u32::from_be_bytes(size.try_into().unwrap()) as usize;
    size >= 16
        && size <= received
        && prefix.get(4..8) == Some(b"ftyp")
        && prefix.get(8..12).is_some_and(|brand| {
            [
                b"isom", b"iso2", b"iso5", b"iso6", b"mp41", b"mp42", b"avc1", b"M4V ", b"qt  ",
                b"dash", b"MSNV",
            ]
            .contains(&brand.try_into().unwrap())
        })
}

pub(crate) fn existing_video(path: &Path, maximum: usize) -> bool {
    let Ok(metadata) = path.symlink_metadata() else {
        return false;
    };
    if !metadata.is_file() || metadata.len() > maximum as u64 {
        return false;
    }
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut prefix = [0; 32];
    let Ok(read) = file.read(&mut prefix) else {
        return false;
    };
    valid_mp4(&prefix[..read], metadata.len() as usize)
}

pub(crate) fn remove_matching_files(
    directory: &Path,
    matches: impl Fn(&str) -> bool,
) -> Result<(), String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("无法读取待清理的媒体目录".into()),
    };
    for entry in entries {
        let entry = entry.map_err(|_| "无法读取待清理的媒体文件")?;
        let kind = entry.file_type().map_err(|_| "无法读取媒体文件类型")?;
        if (kind.is_file() || kind.is_symlink()) && entry.file_name().to_str().is_some_and(&matches)
        {
            fs::remove_file(entry.path()).map_err(|_| "媒体文件清理失败，请检查目录权限")?;
        }
    }
    Ok(())
}

pub(crate) struct TemporaryMedia {
    path: PathBuf,
    file: Option<File>,
}

impl TemporaryMedia {
    pub(crate) fn create(path: PathBuf) -> Result<Self, String> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .map_err(|_| "无法创建临时媒体文件，请检查空间和目录权限".to_owned())?;
        Ok(Self {
            path,
            file: Some(file),
        })
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.file
            .as_mut()
            .ok_or("临时媒体已关闭")?
            .write_all(bytes)
            .map_err(|_| "写入媒体失败，请检查磁盘空间".to_owned())
    }

    pub(crate) fn commit(mut self, target: &Path) -> Result<(), String> {
        self.file
            .as_mut()
            .ok_or("临时媒体已关闭")?
            .sync_all()
            .map_err(|_| "同步媒体文件失败".to_owned())?;
        self.file.take();
        fs::rename(&self.path, target).map_err(|_| "保存媒体文件失败".to_owned())?;
        Ok(())
    }
}

impl Drop for TemporaryMedia {
    fn drop(&mut self) {
        self.file.take();
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_deadline_stops_pending_work() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let result = tokio::time::timeout(
                    std::time::Duration::from_millis(50),
                    with_deadline(
                        std::time::Duration::from_millis(1),
                        std::future::pending::<Result<(), String>>(),
                    ),
                )
                .await;
                assert!(matches!(result, Ok(Err(message)) if message.contains("超时")));
            });
    }

    #[test]
    fn cleanup_only_removes_matching_media_files() {
        let root = std::env::temp_dir().join(format!("qzone-cleanup-test-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        for name in ["10001-12.mp4", "10002-12.mp4", "100010-12.mp4"] {
            fs::write(root.join(name), b"synthetic").unwrap();
        }
        fs::create_dir(root.join("10001-directory")).unwrap();
        remove_matching_files(&root, |name| name.starts_with("10001-")).unwrap();
        assert!(!root.join("10001-12.mp4").exists());
        assert!(root.join("10002-12.mp4").exists());
        assert!(root.join("100010-12.mp4").exists());
        assert!(root.join("10001-directory").is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_previews_are_revocable_without_deleting_originals() {
        let root = std::env::temp_dir().join(format!(
            "qzone-preview-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let source = root.join("synthetic.jpg");
        fs::write(&source, b"synthetic-original").unwrap();
        let mut registry = PreviewRegistry::default();
        let alias = registry.expose(&source, &root, |_| Ok(())).unwrap();
        assert_ne!(alias, source);
        let mut denied = Vec::new();
        registry
            .revoke(|p| {
                denied.push(p.to_owned());
                Ok(())
            })
            .unwrap();
        assert_eq!(denied, vec![alias.clone()]);
        assert!(!alias.exists());
        assert_eq!(fs::read(&source).unwrap(), b"synthetic-original");
        let next = registry.expose(&source, &root, |_| Ok(())).unwrap();
        assert_ne!(alias, next);
        registry.revoke(|_| Ok(())).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_commit_preserves_destination_and_removes_partial() {
        let root = std::env::temp_dir().join(format!(
            "qzone-commit-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let target = root.join("destination-directory");
        fs::create_dir(&target).unwrap();
        let part = root.join("synthetic.part");
        let mut temporary = TemporaryMedia::create(part.clone()).unwrap();
        temporary.write(b"synthetic").unwrap();
        assert!(temporary.commit(&target).is_err());
        assert!(!part.exists());
        assert!(target.is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn core_fix_rejects_images_html_and_truncated_mp4_as_video() {
        assert!(!valid_mp4(b"<!doctype html>video", 2000));
        assert!(!valid_mp4(b"\0\0\0\x18ftypavif\0\0\0\0", 2048));
        assert!(!valid_mp4(b"\0\0\0\x18ftypisom\0\0\0\0", 16));
        assert!(valid_mp4(b"\0\0\0\x18ftypisom\0\0\0\0", 2048));
    }

    #[test]
    fn core_fix_cancelled_media_removes_only_its_temporary_file() {
        let path = std::env::temp_dir().join(format!(
            "qzone-media-test-{}-{}.part",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut temporary = TemporaryMedia::create(path.clone()).unwrap();
        temporary.write(b"synthetic").unwrap();
        assert!(path.exists());
        drop(temporary);
        let exists = path.exists();
        if exists {
            fs::remove_file(&path).unwrap();
        }
        assert!(!exists);
    }
}
