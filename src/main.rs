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
    /// Check the chain, then serve and send until interrupted or terminated; a second
    /// signal exits at once.
    Run,
    /// Print the ledger's numbers.
    Stats {
        /// As JSON, wei amounts as decimal strings.
        #[arg(long)]
        json: bool,
    },
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
    if let Command::Stats { json } = cli.command {
        return match stats(&settings.db) {
            Ok(stats) if json => {
                println!("{:#}", stats.json());
                ExitCode::SUCCESS
            }
            Ok(stats) => {
                print!("{stats}");
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

/// The first interrupt or SIGTERM; a second exits at once.
async fn interrupted() {
    signal().await;
    tracing::info!("shutting down once requests in flight finish; signal again to exit");
    tokio::spawn(async {
        signal().await;
        std::process::exit(130);
    });
}

/// Ctrl-C, or SIGTERM, which `docker stop` sends.
async fn signal() {
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{
            signal,
            SignalKind,
        };
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => drop(terminate.recv().await),
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = terminate => {}
    }
}
