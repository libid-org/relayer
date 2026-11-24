//! The relayer's command line.

use std::{
    path::PathBuf,
    process::ExitCode,
};

use clap::{
    Parser,
    Subcommand,
};
use relayer::{
    logs,
    settings::Settings,
    startup,
    stats::stats,
};

/// Sends signed AUTHORIZED batches of libID web wallets, repaid inside them.
#[derive(Parser)]
#[command(name = "relayer", version, about)]
struct Cli {
    /// The relayer's TOML file.
    #[arg(long, short, global = true, env = "RELAYER_CONFIG")]
    #[arg(default_value = "relayer.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check the chain, then serve and send until interrupted; a second interrupt
    /// exits at once.
    Run,
    /// Print the ledger's numbers.
    Stats,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let settings = match Settings::load(&cli.config, |name| std::env::var(name).ok()) {
        Ok(settings) => settings,
        Err(e) => {
            eprintln!("config error: {}: {e:#}", cli.config.display());
            let missing = e.downcast_ref::<std::io::Error>();
            if missing.is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
                eprintln!(
                    "the path comes from --config or RELAYER_CONFIG; point it at a file"
                );
            }
            return ExitCode::FAILURE;
        }
    };
    if let Command::Stats = cli.command {
        return match stats(&settings.db) {
            Ok(stats) => {
                println!("{stats:#?}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("stats error: {e}");
                ExitCode::FAILURE
            }
        };
    }
    logs::init(settings.rpc_url.as_deref(), settings.signer.as_deref());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build();
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(startup::run(settings, interrupted())) {
        Ok(()) => ExitCode::SUCCESS,
        // Through the log, which redacts the RPC URL a node error names.
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// The first interrupt; a second exits at once.
async fn interrupted() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!(
        "shutting down once requests in flight finish; interrupt again to exit"
    );
    tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
        std::process::exit(130);
    });
}
