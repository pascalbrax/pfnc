//! `ArchiveFs`: a read-only `pfnc_core::Vfs` implementation layered over
//! `.tar` / `.tar.zst` / `.zip` files, browsable exactly like a directory
//! tree. Works over any base `Vfs` (local or SFTP) by first materializing
//! the whole archive into a local anonymous temp file — see `open()`.

mod format;
mod index;
mod tar_backend;
mod zip_backend;

pub use format::{detect_format, ArchiveFormat};

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use index::ArchiveEntry;
use pfnc_core::{EntryMeta, Vfs, VfsCapabilities, VfsError, VfsPath, VfsResult};

fn io_err(e: std::io::Error) -> VfsError {
    VfsError::Io(e)
}

pub struct ArchiveFs {
    // Kept open for the lifetime of `ArchiveFs`; each read clones the fd
    // (`File::try_clone`) so tar/zip parsing gets its own independent seek
    // position without needing a path to reopen by (the temp file this
    // wraps has none — it's anonymous and cleaned up automatically on
    // close).
    file: File,
    format: ArchiveFormat,
    entries: Vec<ArchiveEntry>,
}

impl ArchiveFs {
    /// Downloads `archive_path` from `base` into a local anonymous temp
    /// file and indexes its contents. Blocking (does real I/O, potentially
    /// over the network) — callers run this on a background job thread.
    pub fn open(base: &dyn Vfs, archive_path: &VfsPath, format: ArchiveFormat) -> VfsResult<Self> {
        let mut reader = base.open_read(archive_path)?;
        let mut file = tempfile::tempfile().map_err(io_err)?;
        std::io::copy(&mut reader, &mut file).map_err(io_err)?;
        file.seek(SeekFrom::Start(0)).map_err(io_err)?;

        let entries = match format {
            ArchiveFormat::Tar => tar_backend::index(file.try_clone().map_err(io_err)?)?,
            ArchiveFormat::TarZst => tar_backend::index_zst(file.try_clone().map_err(io_err)?)?,
            ArchiveFormat::Zip => zip_backend::index(file.try_clone().map_err(io_err)?)?,
        };

        // `File::try_clone` duplicates the fd but shares the underlying
        // seek offset (POSIX dup() semantics) — indexing just read the
        // clone through to EOF, which leaves `file` itself at EOF too.
        // Reset before storing it so the first `open_read` doesn't start
        // from the end of the file.
        file.seek(SeekFrom::Start(0)).map_err(io_err)?;

        Ok(Self { file, format, entries })
    }

    fn find(&self, path: &VfsPath) -> VfsResult<&ArchiveEntry> {
        self.entries
            .iter()
            .find(|e| &e.path == path)
            .ok_or_else(|| VfsError::NotFound(path.clone()))
    }
}

impl std::fmt::Debug for ArchiveFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveFs")
            .field("format", &self.format)
            .field("entry_count", &self.entries.len())
            .finish()
    }
}

fn unsupported<T>() -> VfsResult<T> {
    Err(VfsError::Unsupported("archives are read-only"))
}

impl Vfs for ArchiveFs {
    fn list_dir(&self, path: &VfsPath) -> VfsResult<Vec<EntryMeta>> {
        if path != "/" {
            self.find(path)?;
        }
        let out = self
            .entries
            .iter()
            .filter(|e| {
                let Some(parent) = e.path.parent() else { return false };
                let parent_normalized = if parent.as_str().is_empty() { "/" } else { parent.as_str() };
                parent_normalized == path.as_str()
            })
            .map(|e| EntryMeta {
                name: e.path.file_name().unwrap_or_default().to_string(),
                path: e.path.clone(),
                kind: e.kind.clone(),
                size: e.size,
                modified: e.modified,
                permissions: e.permissions,
                owner: None,
                group: None,
            })
            .collect();
        Ok(out)
    }

    fn stat(&self, path: &VfsPath) -> VfsResult<EntryMeta> {
        if path == "/" {
            return Ok(EntryMeta {
                name: "/".to_string(),
                path: path.clone(),
                kind: pfnc_core::EntryKind::Dir,
                size: 0,
                modified: None,
                permissions: None,
                owner: None,
                group: None,
            });
        }
        let e = self.find(path)?;
        Ok(EntryMeta {
            name: e.path.file_name().unwrap_or_default().to_string(),
            path: e.path.clone(),
            kind: e.kind.clone(),
            size: e.size,
            modified: e.modified,
            permissions: e.permissions,
            owner: None,
            group: None,
        })
    }

    fn open_read(&self, path: &VfsPath) -> VfsResult<Box<dyn Read + Send>> {
        // Same shared-offset caveat as in `open`: a previous `open_read`
        // (or the initial indexing pass) may have left the underlying
        // position anywhere, including EOF, since clones of `self.file`
        // share one seek position. Always reset before reading.
        let mut clone = self.file.try_clone().map_err(io_err)?;
        clone.seek(SeekFrom::Start(0)).map_err(io_err)?;
        match self.format {
            ArchiveFormat::Tar => tar_backend::extract(clone, path),
            ArchiveFormat::TarZst => tar_backend::extract_zst(clone, path),
            ArchiveFormat::Zip => zip_backend::extract(clone, path),
        }
    }

    fn create_write(&self, _path: &VfsPath, _mode: Option<u32>) -> VfsResult<Box<dyn Write + Send>> {
        unsupported()
    }

