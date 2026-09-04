use std::{path::PathBuf, time::Duration};

use clap::{Parser, Subcommand};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::oidc::OidcConfig;

mod auth;
mod dashboard;
mod hub;
mod oidc;
mod server;
mod store;

use store::{Role, Store};

const DEFAULT_DB_PATH: &str = "maki-anchor.sqlite3";
const DEFAULT_CONFIG_PATH: &str = "maki-anchor.toml";
const DEFAULT_BIND: &str = "0.0.0.0:8688";
const DEFAULT_LINK_TTL_HOURS: u64 = 2;

/// Anchor's own config file: OIDC settings live here, not in maki's config.
#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct AnchorConfig {
    oidc: Option<OidcFileConfig>,
    auth: Option<AuthFileConfig>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct AuthFileConfig {
    /// Allow username/password local users in addition to OIDC. Default true.
    allow_local_users: Option<bool>,
    /// If true, only authenticated users (OIDC or local) can mint instance tokens via the UI/API.
    /// Default: true when OIDC is configured, false otherwise. Override with MAKI_ANCHOR_REQUIRE_AUTH_FOR_TOKENS env.
    require_auth_for_tokens: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct OidcFileConfig {
    issuer: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    origin: Option<String>,
}

impl OidcFileConfig {
    fn complete(&self) -> Option<OidcConfig> {
        Some(OidcConfig {
            issuer: self.issuer.clone()?,
            client_id: self.client_id.clone()?,
            client_secret: self.client_secret.clone()?,
            origin: self.origin.clone()?,
        })
    }
}

#[derive(Parser)]
#[command(name = "maki-anchor", about = "Anchor server for maki remote control")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the anchor server
    Serve {
        #[arg(long, default_value = DEFAULT_BIND)]
        bind: String,
        #[arg(long, default_value = DEFAULT_DB_PATH)]
        db: PathBuf,
        #[arg(long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
    /// Manage instances: registration tokens and share links
    #[clap(name = "tokens")]
    Tokens {
        #[command(subcommand)]
        sub: TokenCommand,
        #[arg(long, default_value = DEFAULT_DB_PATH, global = true)]
        db: PathBuf,
    },
    /// Manage per-user access grants
    Grants {
        #[command(subcommand)]
        sub: GrantCommand,
        #[arg(long, default_value = DEFAULT_DB_PATH, global = true)]
        db: PathBuf,
    },
    /// Manage local users (username/password) for when OIDC is not used or as fallback
    Users {
        #[command(subcommand)]
        sub: UserCommand,
        #[arg(long, default_value = DEFAULT_DB_PATH, global = true)]
        db: PathBuf,
    },
}

#[derive(Subcommand)]
enum GrantCommand {
    /// Grant a user rights on an instance
    Set {
        user_id: i64,
        instance: String,
        #[arg(default_value = "view")]
        rights: String,
    },
    /// Remove a grant
    Revoke { user_id: i64, instance: String },
    /// List grants
    List,
    /// Look up a user id by OIDC subject (for writing grants)
    Lookup { oidc_sub: String },
}

#[derive(Subcommand)]
enum UserCommand {
    /// Add a local user (prompts for password)
    Add {
        username: String,
        #[arg(long)]
        admin: bool,
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    /// List users
    List,
    /// Set password for a local user
    SetPassword { username: String },
}

#[derive(Subcommand)]
enum TokenCommand {
    /// Create (or rotate) a registration token for an instance
    Add { name: String },
    /// Mint a share link: `view` or `control` rights, hours until expiry
    Link {
        instance: String,
        #[arg(default_value = "view")]
        rights: String,
        #[arg(long, default_value_t = DEFAULT_LINK_TTL_HOURS)]
        ttl_hours: u64,
        #[arg(long)]
        session: Option<String>,
    },
    /// Revoke a share link by its raw token
    Revoke { token: String },
}

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MAKI_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("maki_anchor=info")),
        )
        .with_writer(std::io::stderr)
        .try_init();
    let cli = Cli::parse();
    match cli.command {
        Command::Serve { bind, db, config } => {
            let store = Store::open(&db).expect("open anchor db");
            let anchor_config = match std::fs::read_to_string(&config) {
                Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                    eprintln!("config {config:?}: {e}");
                    std::process::exit(1);
                }),
                Err(_) => AnchorConfig::default(),
            };
            let oidc = anchor_config
                .oidc
                .as_ref()
                .and_then(OidcFileConfig::complete)
                .inspect(|_| {
                    tracing::info!("OIDC login enabled");
                });
            let allow_local = anchor_config
                .auth
                .as_ref()
                .and_then(|a| a.allow_local_users)
                .unwrap_or(true);
            let require_auth_for_tokens = anchor_config
                .auth
                .as_ref()
                .and_then(|a| a.require_auth_for_tokens)
                .or_else(|| {
                    std::env::var("MAKI_ANCHOR_REQUIRE_AUTH_FOR_TOKENS")
                        .ok()
                        .map(|v| v != "0" && v.to_lowercase() != "false")
                })
                .unwrap_or_else(|| oidc.is_some());
            if let Err(err) =
                server::serve(&bind, store, oidc, allow_local, require_auth_for_tokens)
            {
                eprintln!("fatal: {err}");
                std::process::exit(1);
            }
        }
        Command::Tokens { sub, db } => {
            let store = Store::open(&db).expect("open anchor db");
            match sub {
                TokenCommand::Add { name } => {
                    let token = new_token();
                    store
                        .register_instance(&name, &store::hash_token(&token))
                        .expect("register instance");
                    println!("{token}");
                }
                TokenCommand::Link {
                    instance,
                    rights,
                    ttl_hours,
                    session,
                } => {
                    let Some(rights) = Role::parse(&rights) else {
                        eprintln!("rights must be `view` or `control`, got `{rights}`");
                        std::process::exit(1);
                    };
                    let rights = rights.as_str();
                    let instance_id = store
                        .instance_by_name(&instance)
                        .expect("unknown instance; add it with `tokens add` first")
                        .id;
                    let link = new_token();
                    store
                        .create_link(
                            &store::hash_token(&link),
                            instance_id,
                            session.as_deref(),
                            rights,
                            Duration::from_secs(ttl_hours * 3600),
                        )
                        .expect("create link");
                    println!("{link}");
                }
                TokenCommand::Revoke { token } => {
                    if store
                        .revoke_link(&store::hash_token(&token))
                        .expect("revoke")
                    {
                        println!("revoked");
                    } else {
                        eprintln!("no such active link");
                        std::process::exit(1);
                    }
                }
            }
        }
        Command::Grants { sub, db } => {
            let store = Store::open(&db).expect("open anchor db");
            match sub {
                GrantCommand::Set {
                    user_id,
                    instance,
                    rights,
                } => {
                    let Some(rights) = Role::parse(&rights) else {
                        eprintln!("rights must be `view` or `control`");
                        std::process::exit(1);
                    };
                    let instance_id = instance_id_by_name(&store, &instance);
                    store
                        .set_grant(user_id, instance_id, rights)
                        .expect("set grant");
                    println!("{rights:?} granted to {user_id} on {instance}");
                }
                GrantCommand::Revoke { user_id, instance } => {
                    let instance_id = instance_id_by_name(&store, &instance);
                    if store
                        .delete_grant(user_id, instance_id)
                        .expect("delete grant")
                    {
                        println!("revoked");
                    } else {
                        eprintln!("no such grant");
                        std::process::exit(1);
                    }
                }
                GrantCommand::List => {
                    for (user, instance, rights) in store.list_grants().expect("list grants") {
                        println!("{user} {instance} {rights}");
                    }
                }
                GrantCommand::Lookup { oidc_sub } => {
                    let user = store.user_by_sub(&oidc_sub).expect("no such user");
                    println!("{} {}", user.id, user.oidc_sub);
                }
            }
        }
        Command::Users { sub, db } => {
            let store = Store::open(&db).expect("open anchor db");
            match sub {
                UserCommand::Add {
                    username,
                    admin,
                    email,
                    name,
                } => {
                    let password = prompt_password(&format!("Password for {username}: "));
                    let confirm = prompt_password("Confirm: ");
                    if password != confirm {
                        eprintln!("passwords do not match");
                        std::process::exit(1);
                    }
                    if password.len() < 8 {
                        eprintln!("password must be at least 8 characters");
                        std::process::exit(1);
                    }
                    let user = store
                        .create_local_user(
                            &username,
                            &password,
                            email.as_deref(),
                            name.as_deref(),
                            admin,
                        )
                        .expect("create local user");
                    println!(
                        "local user {} id {} admin={} created",
                        username, user.id, user.is_admin
                    );
                }
                UserCommand::List => {
                    for u in store.list_users().expect("list users") {
                        println!(
                            "{} {} {} {} admin={} local={}",
                            u.id,
                            u.oidc_sub,
                            u.email.as_deref().unwrap_or("-"),
                            u.name.as_deref().unwrap_or("-"),
                            u.is_admin,
                            u.oidc_sub.starts_with("local:")
                        );
                    }
                }
                UserCommand::SetPassword { username } => {
                    let password = prompt_password(&format!("New password for {username}: "));
                    let confirm = prompt_password("Confirm: ");
                    if password != confirm {
                        eprintln!("passwords do not match");
                        std::process::exit(1);
                    }
                    let existing = store
                        .local_user_by_username(&username)
                        .expect("no such local user");
                    store
                        .create_local_user(
                            &username,
                            &password,
                            existing.email.as_deref(),
                            existing.name.as_deref(),
                            existing.is_admin,
                        )
                        .expect("set password");
                    println!("password updated for {username}");
                }
            }
        }
    }
}

fn prompt_password(prompt: &str) -> String {
    use std::io::{self, Write};
    print!("{prompt}");
    io::stdout().flush().unwrap();
    // Try to disable echo with stty if available, otherwise just read.
    let mut password = String::new();
    // Use rpassword if available via tty, fallback.
    #[cfg(unix)]
    {
        let _ = io::stdout().flush();
        // Try stty -echo
        let _ = std::process::Command::new("stty").arg("-echo").status();
        let res = io::stdin().read_line(&mut password);
        let _ = std::process::Command::new("stty").arg("echo").status();
        println!();
        if res.is_ok() {
            return password.trim().to_owned();
        }
    }
    io::stdin().read_line(&mut password).unwrap();
    password.trim().to_owned()
}

fn instance_id_by_name(store: &Store, name: &str) -> i64 {
    store
        .instance_by_name(name)
        .expect("unknown instance; add it with `tokens add` first")
        .id
}

fn new_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("rng failed");
    let digest = Sha256::digest(bytes);
    format!("{:x}", digest)[..32].to_string()
}
