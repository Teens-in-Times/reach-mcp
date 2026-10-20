use crate::auth::Principal;
use anyhow::Result;
use serde_json::json;
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
    sync::Mutex,
    time::Instant,
};

/// No commands, paths, arguments, file contents or secrets are recorded.
pub struct Audit {
    file: Mutex<File>,
}
impl Audit {
    pub fn event(&self, action: &str, target: &str) -> Result<()> {
        let row = json!({"at":crate::auth::now(),"principal":"local-admin","action":action,"target":target});
        let mut file = self.file.lock().expect("audit lock");
        writeln!(file, "{row}")?;
        file.flush()?;
        Ok(())
    }
    pub fn open(state: &Path) -> Result<Self> {
        std::fs::create_dir_all(state)?;
        Ok(Self {
            file: Mutex::new(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(state.join("audit.jsonl"))?,
            ),
        })
    }
    pub fn record(&self, p: &Principal, tool: &str, started: Instant, ok: bool) -> Result<()> {
        let row = json!({"at":crate::auth::now(),"principal":p.id,"credential_id":p.credential_id,"tool":tool,"ok":ok,"duration_ms":started.elapsed().as_millis()});
        let mut file = self.file.lock().expect("audit lock");
        writeln!(file, "{row}")?;
        file.flush()?;
        Ok(())
    }
}
