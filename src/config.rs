use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, path::PathBuf};
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub auth_file: PathBuf,
    pub default_cwd: PathBuf,
    pub state_dir: PathBuf,
    pub ssh: Option<SshConfig>,
    #[serde(default = "rate")]
    pub requests_per_minute: u32,
    #[serde(default)]
    pub admin: Option<AdminConfig>,
    #[serde(default)]
    pub protected_paths: Vec<PathBuf>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
    pub listen: SocketAddr,
    pub tickets_file: PathBuf,
}
fn rate() -> u32 {
    240
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SshConfig {
    pub host: String,
    pub hostname: String,
    pub host_key_alias: String,
    pub remote_port: u16,
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        if let Some(admin) = &self.admin {
            ensure!(
                admin.listen.ip().is_loopback() && admin.listen != self.listen,
                "Admin must use a separate loopback listener"
            );
            ensure!(
                admin.tickets_file.is_absolute(),
                "Admin tickets_file must be absolute"
            );
        }
        ensure!(
            self.listen.ip().is_loopback(),
            "Listener must be loopback; use SSH or a separately authenticated TLS gateway"
        );
        ensure!(
            self.auth_file.is_absolute() && self.state_dir.is_absolute(),
            "auth_file and state_dir must be absolute"
        );
        ensure!(
            self.default_cwd.is_absolute() && self.default_cwd.is_dir(),
            "default_cwd must be an existing absolute directory"
        );
        ensure!(
            self.requests_per_minute > 0,
            "requests_per_minute must be positive"
        );
        crate::auth::Store::load(&self.auth_file)?;
        Ok(())
    }
}
