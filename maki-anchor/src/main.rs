use std::{path::PathBuf, time::Duration};

use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};

mod hub;
mod server;
mod store;

use store::Store;

const DEFAULT_DB_PATH: &str = "maki-anchor.sqlite3";
const DEFAULT_BIND: &str = "0.0.0.0:8688";
const DEFAULT_LINK_TTL_HOURS: u64 = 2;

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
    },
    /// Manage instances: registration tokens and share links
    #[clap(name = "tokens")]
    Tokens {
        #[command(subcommand)]
        sub: TokenCommand,
        #[arg(long, default_value = DEFAULT_DB_PATH, global = true)]
        db: PathBuf,
    },
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
        .try_init();
    let cli = Cli::parse();
    match cli.command {
        Command::Serve { bind, db } => {
            let store = Store::open(&db).expect("open anchor db");
            if let Err(err) = server::serve(&bind, store) {
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
                    let rights = match rights.as_str() {
                        "view" | "viewer" => "viewer",
                        "control" => "controller",
                        _other => {
                            eprintln!("rights must be `view` or `control`, got `{rights}`");
                            std::process::exit(1);
                        }
                    };
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
    }
}

fn new_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("rng failed");
    let digest = Sha256::digest(bytes);
    format!("{:x}", digest)[..32].to_string()
}
