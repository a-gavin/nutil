// Required for string conversion when getting a connection's SSID
#![feature(ascii_char)]

use anyhow::{Context, Result};
use clap::Parser;
use nm::*;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

pub mod access_point;
pub mod bond;
pub mod cli;
pub mod connection;
pub mod station;
pub mod util;

use crate::access_point::*;
use crate::bond::*;
use crate::cli::*;

fn main() -> Result<()> {
    // Defaults to printing logs at info level for all spans if not specified
    // TODO: ^^^^
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_env("NUTIL_LOG"))
        .init();

    let opts = App::parse();

    let context = glib::MainContext::default();
    context.block_on(run(opts))
}

async fn run(args: App) -> Result<()> {
    let client = Client::new_future()
        .await
        .context("Failed to create NM Client")?;

    match args.command {
        Command::AccessPoint { action, mut c_args } => {
            c_args.config = args.config;
            let opts = AccessPointOpts::try_from(c_args)?;

            match action {
                Action::Create => create_access_point(&client, opts).await,
                Action::Delete => todo!(), //delete_access_point(&client, opts).await,
                Action::Status => todo!(), //bond_staccess_point(&client, opts),
            }
        }
        Command::Bond { action, mut c_args } => {
            c_args.config = args.config;
            let opts = BondOpts::try_from(c_args)?;

            match action {
                Action::Create => create_bond(&client, opts).await,
                Action::Delete => delete_bond(&client, opts).await,
                Action::Status => bond_status(&client, opts),
            }
        }
    }
}
