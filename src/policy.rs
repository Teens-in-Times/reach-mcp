use anyhow::{Context, Result, ensure};
use std::path::{Component, Path, PathBuf};

#[derive(Clone)]
pub struct Policy {
    roots: Vec<PathBuf>,
    denied: Vec<PathBuf>,
}
fn normalize(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "Filesystem path must be absolute");
    let mut clean = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                ensure!(clean.pop(), "Path escapes its root");
            }
            _ => clean.push(part.as_os_str()),
        }
    }
    let mut ancestor = clean.as_path();
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        suffix.push(
            ancestor
                .file_name()
                .context("Cannot resolve path")?
                .to_owned(),
        );
        ancestor = ancestor.parent().context("Missing path ancestor")?;
    }
    let mut result = std::fs::canonicalize(ancestor)?;
    for part in suffix.into_iter().rev() {
        result.push(part);
    }
    Ok(result)
}
fn key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        path.to_string_lossy().into_owned()
    }
}
fn within(path: &Path, root: &Path) -> bool {
    let path = key(path);
    let root = key(root);
    path == root
        || path.starts_with(
            &(root.trim_end_matches(['\\', '/']).to_string()
                + if cfg!(windows) { "\\" } else { "/" }),
        )
}
impl Policy {
    pub fn new(config: &crate::Config, p: &crate::auth::Principal) -> Result<Self> {
        let roots = if p.roots.is_empty() {
            vec![config.default_cwd.clone()]
        } else {
            p.roots.clone()
        };
        let roots = roots
            .iter()
            .map(|p| normalize(p))
            .collect::<Result<Vec<_>>>()?;
        let mut denied = vec![
            config.auth_file.clone(),
            config.state_dir.clone(),
            std::env::current_exe()?,
        ];
        denied.extend(config.protected_paths.clone());
        if let Some(admin) = &config.admin {
            denied.push(admin.tickets_file.clone());
        }
        Ok(Self {
            roots,
            denied: denied
                .iter()
                .map(|p| normalize(p))
                .collect::<Result<Vec<_>>>()?,
        })
    }
    pub fn check(&self, path: &Path) -> Result<()> {
        let actual = normalize(path)?;
        ensure!(
            self.allows_walk(&actual),
            "Path is outside granted workspaces or belongs to protected executor data"
        );
        Ok(())
    }
    pub fn allows_walk(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| within(path, root))
            && !self.denied.iter().any(|root| within(path, root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workspace_and_secret_paths_cannot_be_reached_by_parent_components() {
        let d = tempfile::tempdir().unwrap();
        let project = d.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let state = d.path().join("private");
        std::fs::create_dir(&state).unwrap();
        let auth = state.join("credentials.json");
        std::fs::write(&auth, b"{}").unwrap();
        let c = crate::Config {
            listen: "127.0.0.1:1".parse().unwrap(),
            auth_file: auth.clone(),
            default_cwd: project.clone(),
            state_dir: state,
            ssh: None,
            requests_per_minute: 240,
            admin: None,
            protected_paths: vec![],
        };
        let p = Policy::new(&c, &crate::auth::Principal::full("a")).unwrap();
        assert!(p.check(&project.join("new/file")).is_ok());
        assert!(
            p.check(&project.join("../private/credentials.json"))
                .is_err()
        );
        assert!(p.check(&auth).is_err());
    }
}
