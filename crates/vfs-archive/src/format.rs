use pfnc_core::VfsPath;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveFormat {
    Tar,
    TarZst,
    Zip,
}

/// Detects a recognized archive format from a filename, so callers (the
/// app's "Enter" handler) can decide whether to open a file as a directory
/// tree rather than editing/copying it as a plain file.
pub fn detect_format(path: &VfsPath) -> Option<ArchiveFormat> {
    let name = path.file_name()?.to_ascii_lowercase();
    if name.ends_with(".tar.zst") || name.ends_with(".tzst") {
        Some(ArchiveFormat::TarZst)
    } else if name.ends_with(".tar") {
        Some(ArchiveFormat::Tar)
    } else if name.ends_with(".zip") {
        Some(ArchiveFormat::Zip)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_known_extensions() {
        assert_eq!(detect_format(&VfsPath::from("/a/b.tar")), Some(ArchiveFormat::Tar));
        assert_eq!(detect_format(&VfsPath::from("/a/b.tar.zst")), Some(ArchiveFormat::TarZst));
        assert_eq!(detect_format(&VfsPath::from("/a/b.tzst")), Some(ArchiveFormat::TarZst));
        assert_eq!(detect_format(&VfsPath::from("/a/b.zip")), Some(ArchiveFormat::Zip));
        assert_eq!(detect_format(&VfsPath::from("/a/b.ZIP")), Some(ArchiveFormat::Zip));
        assert_eq!(detect_format(&VfsPath::from("/a/b.txt")), None);
    }
}
