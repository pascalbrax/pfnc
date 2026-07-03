//! `LocalFs`: a `pfnc_core::Vfs` implementation backed by `std::fs`.
//!
//! Operates directly on absolute filesystem paths (no chroot); `root()` is
//! simply `/`.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

use camino::Utf8PathBuf;
use pfnc_core::{EntryKind, EntryMeta, Vfs, VfsCapabilities, VfsError, VfsPath, VfsResult};

#[derive(Debug, Default, Clone, Copy)]
pub struct LocalFs;

impl LocalFs {
    pub fn new() -> Self {
        LocalFs
    }
}

fn map_io_err(e: std::io::Error, path: &VfsPath) -> VfsError {
    use std::io::ErrorKind::*;
    match e.kind() {
        NotFound => VfsError::NotFound(path.clone()),
        PermissionDenied => VfsError::PermissionDenied(path.clone()),
        AlreadyExists => VfsError::AlreadyExists(path.clone()),
        _ => VfsError::Io(e),
    }
}

fn entry_meta_from_std(path: &VfsPath, name: String, meta: &fs::Metadata) -> EntryMeta {
    let file_type = meta.file_type();
    let kind = if file_type.is_symlink() {
        let target = fs::read_link(path.as_std_path())
            .ok()
            .and_then(|t| Utf8PathBuf::from_path_buf(t).ok());
        EntryKind::Symlink { target }
    } else if file_type.is_dir() {
        EntryKind::Dir
    } else if file_type.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };

    EntryMeta {
        name,
        path: path.clone(),
        kind,
        size: meta.len(),
        modified: meta.modified().ok(),
        permissions: Some(meta.mode() & 0o7777),
        // Resolving uid/gid to names needs an /etc/passwd lookup crate not
        // yet in the dependency set; deferred past Phase 1 M1.
        owner: None,
        group: None,
    }
}

