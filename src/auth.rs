//! Private-transport credentials. This is deliberately not an OAuth authorization server.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use uuid::Uuid;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("OS randomness failed: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[derive(Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    #[default]
    Compact,
    Native,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub principal: String,
    pub label: String,
    pub sha256: String,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub roots: Vec<std::path::PathBuf>,
    #[serde(default)]
    pub profile: Profile,
    pub created_at: u64,
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub revoked: bool,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Store {
    pub credentials: BTreeMap<String, Credential>,
}

#[derive(Clone, Debug)]
pub struct Principal {
    pub credential_id: String,
    pub id: String,
    pub scopes: Vec<String>,
    pub roots: Vec<std::path::PathBuf>,
    pub profile: Profile,
}

impl Principal {
    pub fn allows(&self, tool: &str) -> bool {
        let Some(scope) = crate::operation::Operation::scope(tool) else {
            return false;
        };
        if scope.is_empty() {
            return true;
        }
        self.scopes.iter().any(|s| s == scope || s == "*")
    }
    #[cfg(test)]
    pub fn full(id: &str) -> Self {
        Self {
            credential_id: id.into(),
            id: id.into(),
            scopes: vec!["*".into()],
            roots: vec![],
            profile: Profile::Native,
        }
    }
}

impl Store {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .context("Credential file needs a parent directory")?;
        fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.as_file().sync_all()?;
        file.persist(path).map_err(|e| e.error)?;
        Ok(())
    }
    pub fn issue(
        &mut self,
        principal: &str,
        label: &str,
        scopes: Vec<String>,
        profile: Profile,
        ttl: Option<u64>,
    ) -> Result<(String, String)> {
        ensure!(
            !principal.is_empty()
                && principal.len() <= 64
                && principal
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
            "principal must be 1-64 ASCII letters, digits, -, _ or ."
        );
        ensure!(
            !scopes.is_empty()
                && scopes
                    .iter()
                    .all(|s| ["*", "read", "write", "exec", "tasks"].contains(&s.as_str())),
            "Unknown or empty scopes"
        );
        ensure!(ttl != Some(0), "TTL must be positive");
        let id = Uuid::new_v4().simple().to_string();
        let token = format!("reach_{id}.{}", secret()?);
        let expires_at = ttl
            .map(|t| now().checked_add(t).context("TTL overflow"))
            .transpose()?;
        self.credentials.insert(
            id.clone(),
            Credential {
                principal: principal.into(),
                label: label.into(),
                sha256: crate::files::hash(token.as_bytes()),
                scopes,
                roots: vec![],
                profile,
                created_at: now(),
                expires_at,
                revoked: false,
            },
        );
        Ok((id, token))
    }
    pub fn authenticate(&self, token: &str) -> Option<Principal> {
        if token.len() != 103 {
            return None;
        }
        let (id, secret) = token.strip_prefix("reach_")?.split_once('.')?;
        if id.len() != 32
            || secret.len() != 64
            || !id
                .bytes()
                .chain(secret.bytes())
                .all(|b| b.is_ascii_hexdigit())
        {
            return None;
        }
        let c = self.credentials.get(id)?;
        let digest = crate::files::hash(token.as_bytes());
        if c.revoked
            || c.expires_at.is_some_and(|t| t <= now())
            || !bool::from(digest.as_bytes().ct_eq(c.sha256.as_bytes()))
        {
            return None;
        }
        Some(Principal {
            credential_id: id.into(),
            id: c.principal.clone(),
            scopes: c.scopes.clone(),
            roots: c.roots.clone(),
            profile: c.profile,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hashed_expiring_revocable_identity() {
        let mut s = Store::default();
        let (id, token) = s
            .issue(
                "tui-a",
                "A",
                vec!["read".into()],
                Profile::Compact,
                Some(60),
            )
            .unwrap();
        assert!(!serde_json::to_string(&s).unwrap().contains(&token));
        let p = s.authenticate(&token).unwrap();
        assert_eq!(p.id, "tui-a");
        assert!(p.allows("Read"));
        assert!(!p.allows("Run"));
        assert!(!p.allows("Shell"));
        assert!(s.authenticate(&(token.clone() + "x")).is_none());
        s.credentials.get_mut(&id).unwrap().expires_at = Some(now());
        assert!(s.authenticate(&token).is_none());
        s.credentials.get_mut(&id).unwrap().expires_at = None;
        s.credentials.get_mut(&id).unwrap().revoked = true;
        assert!(s.authenticate(&token).is_none());
    }
}
