use std::{path::PathBuf, time::Duration};

use clap::{Parser, Subcommand};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::oidc::OidcConfig;

mod auth;
mod dashboard;
mod http;
mod hub;
mod oidc;
mod server;
mod store;

use store::{MintTokens, Role, Store};

const DEFAULT_DB_PATH: &str = "maki-anchor.sqlite3";
const DEFAULT_CONFIG_PATH: &str = "maki-anchor.toml";
const DEFAULT_BIND: &str = "0.0.0.0:8688";
const DEFAULT_LINK_TTL_HOURS: u64 = 2;

/// Anchor's own config file: listen address, OIDC and auth policy live here,
/// not in maki's config.
#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct AnchorConfig {
    /// Listen address, e.g. `127.0.0.1:8688`. The `--bind` flag wins.
    bind: Option<String>,
    oidc: Option<OidcFileConfig>,
    auth: Option<AuthFileConfig>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct AuthFileConfig {
    /// Allow username/password local users in addition to OIDC. Default true.
    allow_local_users: Option<bool>,
    /// Who can mint instance tokens: any (anonymous), user (any logged-in), admin (admins only). Default: admin if OIDC, user otherwise.
    /// Env MAKI_ANCHOR_MINT_TOKENS overrides.
    mint_tokens: Option<MintTokens>,
    /// Deprecated: use mint_tokens. If true, maps to user, if false to any.
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
        #[arg(long)]
        bind: Option<String>,
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
    /// Toggle admin flag for a user
    SetAdmin {
        username: String,
        #[arg(long, default_value_t = true)]
        admin: bool,
    },
    /// Delete a user (and their grants/sessions)
    Delete { username: String },
}

