use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use reach_mcp::{
    Config,
    audit::Audit,
    auth::{Profile, Store},
    engine::Hand,
    jobs::Jobs,
};
use std::{io::Write, path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;
#[derive(Parser)]
#[command(version, about = "A standalone Rust remote hand for MCP clients")]
struct Args {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    Serve {
        #[arg(long)]
        background: bool,
    },
    Init {
        #[arg(long)]
        directory: PathBuf,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long, default_value = "127.0.0.1:42373")]
        listen: std::net::SocketAddr,
        #[arg(long, default_value = "127.0.0.1:42374")]
        admin_listen: std::net::SocketAddr,
        #[arg(long)]
        no_admin: bool,
    },
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    Check,
    Ui {
        #[arg(long)]
        no_open: bool,
    },
    Catalog {
        #[arg(long, value_enum, default_value = "compact")]
        profile: ProfileArg,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum ProfileArg {
    Compact,
    Native,
}
impl From<ProfileArg> for Profile {
    fn from(p: ProfileArg) -> Self {
        match p {
            ProfileArg::Compact => Profile::Compact,
            ProfileArg::Native => Profile::Native,
        }
    }
}
#[derive(Subcommand)]
enum TokenCommand {
    Issue {
        #[arg(long)]
        principal: String,
        #[arg(long, default_value = "MCP client")]
        label: String,
        #[arg(long, value_delimiter = ',', default_value = "read,write,exec,tasks")]
        scopes: Vec<String>,
        #[arg(long, value_enum, default_value = "compact")]
        profile: ProfileArg,
        #[arg(long, default_value_t = 720)]
        ttl_hours: u64,
        #[arg(long)]
        token_file: Option<PathBuf>,
        #[arg(long = "root")]
        roots: Vec<PathBuf>,
    },
    List,
    Revoke {
        id: String,
    },
}
fn secure_directory(path: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let user = std::env::var("USERNAME")?;
        let status = std::process::Command::new("icacls.exe")
            .arg(path)
            .args([
                "/inheritance:r",
                "/grant:r",
                &format!("{user}:(OI)(CI)F"),
                "SYSTEM:(OI)(CI)F",
            ])
            .creation_flags(0x08000000)
            .stdout(std::process::Stdio::null())
            .status()?;
        anyhow::ensure!(status.success(), "Could not protect private directory");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn secure_file(path: &std::path::Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let user = std::env::var("USERNAME")?;
        let status = std::process::Command::new("icacls.exe")
            .arg(path)
            .args([
                "/inheritance:r",
                "/grant:r",
                &format!("{user}:F"),
                "SYSTEM:F",
            ])
            .creation_flags(0x08000000)
            .stdout(std::process::Stdio::null())
            .status()?;
        anyhow::ensure!(status.success(), "Could not protect token file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let command = args.command.unwrap_or(Command::Serve { background: false });
    if let Command::Init {
        directory,
        cwd,
        listen,
        admin_listen,
        no_admin,
    } = command
    {
        anyhow::ensure!(
            directory.is_absolute() && !directory.exists(),
            "directory must be an absolute new private directory"
        );
        secure_directory(&directory)?;
        anyhow::ensure!(
            cwd.is_absolute() && cwd.is_dir(),
            "cwd must be an existing absolute directory"
        );
        let config_file = directory.join("config.json");
        anyhow::ensure!(!config_file.exists(), "Configuration already exists");
        let auth = directory.join("credentials.json");
        anyhow::ensure!(!auth.exists(), "Credential store already exists");
        Store::default().save(&auth)?;
        let c = Config {
            listen,
            auth_file: auth,
            default_cwd: cwd,
            state_dir: directory.join("state"),
            ssh: None,
            requests_per_minute: 240,
            admin: if no_admin {
                None
            } else {
                Some(reach_mcp::config::AdminConfig {
                    listen: admin_listen,
                    tickets_file: directory.join("login-tickets.json"),
                })
            },
            protected_paths: vec![directory],
        };
        if let Some(admin) = &c.admin {
            reach_mcp::admin::prepare(&admin.tickets_file)?;
        }
        c.validate()?;
        std::fs::write(&config_file, serde_json::to_vec_pretty(&c)?)?;
        println!(
            "Initialized {}. Issue a credential before serving.",
            config_file.display()
        );
        return Ok(());
    }
    if let Command::Catalog { profile } = command {
        let p = reach_mcp::auth::Principal {
            credential_id: "catalog".into(),
            id: "catalog".into(),
            scopes: vec!["*".into()],
            roots: vec![],
            profile: profile.into(),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&reach_mcp::catalog::exposed(&p))?
        );
        return Ok(());
    }
    let config_path = args.config.context("--config is required")?;
    let mut config: Config = serde_json::from_slice(&std::fs::read(&config_path)?)?;
    config.protected_paths.push(config_path.clone());
    config.validate()?;
    match command {
        Command::Token { command } => {
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(config.auth_file.with_extension("lock"))?;
            fs2::FileExt::try_lock_exclusive(&lock)
                .context("Another credential change is in progress")?;
            let mut store = Store::load(&config.auth_file)?;
            match command {
                TokenCommand::Issue {
                    principal,
                    label,
                    scopes,
                    profile,
                    ttl_hours,
                    token_file,
                    roots,
                } => {
                    let (id, token) = store.issue(
                        &principal,
                        &label,
                        scopes,
                        profile.into(),
                        Some(ttl_hours.checked_mul(3600).context("TTL overflow")?),
                    )?;
                    let roots = if roots.is_empty() {
                        vec![config.default_cwd.clone()]
                    } else {
                        roots
                    };
                    anyhow::ensure!(
                        roots.iter().all(|p| p.is_absolute() && p.is_dir()),
                        "Roots must be existing absolute directories"
                    );
                    store
                        .credentials
                        .get_mut(&id)
                        .expect("issued credential")
                        .roots = roots;
                    if let Some(file) = token_file {
                        anyhow::ensure!(file.is_absolute(), "Token file must be absolute");
                        let mut output = std::fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&file)?;
                        secure_file(&file)?;
                        output.write_all(token.as_bytes())?;
                        output.sync_all()?;
                        if let Err(e) = store.save(&config.auth_file) {
                            drop(output);
                            let _ = std::fs::remove_file(&file);
                            return Err(e);
                        }
                        println!(
                            "Issued {id} for {principal}; secret written once to {}",
                            file.display()
                        );
                    } else {
                        store.save(&config.auth_file)?;
                        println!("{token}");
                    }
                }
                TokenCommand::List => {
                    let rows:Vec<_>=store.credentials.iter().map(|(id,c)|serde_json::json!({"id":id,"principal":c.principal,"label":c.label,"scopes":c.scopes,"profile":c.profile,"created_at":c.created_at,"expires_at":c.expires_at,"revoked":c.revoked})).collect();
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                }
                TokenCommand::Revoke { id } => {
                    store
                        .credentials
                        .get_mut(&id)
                        .context("Credential not found")?
                        .revoked = true;
                    store.save(&config.auth_file)?;
                    println!("Revoked {id}; takes effect on the next request.");
                }
            }
            return Ok(());
        }
        Command::Check => {
            println!("Configuration and credential store are valid");
            return Ok(());
        }
        Command::Ui { no_open } => {
            let admin = config
                .admin
                .as_ref()
                .context("Management listener is disabled")?;
            let url = reach_mcp::admin::login_url(admin)?;
            if no_open {
                println!("{url}");
            } else {
                open_browser(&url)?;
                println!(
                    "Opened Reach management. The login link expires after 3 minutes and one use."
                );
            }
            return Ok(());
        }
        Command::Serve { background: true } => {
            std::fs::create_dir_all(&config.state_dir)?;
            let mut child = std::process::Command::new(std::env::current_exe()?);
            child
                .arg("--config")
                .arg(config_path)
                .arg("serve")
                .stdin(std::process::Stdio::null())
                .stdout(std::fs::File::create(
                    config.state_dir.join("server.stdout.log"),
                )?)
                .stderr(std::fs::File::create(
                    config.state_dir.join("server.stderr.log"),
                )?);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                child.creation_flags(0x08000000);
            }
            let pid = child.spawn()?.id();
            std::fs::write(config.state_dir.join("daemon.pid"), pid.to_string())?;
            println!("Reach started in background, PID {pid}");
            return Ok(());
        }
        _ => {}
    }
    std::fs::create_dir_all(&config.state_dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(config.state_dir.join("instance.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("Another reach owns this state directory")?;
    let stop = CancellationToken::new();
    let jobs = Arc::new(Jobs::open(&config.state_dir)?);
    let audit = Arc::new(Audit::open(&config.state_dir)?);
    let hand = Hand {
        config: Arc::new(config.clone()),
        jobs: jobs.clone(),
        audit,
    };
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let admin_server = if let Some(admin) = config.admin.clone() {
        let listener = tokio::net::TcpListener::bind(admin.listen).await?;
        let router = reach_mcp::admin::router(hand.clone(), admin);
        let shutdown = stop.clone();
        Some(tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        }))
    } else {
        None
    };
    let tunnel = config.ssh.clone().map(|ssh| {
        tokio::spawn(reach_mcp::process::tunnel(
            ssh,
            config.listen.port(),
            stop.clone(),
        ))
    });
    let server_stop = stop.clone();
    let result = axum::serve(listener, reach_mcp::http::router(hand, stop.clone()))
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            server_stop.cancel();
        })
        .await;
    stop.cancel();
    jobs.cancel_all();
    for _ in 0..100 {
        if jobs.active_count() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    if let Some(tunnel) = tunnel {
        let _ = tunnel.await;
    }
    if let Some(admin) = admin_server {
        admin.await??;
    }
    result?;
    Ok(())
}

fn open_browser(url: &str) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("rundll32.exe")
            .args(["url.dll,FileProtocolHandler", url])
            .creation_flags(0x08000000)
            .spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}
