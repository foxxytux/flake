use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub is_parent: bool,
}

pub fn read_entries(dir: impl AsRef<Path>, show_hidden: bool) -> Result<Vec<Entry>> {
    let dir = dir.as_ref();
    let mut entries = Vec::new();
    if let Some(parent) = dir.parent().filter(|parent| *parent != dir) {
        entries.push(Entry {
            path: parent.to_path_buf(),
            name: "..".to_string(),
            is_dir: true,
            is_parent: true,
        });
    }
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        entries.push(Entry {
            path,
            name,
            is_dir: metadata.is_dir(),
            is_parent: false,
        });
    }
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (_, _) if a.is_parent && !b.is_parent => std::cmp::Ordering::Less,
        (_, _) if !a.is_parent && b.is_parent => std::cmp::Ordering::Greater,
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::read_entries;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn read_entries_filters_hidden_and_sorts_directories_first() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock moved backwards")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("flake-fs-test-{}-{}", std::process::id(), unique));
        fs::create_dir_all(&dir).expect("create temp dir");

        let nested = dir.join("zeta");
        fs::create_dir_all(&nested).expect("create nested dir");
        fs::write(dir.join("beta.txt"), "beta").expect("write beta");
        fs::write(dir.join("alpha.txt"), "alpha").expect("write alpha");
        fs::write(dir.join(".hidden"), "hidden").expect("write hidden");

        let entries = read_entries(&dir, false).expect("read entries");
        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["..", "zeta", "alpha.txt", "beta.txt"]);
        assert!(entries[0].is_parent);
        assert!(entries[0].is_dir);
        assert!(entries[1].is_dir);
        assert!(!entries[2].is_dir);
        assert!(!entries[3].is_dir);

        fs::remove_dir_all(&dir).expect("cleanup temp dir");
    }
}
