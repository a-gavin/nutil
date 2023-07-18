use anyhow::{Context, Result};
use clap::Parser;
use nm::*;

pub mod bond;
pub mod cli;
pub mod connection;

use crate::bond::*;
use crate::cli::*;

fn main() -> Result<()> {
    // Defaults to printing logs at info level for all spans if not specified
    tracing_subscriber::fmt().pretty().init();

    let opts = App::parse();

    let context = glib::MainContext::default();
    context.block_on(run(opts))
}

async fn run(args: App) -> Result<()> {
    let client = Client::new_future()
        .await
        .context("Failed to create NM Client")?;

    match args.command {
        Command::AccessPoint {
            action: _,
            c_args: _,
        } => todo!(),
        Command::Bond { action, c_args } => match action {
            Action::Create => create_bond(&client, c_args).await,
            Action::Delete => delete_bond(&client, c_args).await,
            Action::Status => bond_status(&client, c_args),
        },
    }
}