#[derive(Subcommand)]
enum TokenCommand {
    /// Create (or rotate) a registration token for an instance
    Add {
        name: String,
        /// Grant this user rights on the instance immediately (see `users list`
        /// for ids). Without this, a CLI-minted instance has no grants, and
        /// stays invisible on every non-admin dashboard until `grants set`.
        #[arg(long)]
        user_id: Option<i64>,
        #[arg(long, default_value = "control")]
        rights: String,
    },
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
            let oidc = db_oidc(&store)
                .or_else(|| {
                    anchor_config
                        .oidc
                        .as_ref()
                        .and_then(OidcFileConfig::complete)
                })
                .inspect(|_| {
                    tracing::info!("OIDC login enabled");
                });
            let bind = resolve_bind(bind.as_deref(), anchor_config.bind.as_deref());
            let allow_local = anchor_config
                .auth
                .as_ref()
                .and_then(|a| a.allow_local_users)
                .unwrap_or(true);
            let mint_tokens = {
                if let Some(v) = std::env::var("MAKI_ANCHOR_MINT_TOKENS")
                    .ok()
                    .and_then(|s| MintTokens::parse(&s))
                {
                    v
                } else if let Some(v) = anchor_config.auth.as_ref().and_then(|a| a.mint_tokens) {
                    v
                } else if let Some(v) = anchor_config
                    .auth
                    .as_ref()
                    .and_then(|a| a.require_auth_for_tokens)
                {
                    if v { MintTokens::User } else { MintTokens::Any }
                } else if let Some(v) = std::env::var("MAKI_ANCHOR_REQUIRE_AUTH_FOR_TOKENS")
                    .ok()
                    .map(|s| s != "0" && s.to_lowercase() != "false")
                {
                    if v { MintTokens::User } else { MintTokens::Any }
                } else if oidc.is_some() {
                    MintTokens::Admin
                } else {
                    MintTokens::Any
                }
            };
            if let Err(err) = server::serve(&bind, store, oidc, allow_local, mint_tokens) {
                eprintln!("fatal: {err}");
                std::process::exit(1);
            }
        }
        Command::Tokens { sub, db } => {
            let store = Store::open(&db).expect("open anchor db");
            match sub {
                TokenCommand::Add {
                    name,
                    user_id,
                    rights,
                } => {
                    if !store::valid_instance_name(&name) {
                        eprintln!("instance name must be 1-64 chars of alphanumeric, -, _, .");
                        std::process::exit(1);
                    }
                    let rights = if user_id.is_some() {
                        match Role::parse(&rights) {
                            Some(rights) => rights,
                            None => {
                                eprintln!("rights must be `view` or `control`, got `{rights}`");
                                std::process::exit(1);
                            }
                        }
                    } else {
                        Role::Controller
                    };
                    let token = new_token();
                    let instance_id = store
                        .register_instance(&name, &store::hash_token(&token))
                        .expect("register instance");
                    if let Some(user_id) = user_id {
                        store
                            .set_grant(user_id, instance_id, rights)
                            .expect("grant user on instance");
                        eprintln!("granted user {user_id} {} on {name}", rights.as_str());
                    }
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
                            &link,
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
                    for (user_id, user, instance, rights) in
                        store.list_grants().expect("list grants")
                    {
                        println!("{user} {instance} {rights} (user {user_id})");
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
                UserCommand::SetAdmin { username, admin } => {
                    let user = store
                        .local_user_by_username(&username)
                        .or_else(|_| store.user_by_sub(&format!("local:{username}")))
                        .or_else(|_| store.user_by_sub(&username))
                        .expect("no such user");
                    store.set_user_admin(user.id, admin).expect("set admin");
                    println!("user {} admin={}", username, admin);
                }
                UserCommand::Delete { username } => {
                    let user = store
                        .local_user_by_username(&username)
                        .or_else(|_| store.user_by_sub(&format!("local:{username}")))
                        .or_else(|_| store.user_by_sub(&username))
                        .expect("no such user");
                    if store.delete_user(user.id).expect("delete") {
                        println!("deleted user {} id {}", username, user.id);
                    } else {
                        eprintln!("delete failed");
                        std::process::exit(1);
                    }
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

/// Where the listener binds: the flag still wins over the file, and the
/// file over history.
fn resolve_bind(flag: Option<&str>, file: Option<&str>) -> String {
    flag.or(file).unwrap_or(DEFAULT_BIND).to_owned()
}

/// SSO configured from the admin page lives in the settings table and beats
/// the file: hosted installs should not need shell access to turn on OIDC.
fn db_oidc(store: &Store) -> Option<OidcConfig> {
    let issuer = store.get_setting("oidc_issuer").ok()?;
    let client_id = store.get_setting("oidc_client_id").ok()?;
    let client_secret = store.get_setting("oidc_client_secret").ok()?;
    let origin = store.get_setting("oidc_origin").ok()?;
    let config = OidcConfig {
        issuer: issuer?,
        client_id: client_id?,
        client_secret: client_secret?,
        origin: origin?,
    };
    (!config.issuer.is_empty() && !config.origin.is_empty()).then_some(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(Some("1.2.3.4:1"), Some("5.6.7.8:2") => "1.2.3.4:1".to_owned() ; "flag_wins")]
    #[test_case(None,              Some("5.6.7.8:2") => "5.6.7.8:2".to_owned() ; "file_second")]
    #[test_case(None,              None             => DEFAULT_BIND.to_owned() ; "default_last")]
    fn bind_precedence(flag: Option<&str>, file: Option<&str>) -> String {
        resolve_bind(flag, file)
    }

    #[test]
    fn settings_table_beats_the_file_for_sso() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        assert!(db_oidc(&store).is_none(), "nothing configured yet");
        for (key, value) in [
            ("oidc_issuer", "https://auth.test/realms/main"),
            ("oidc_client_id", "maki-anchor"),
            ("oidc_client_secret", "shh"),
            ("oidc_origin", "https://maki.test"),
        ] {
            store.set_setting(key, value).unwrap();
        }
        let oidc = db_oidc(&store).expect("the full set configures SSO");
        assert_eq!(oidc.issuer, "https://auth.test/realms/main");
        assert_eq!(oidc.origin, "https://maki.test");
        store.delete_setting("oidc_issuer").unwrap();
        assert!(db_oidc(&store).is_none(), "a half set is no set");
    }

    #[test]
    fn config_file_accepts_bind() {
        let parsed: AnchorConfig =
            toml::from_str("bind = \"127.0.0.1:9999\"\n[auth]\nallow_local_users = false\n")
                .expect("bind is a known key");
        assert_eq!(parsed.bind.as_deref(), Some("127.0.0.1:9999"));
        assert_eq!(parsed.auth.unwrap().allow_local_users, Some(false));
    }
}
