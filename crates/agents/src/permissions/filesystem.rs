use std::path::{Path, PathBuf};

/// Sensitive file patterns that should be flagged.
const SENSITIVE_PATTERNS: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    "credentials.json",
    "credentials.yaml",
    "credentials.yml",
    ".aws/credentials",
    ".ssh/id_rsa",
    ".ssh/id_ed25519",
    "id_rsa",
    "id_ed25519",
    ".npmrc",
    ".pypirc",
];

/// Validates filesystem paths against allowed directories.
pub struct FilesystemValidator {
    allowed_dirs: Vec<PathBuf>,
    read_only_dirs: Vec<PathBuf>,
}

impl FilesystemValidator {
    pub fn new(working_dir: &str) -> Self {
        let working = PathBuf::from(working_dir);
        let home = dirs_home();

        let mut allowed_dirs = vec![
            working.clone(),
            std::env::temp_dir(),
        ];

        if let Some(home) = &home {
            allowed_dirs.push(home.join(".codeany"));
        }

        Self {
            allowed_dirs,
            read_only_dirs: Vec::new(),
        }
    }

    pub fn add_allowed_dir(&mut self, dir: PathBuf) {
        self.allowed_dirs.push(dir);
    }

    pub fn add_read_only_dir(&mut self, dir: PathBuf) {
        self.read_only_dirs.push(dir);
    }

    /// Check if a path is allowed for the given operation.
    pub fn validate_path(&self, path: &str, write: bool) -> Result<(), String> {
        let path = resolve_path(path);

        // Check read-only dirs
        if write {
            for ro_dir in &self.read_only_dirs {
                if path.starts_with(ro_dir) {
                    return Err(format!(
                        "Path {} is in a read-only directory",
                        path.display()
                    ));
                }
            }
        }

        // Check allowed dirs
        let allowed = self.allowed_dirs.iter().any(|dir| path.starts_with(dir));
        if !allowed {
            return Err(format!(
                "Path {} is outside allowed directories",
                path.display()
            ));
        }

        Ok(())
    }

    /// Check if a path is sensitive (credentials, keys, etc.).
    pub fn is_sensitive_path(path: &str) -> bool {
        let path_lower = path.to_lowercase();
        SENSITIVE_PATTERNS
            .iter()
            .any(|pattern| path_lower.ends_with(pattern) || path_lower.contains(pattern))
    }
}

fn resolve_path(path: &str) -> PathBuf {
    let p = Path::new(path);
    if let Ok(canonical) = p.canonicalize() {
        canonical
    } else {
        p.to_path_buf()
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
}
