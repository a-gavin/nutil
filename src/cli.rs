use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::bond::BondMode;

#[derive(Parser, Debug)]
#[command(name = "nutil")]
#[command(author = "A. Gavin <a_gavin@icloud.com>")]
#[command(about = "Utility for creating and managing bond devices using libnm", long_about = None)]
pub struct App {
    #[clap(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// UNIMPLEMENTED
    // Configure NetworkManager-managed access point (wireless) connections
    AccessPoint {
        #[clap(value_enum)]
        action: Action,

        #[clap(flatten)]
        c_args: BondArgs,
    },
    /// Configure NetworkManager-managed bond connections
    Bond {
        #[clap(value_enum)]
        action: Action,

        #[clap(flatten)]
        c_args: BondArgs,
    },
}

#[derive(ValueEnum, Clone, Debug)]
pub enum Action {
    Create,
    Delete,
    Status,
}

#[derive(Args, Debug)]
pub struct AccessPointArgs {
    pub ifname: Option<String>,
    // TODO: Other AP options
}

#[derive(Args, Debug)]
pub struct BondArgs {
    /// Bond connection and backing device name (must match)
    pub ifname: Option<String>,

    #[clap(value_enum)]
    pub bond_mode: Option<BondMode>,

    /// Bond backing wired device interface names (required for creation and deletion)
    pub slave_ifnames: Vec<String>,
}
