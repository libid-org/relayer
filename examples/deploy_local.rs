//! The wallet stack on a fresh anvil, and a relayer file for it: compose.yaml's
//! one-shot `deploy`. Dev only: the stack's verifier is a stub that checks no proof.
//!
//! Run again with the file's factory deployed, it leaves both be; on an anvil that is
//! neither fresh nor deployed by it, it refuses.

use std::path::PathBuf;

use alloy::{
    providers::{
        Provider,
        ProviderBuilder,
    },
    signers::local::PrivateKeySigner,
};
use clap::Parser;
use libid_contracts::{
    testing::deploy_wallet_stack,
    Artifacts,
};
use relayer::{
    relay::Error,
    settings::Settings,
};

#[derive(Parser)]
struct Args {
    /// The anvil the relayer reaches, written into the file.
    #[arg(long, env = "RELAYER_RPC_URL", default_value = "http://127.0.0.1:8545")]
    rpc_url: String,
    /// The deployer's hex key, the stack's owner. The relayer is anvil's account 1.
    #[arg(long, env = "DEPLOYER_KEY", hide_env_values = true)]
    deployer: PrivateKeySigner,
    /// The relayer file to write.
    #[arg(long, env = "RELAYER_CONFIG", default_value = "relayer.toml")]
    out: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let Args {
        rpc_url,
        deployer,
        out,
    } = Args::parse();
    let owner = deployer.address();
    let provider = ProviderBuilder::new()
        .wallet(deployer)
        .connect_http(rpc_url.parse()?);
    if out.exists() {
        let factory = Settings::load(&out, |_| None)?.config.factory;
        if !provider.get_code_at(factory).await?.is_empty() {
            println!(
                "{} names deployed factory {factory}: left as is",
                out.display()
            );
            return Ok(());
        }
    }
    if provider.get_transaction_count(owner).await? != 0 {
        return Err("the deployer has sent before: start a fresh anvil".into());
    }
    let stack = deploy_wallet_stack(&provider, &Artifacts::embedded(), owner).await?;
    let relayer = provider.get_accounts().await?[1];
    let file = include_str!("deploy_local.toml")
        .replace("{rpc_url}", &rpc_url)
        .replace("{chain_id}", &provider.get_chain_id().await?.to_string())
        .replace("{factory}", &stack.factory.to_string())
        .replace("{forwarder}", &stack.forwarder.to_string())
        .replace("{relayer}", &relayer.to_string())
        .replace("{release}", &stack.release.version.to_string());
    std::fs::write(&out, file)?;
    Settings::load(&out, |_| None)?;
    println!("wrote {}: factory {}", out.display(), stack.factory);
    Ok(())
}
