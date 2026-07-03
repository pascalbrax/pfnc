use std::fs::File;
use std::io::Read;

use pfnc_core::{VfsError, VfsPath, VfsResult};

use crate::index::{build_index, sanitize_path, ArchiveEntry, RawEntry};

fn zip_err(e: zip::result::ZipError) -> VfsError {
    VfsError::Io(std::io::Error::other(e.to_string()))
}

fn io_err(e: std::io::Error) -> VfsError {
    VfsError::Io(e)
}

fn raw_entry_from(file: &zip::read::ZipFile) -> RawEntry {
    RawEntry {
        raw_path: file.name().to_string(),
        is_dir: file.is_dir(),
        is_symlink: false, // zip has no first-class symlink concept we rely on here
        symlink_target: None,
        size: file.size(),
        // `ZipFile::last_modified()` returns zip's own DateTime type; a
        // faithful conversion needs an extra crate feature, so — same
        // trade-off as LocalFs/SftpFs's owner/group — this is left
        // unpopulated for Phase 1 rather than pulling in more surface area
        // for a cosmetic field.
        modified: None,
        permissions: file.unix_mode(),
    }
}

pub fn index(file: File) -> VfsResult<Vec<ArchiveEntry>> {
    let mut archive = zip::ZipArchive::new(file).map_err(zip_err)?;
    let mut raw = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let entry = archive.by_index(i).map_err(zip_err)?;
        raw.push(raw_entry_from(&entry));
    }
    Ok(build_index(raw))
}

pub fn extract(file: File, target: &VfsPath) -> VfsResult<Box<dyn Read + Send>> {
    let mut archive = zip::ZipArchive::new(file).map_err(zip_err)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(zip_err)?;
        if entry.is_dir() {
            continue;
        }
        if sanitize_path(entry.name()).as_ref() != Some(target) {
            continue;
        }
        let mut temp = tempfile::tempfile().map_err(io_err)?;
        std::io::copy(&mut entry, &mut temp).map_err(io_err)?;
        std::io::Seek::seek(&mut temp, std::io::SeekFrom::Start(0)).map_err(io_err)?;
        return Ok(Box::new(temp));
    }
    Err(VfsError::NotFound(target.clone()))
}
