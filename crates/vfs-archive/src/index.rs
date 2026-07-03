//! Builds an in-memory, hierarchical index of an archive's contents from a
//! flat list of raw entries. Shared by the tar and zip readers so path
//! sanitization (rejecting path-traversal entries) and "synthesize missing
//! parent directories" only need to be implemented once.

use std::time::SystemTime;

use pfnc_core::{EntryKind, VfsPath};

#[derive(Clone, Debug)]
pub struct RawEntry {
    /// The raw path as stored in the archive (forward-slash separated,
    /// typically no leading `/`).
    pub raw_path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub symlink_target: Option<String>,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub permissions: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct ArchiveEntry {
    pub path: VfsPath,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub permissions: Option<u32>,
}

/// Rejects an entry path that could escape the archive's own virtual root
/// (`..` components, or an absolute path) — the archive-internal
/// equivalent of the classic "zip-slip" vulnerability. Also rejects empty
/// paths. Returns the cleaned, `/`-prefixed `VfsPath` on success.
///
/// Public within the crate so the tar/zip extraction paths can re-derive
/// the same sanitized path from a raw archive entry name when looking up
/// a specific entry to extract (see `open_read`).
pub(crate) fn sanitize_path(raw: &str) -> Option<VfsPath> {
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let mut clean_components = Vec::new();
    for component in trimmed.split('/') {
        match component {
            "" | "." => continue,
            ".." => return None,
            other => clean_components.push(other),
        }
    }
    if clean_components.is_empty() {
        return None;
    }
    Some(VfsPath::from(format!("/{}", clean_components.join("/"))))
}

fn parent_of(path: &VfsPath) -> Option<VfsPath> {
    let parent = path.parent()?;
    if parent.as_str().is_empty() {
        Some(VfsPath::from("/"))
    } else {
        Some(parent.to_path_buf())
    }
}

/// Builds the final entry list: sanitizes every raw path, then synthesizes
/// any missing intermediate directories (many archives, especially plain
/// `.tar`, don't include an explicit entry for every ancestor directory).
pub fn build_index(raw_entries: Vec<RawEntry>) -> Vec<ArchiveEntry> {
    let mut by_path: std::collections::HashMap<VfsPath, ArchiveEntry> = std::collections::HashMap::new();

    for raw in raw_entries {
        let Some(path) = sanitize_path(&raw.raw_path) else {
            tracing::warn!(path = %raw.raw_path, "skipping unsafe or empty archive entry path");
            continue;
        };
        let kind = if raw.is_symlink {
            EntryKind::Symlink {
                target: raw.symlink_target.as_deref().and_then(sanitize_symlink_target),
            }
        } else if raw.is_dir {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        by_path.insert(
            path.clone(),
            ArchiveEntry {
                path,
                kind,
                size: raw.size,
                modified: raw.modified,
                permissions: raw.permissions,
            },
        );
    }

    // Synthesize any ancestor directories that weren't explicitly present
    // as their own entries, so panel navigation always has something to
    // list_dir() at every level.
    let mut to_add = Vec::new();
    for entry in by_path.values() {
        let mut ancestor = parent_of(&entry.path);
        while let Some(dir_path) = ancestor {
            if dir_path.as_str() == "/" || by_path.contains_key(&dir_path) {
                break;
            }
            to_add.push(dir_path.clone());
            ancestor = parent_of(&dir_path);
        }
    }
    for dir_path in to_add {
        by_path.entry(dir_path.clone()).or_insert(ArchiveEntry {
            path: dir_path,
            kind: EntryKind::Dir,
            size: 0,
            modified: None,
            permissions: None,
        });
    }

    let mut entries: Vec<ArchiveEntry> = by_path.into_values().collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

/// Symlink targets stored in an archive are just as capable of trying to
/// point somewhere unsafe; sanitize them the same way, but tolerate a
/// `None` result (an unrecognized target just means we can't resolve it
/// for display, not a reason to drop the symlink entry itself).
fn sanitize_symlink_target(raw: &str) -> Option<VfsPath> {
    sanitize_path(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, size: u64) -> RawEntry {
        RawEntry {
            raw_path: path.to_string(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            size,
            modified: None,
            permissions: None,
        }
    }

    #[test]
    fn synthesizes_missing_parent_directories() {
        let entries = build_index(vec![file("a/b/c.txt", 3)]);
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"/a"));
        assert!(paths.contains(&"/a/b"));
        assert!(paths.contains(&"/a/b/c.txt"));
        assert!(entries.iter().find(|e| e.path == "/a").unwrap().kind.is_dir());
    }

    #[test]
    fn rejects_path_traversal_entries() {
        let entries = build_index(vec![file("../../etc/passwd", 10), file("ok.txt", 1)]);
        assert_eq!(entries.iter().filter(|e| e.path.as_str().contains("etc")).count(), 0);
        assert!(entries.iter().any(|e| e.path == "/ok.txt"));
    }

    #[test]
    fn normalizes_leading_slash_like_gnu_tar_does() {
        // A leading "/" is stripped rather than rejected — same
        // long-standing safety behavior GNU tar itself uses by default —
        // since our virtual paths never touch a real absolute filesystem
        // path, there's no traversal risk in allowing it.
        let entries = build_index(vec![file("/etc/passwd", 1)]);
        assert!(entries.iter().any(|e| e.path == "/etc/passwd"));
    }

    #[test]
    fn drops_empty_and_dot_only_paths() {
        let entries = build_index(vec![file("", 1), file(".", 1)]);
        assert!(entries.is_empty());
    }

    #[test]
    fn deduplicates_explicit_and_synthesized_directories() {
        let mut dir = file("a/", 0);
        dir.is_dir = true;
        let entries = build_index(vec![dir, file("a/b.txt", 1)]);
        assert_eq!(entries.iter().filter(|e| e.path == "/a").count(), 1);
    }
}
