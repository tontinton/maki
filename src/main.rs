mod print;

use clap::{Parser, Subcommand};
use color_eyre::Result;
use maki_agent::model::{DEFAULT_SPEC, Model};
use tracing_subscriber::EnvFilter;

use print::OutputFormat;

const LOG_FILE_NAME: &str = "maki.log";

#[derive(Parser)]
#[command(name = "maki", version, about = "AI coding assistant")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(short, long)]
    print: bool,

    #[arg(short, long, default_value = DEFAULT_SPEC)]
    model: String,

    #[arg(long)]
    verbose: bool,

    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    output_format: OutputFormat,

    prompt: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    Models,
}

#[derive(Subcommand)]
enum AuthAction {
    Login,
    Logout,
}

fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();
    match cli.command {
        Some(Command::Auth { action }) => match action {
            AuthAction::Login => maki_agent::auth::login()?,
            AuthAction::Logout => maki_agent::auth::logout()?,
        },
        Some(Command::Models) => {
            maki_agent::provider::fetch_all_models(|models| {
                for model in models {
                    println!("{model}");
                }
            });
        }
        None => {
            let model = Model::from_spec(&cli.model)?;
            init_logging();
            if cli.print {
                print::run(&model, cli.prompt, cli.output_format, cli.verbose)?;
            } else {
                maki_ui::run(model)?;
            }
        }
    }
    Ok(())
}

fn init_logging() {
    let Ok(log_dir) = maki_agent::data_dir() else {
        return;
    };
    let file_appender = tracing_appender::rolling::never(&log_dir, LOG_FILE_NAME);
    let filter = EnvFilter::try_from_env("MAKI_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(file_appender)
        .init();
}
