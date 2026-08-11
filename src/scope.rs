use std::path::Path;

/// Resolve the project scope for a working directory.
///
/// Walks up looking for a `.git` entry and uses that directory's name. Falls
/// back to the working directory's own name, then to `"default"`. Never fails —
/// a missing scope must not block a write.
pub fn detect_scope(cwd: &Path) -> String {
    let mut current = Some(cwd);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            if let Some(name) = dir.file_name() {
                return name.to_string_lossy().to_string();
            }
        }
        current = dir.parent();
    }

    cwd.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "default".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mem8-scope-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn uses_git_root_basename_when_git_present() {
        let root = temp_dir("gitroot");
        fs::create_dir_all(root.join(".git")).unwrap();
        let nested = root.join("src").join("deep");
        fs::create_dir_all(&nested).unwrap();

        let expected = root.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(detect_scope(&nested), expected);
    }

    #[test]
    fn falls_back_to_cwd_basename_without_git() {
        let dir = temp_dir("nogit");
        let expected = dir.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(detect_scope(&dir), expected);
    }

    #[test]
    fn falls_back_to_default_for_root_path() {
        let root = std::path::Path::new("/");
        assert_eq!(detect_scope(root), "default");
    }
}
