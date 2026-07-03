use std::fs::File;
use std::io::Read;
use std::time::{Duration, UNIX_EPOCH};

use pfnc_core::{VfsError, VfsPath, VfsResult};

use crate::index::{build_index, sanitize_path, ArchiveEntry, RawEntry};

fn io_err(e: std::io::Error) -> VfsError {
    VfsError::Io(e)
}

fn raw_entry_from<R: Read>(entry: &tar::Entry<'_, R>) -> Option<RawEntry> {
    let path = entry.path().ok()?.to_string_lossy().into_owned();
    let header = entry.header();
    let entry_type = header.entry_type();
    let symlink_target = if entry_type.is_symlink() {
        header
            .link_name()
            .ok()
            .flatten()
            .map(|p| p.to_string_lossy().into_owned())
    } else {
        None
    };
    Some(RawEntry {
        raw_path: path,
        is_dir: entry_type.is_dir(),
        is_symlink: entry_type.is_symlink(),
        symlink_target,
        size: header.size().unwrap_or(0),
        modified: header.mtime().ok().map(|t| UNIX_EPOCH + Duration::from_secs(t)),
        permissions: header.mode().ok(),
    })
}

fn read_all_entries<R: Read>(reader: R) -> VfsResult<Vec<RawEntry>> {
    let mut archive = tar::Archive::new(reader);
    let mut raw = Vec::new();
    for entry in archive.entries().map_err(io_err)? {
        let entry = entry.map_err(io_err)?;
        if let Some(re) = raw_entry_from(&entry) {
            raw.push(re);
        }
    }
    Ok(raw)
}

pub fn index(file: File) -> VfsResult<Vec<ArchiveEntry>> {
    Ok(build_index(read_all_entries(file)?))
}

pub fn index_zst(file: File) -> VfsResult<Vec<ArchiveEntry>> {
    let decoder = zstd::Decoder::new(file).map_err(io_err)?;
    Ok(build_index(read_all_entries(decoder)?))
}

fn extract_from<R: Read>(reader: R, target: &VfsPath) -> VfsResult<Box<dyn Read + Send>> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().map_err(io_err)? {
        let mut entry = entry.map_err(io_err)?;
        let Some(raw) = raw_entry_from(&entry) else { continue };
        if raw.is_dir {
            continue;
        }
        if sanitize_path(&raw.raw_path).as_ref() != Some(target) {
            continue;
        }
        let mut temp = tempfile::tempfile().map_err(io_err)?;
        std::io::copy(&mut entry, &mut temp).map_err(io_err)?;
        std::io::Seek::seek(&mut temp, std::io::SeekFrom::Start(0)).map_err(io_err)?;
        return Ok(Box::new(temp));
    }
    Err(VfsError::NotFound(target.clone()))
}

pub fn extract(file: File, target: &VfsPath) -> VfsResult<Box<dyn Read + Send>> {
    extract_from(file, target)
}

pub fn extract_zst(file: File, target: &VfsPath) -> VfsResult<Box<dyn Read + Send>> {
    let decoder = zstd::Decoder::new(file).map_err(io_err)?;
    extract_from(decoder, target)
}