impl Vfs for LocalFs {
    fn list_dir(&self, path: &VfsPath) -> VfsResult<Vec<EntryMeta>> {
        let rd = fs::read_dir(path.as_std_path()).map_err(|e| map_io_err(e, path))?;
        let mut out = Vec::new();
        for entry in rd {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(%path, error = %e, "skipping unreadable directory entry");
                    continue;
                }
            };
            let name = match entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => {
                    tracing::warn!(%path, "skipping non-UTF8 filename");
                    continue;
                }
            };
            let full = path.join(&name);
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(path = %full, error = %e, "skipping entry: metadata failed");
                    continue;
                }
            };
            out.push(entry_meta_from_std(&full, name, &meta));
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn stat(&self, path: &VfsPath) -> VfsResult<EntryMeta> {
        let meta = fs::symlink_metadata(path.as_std_path()).map_err(|e| map_io_err(e, path))?;
        let name = path.file_name().unwrap_or("/").to_string();
        Ok(entry_meta_from_std(path, name, &meta))
    }

    fn open_read(&self, path: &VfsPath) -> VfsResult<Box<dyn Read + Send>> {
        let f = File::open(path.as_std_path()).map_err(|e| map_io_err(e, path))?;
        Ok(Box::new(f))
    }

    fn create_write(&self, path: &VfsPath, mode: Option<u32>) -> VfsResult<Box<dyn Write + Send>> {
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        if let Some(m) = mode {
            opts.mode(m);
        }
        let f = opts.open(path.as_std_path()).map_err(|e| map_io_err(e, path))?;
        Ok(Box::new(f))
    }

    fn mkdir(&self, path: &VfsPath, mode: Option<u32>) -> VfsResult<()> {
        fs::create_dir_all(path.as_std_path()).map_err(|e| map_io_err(e, path))?;
        if let Some(m) = mode {
            fs::set_permissions(path.as_std_path(), fs::Permissions::from_mode(m))
                .map_err(|e| map_io_err(e, path))?;
        }
        Ok(())
    }

    fn remove_file(&self, path: &VfsPath) -> VfsResult<()> {
        fs::remove_file(path.as_std_path()).map_err(|e| map_io_err(e, path))
    }

    fn remove_dir(&self, path: &VfsPath, recursive: bool) -> VfsResult<()> {
        let result = if recursive {
            fs::remove_dir_all(path.as_std_path())
        } else {
            fs::remove_dir(path.as_std_path())
        };
        result.map_err(|e| map_io_err(e, path))
    }

    fn rename(&self, from: &VfsPath, to: &VfsPath) -> VfsResult<()> {
        fs::rename(from.as_std_path(), to.as_std_path()).map_err(|e| map_io_err(e, from))
    }

    fn set_permissions(&self, path: &VfsPath, mode: u32) -> VfsResult<()> {
        fs::set_permissions(path.as_std_path(), fs::Permissions::from_mode(mode))
            .map_err(|e| map_io_err(e, path))
    }

    fn symlink(&self, target: &VfsPath, link: &VfsPath) -> VfsResult<()> {
        std::os::unix::fs::symlink(target.as_std_path(), link.as_std_path())
            .map_err(|e| map_io_err(e, link))
    }

    fn capabilities(&self) -> VfsCapabilities {
        VfsCapabilities {
            can_write: true,
            can_set_permissions: true,
            can_symlink: true,
            can_rename: true,
        }
    }

    fn root(&self) -> VfsPath {
        VfsPath::from("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn vfs_path(p: &std::path::Path) -> VfsPath {
        Utf8PathBuf::from_path_buf(p.to_path_buf()).expect("tempdir path must be UTF-8")
    }

    #[test]
    fn list_dir_and_stat() {
        let dir = tempdir().unwrap();
        let root = vfs_path(dir.path());

        fs::write(dir.path().join("file.txt"), b"hello").unwrap();
        fs::create_dir(dir.path().join("subdir")).unwrap();

        let fs_impl = LocalFs::new();
        let mut entries = fs_impl.list_dir(&root).unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "file.txt");
        assert_eq!(entries[0].kind, EntryKind::File);
        assert_eq!(entries[0].size, 5);
        assert_eq!(entries[1].name, "subdir");
        assert!(entries[1].kind.is_dir());

        let stat = fs_impl.stat(&root.join("file.txt")).unwrap();
        assert_eq!(stat.name, "file.txt");
        assert_eq!(stat.size, 5);
    }

    #[test]
    fn write_then_read_roundtrip() {
        let dir = tempdir().unwrap();
        let root = vfs_path(dir.path());
        let file_path = root.join("out.bin");

        let fs_impl = LocalFs::new();
        {
            let mut w = fs_impl.create_write(&file_path, None).unwrap();
            w.write_all(b"some bytes").unwrap();
        }
        let mut r = fs_impl.open_read(&file_path).unwrap();
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"some bytes");
    }

    #[test]
    fn mkdir_rename_remove() {
        let dir = tempdir().unwrap();
        let root = vfs_path(dir.path());
        let fs_impl = LocalFs::new();

        let new_dir = root.join("a/b/c");
        fs_impl.mkdir(&new_dir, None).unwrap();
        assert!(new_dir.as_std_path().is_dir());

        let renamed = root.join("a/b/renamed");
        fs_impl.rename(&new_dir, &renamed).unwrap();
        assert!(!new_dir.as_std_path().exists());
        assert!(renamed.as_std_path().is_dir());

        fs_impl.remove_dir(&renamed, false).unwrap();
        assert!(!renamed.as_std_path().exists());
    }

    #[test]
    fn remove_dir_non_empty_requires_recursive() {
        let dir = tempdir().unwrap();
        let root = vfs_path(dir.path());
        let fs_impl = LocalFs::new();

        let sub = root.join("sub");
        fs_impl.mkdir(&sub, None).unwrap();
        fs::write(sub.join("f").as_std_path(), b"x").unwrap();

        assert!(fs_impl.remove_dir(&sub, false).is_err());
        fs_impl.remove_dir(&sub, true).unwrap();
        assert!(!sub.as_std_path().exists());
    }

    #[test]
    fn symlink_reports_target() {
        let dir = tempdir().unwrap();
        let root = vfs_path(dir.path());
        let fs_impl = LocalFs::new();

        let target = root.join("target.txt");
        fs::write(target.as_std_path(), b"x").unwrap();
        let link = root.join("link.txt");
        fs_impl.symlink(&target, &link).unwrap();

        let meta = fs_impl.stat(&link).unwrap();
        match meta.kind {
            EntryKind::Symlink { target: Some(t) } => assert_eq!(t, target),
            other => panic!("expected symlink with target, got {other:?}"),
        }
    }

    #[test]
    fn remove_file_and_not_found_error() {
        let dir = tempdir().unwrap();
        let root = vfs_path(dir.path());
        let fs_impl = LocalFs::new();

        let f = root.join("gone.txt");
        fs::write(f.as_std_path(), b"x").unwrap();
        fs_impl.remove_file(&f).unwrap();

        let err = fs_impl.stat(&f).unwrap_err();
        assert!(matches!(err, VfsError::NotFound(_)));
    }
}