    fn mkdir(&self, _path: &VfsPath, _mode: Option<u32>) -> VfsResult<()> {
        unsupported()
    }

    fn remove_file(&self, _path: &VfsPath) -> VfsResult<()> {
        unsupported()
    }

    fn remove_dir(&self, _path: &VfsPath, _recursive: bool) -> VfsResult<()> {
        unsupported()
    }

    fn rename(&self, _from: &VfsPath, _to: &VfsPath) -> VfsResult<()> {
        unsupported()
    }

    fn set_permissions(&self, _path: &VfsPath, _mode: u32) -> VfsResult<()> {
        unsupported()
    }

    fn symlink(&self, _target: &VfsPath, _link: &VfsPath) -> VfsResult<()> {
        unsupported()
    }

    fn capabilities(&self) -> VfsCapabilities {
        VfsCapabilities {
            can_write: false,
            can_set_permissions: false,
            can_symlink: false,
            can_rename: false,
        }
    }

    fn root(&self) -> VfsPath {
        VfsPath::from("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pfnc_vfs_local::LocalFs;
    use tempfile::tempdir;

    fn make_tar(dir: &std::path::Path) -> VfsPath {
        let tar_path = dir.join("sample.tar");
        let file = File::create(&tar_path).unwrap();
        let mut builder = tar::Builder::new(file);
        let mut header = tar::Header::new_gnu();
        let data = b"hello from tar";
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, "dir/file.txt", &data[..]).unwrap();
        builder.finish().unwrap();
        camino::Utf8PathBuf::from_path_buf(tar_path).unwrap()
    }

    fn make_tar_zst(dir: &std::path::Path) -> VfsPath {
        let path = dir.join("sample.tar.zst");
        let file = File::create(&path).unwrap();
        let encoder = zstd::Encoder::new(file, 0).unwrap().auto_finish();
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        let data = b"hello from tar.zst";
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, "compressed.txt", &data[..]).unwrap();
        builder.finish().unwrap();
        camino::Utf8PathBuf::from_path_buf(path).unwrap()
    }

    fn make_zip(dir: &std::path::Path) -> VfsPath {
        let zip_path = dir.join("sample.zip");
        let file = File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("dir/file.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"hello from zip").unwrap();
        zip.finish().unwrap();
        camino::Utf8PathBuf::from_path_buf(zip_path).unwrap()
    }

    #[test]
    fn browses_and_extracts_tar_contents() {
        let dir = tempdir().unwrap();
        let tar_path = make_tar(dir.path());
        let local = LocalFs::new();

        let archive = ArchiveFs::open(&local, &tar_path, ArchiveFormat::Tar).unwrap();
        let root = archive.list_dir(&VfsPath::from("/")).unwrap();
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].name, "dir");
        assert!(root[0].kind.is_dir());

        let inner = archive.list_dir(&VfsPath::from("/dir")).unwrap();
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0].name, "file.txt");
        assert_eq!(inner[0].size, 14);

        let mut content = Vec::new();
        archive
            .open_read(&VfsPath::from("/dir/file.txt"))
            .unwrap()
            .read_to_end(&mut content)
            .unwrap();
        assert_eq!(content, b"hello from tar");
    }

    #[test]
    fn browses_and_extracts_tar_zst_contents() {
        let dir = tempdir().unwrap();
        let path = make_tar_zst(dir.path());
        let local = LocalFs::new();

        let archive = ArchiveFs::open(&local, &path, ArchiveFormat::TarZst).unwrap();
        let root = archive.list_dir(&VfsPath::from("/")).unwrap();
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].name, "compressed.txt");

        let mut content = Vec::new();
        archive
            .open_read(&VfsPath::from("/compressed.txt"))
            .unwrap()
            .read_to_end(&mut content)
            .unwrap();
        assert_eq!(content, b"hello from tar.zst");
    }

    #[test]
    fn browses_and_extracts_zip_contents() {
        let dir = tempdir().unwrap();
        let zip_path = make_zip(dir.path());
        let local = LocalFs::new();

        let archive = ArchiveFs::open(&local, &zip_path, ArchiveFormat::Zip).unwrap();
        let inner = archive.list_dir(&VfsPath::from("/dir")).unwrap();
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0].name, "file.txt");

        let mut content = Vec::new();
        archive
            .open_read(&VfsPath::from("/dir/file.txt"))
            .unwrap()
            .read_to_end(&mut content)
            .unwrap();
        assert_eq!(content, b"hello from zip");
    }

    #[test]
    fn write_operations_are_rejected() {
        let dir = tempdir().unwrap();
        let tar_path = make_tar(dir.path());
        let local = LocalFs::new();
        let archive = ArchiveFs::open(&local, &tar_path, ArchiveFormat::Tar).unwrap();

        assert!(archive.mkdir(&VfsPath::from("/new"), None).is_err());
        assert!(!archive.capabilities().can_write);
    }

    #[test]
    fn stat_on_missing_path_is_not_found() {
        let dir = tempdir().unwrap();
        let tar_path = make_tar(dir.path());
        let local = LocalFs::new();
        let archive = ArchiveFs::open(&local, &tar_path, ArchiveFormat::Tar).unwrap();

        let err = archive.stat(&VfsPath::from("/nope")).unwrap_err();
        assert!(matches!(err, VfsError::NotFound(_)));
    }
}
