use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::bond::BondMode;

#[derive(Parser, Debug)]
#[command(name = "nutil")]
#[command(author = "A. Gavin <a_gavin@icloud.com>")]
#[command(about = "Utility for creating and managing bond devices using libnm", long_about = None)]
pub struct App {
    #[clap(subcommand)]
    pub command: Command,

    #[arg(short, long)]
    pub config: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    // Configure NetworkManager-managed access point (wireless) connections
    AccessPoint {
        #[clap(value_enum)]
        action: Action,

        #[clap(flatten)]
        c_args: AccessPointArgs,
    },
    /// Configure NetworkManager-managed bond connections
    Bond {
        // TODO: Make bond_mode optional for CLI too (only optional in config atm)
        /// Bond creation requires a bond interface name and one or more
        /// backing wired slave interface names. Bond mode defaults to
        /// ActiveBackup when unspecified (only optional in config).
        ///
        /// Bond status requires only a bond interface name.
        ///
        /// Bond deletion requires a bond interface name and an optional
        /// bond mode. Will delete a bond with a differing bond mode.
        /// If specified, optional backing slave interfaces will be deleted
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
    pub ssid: Option<String>,

    pub wireless_ifname: Option<String>,

    pub ip4_addr: Option<String>,

    pub password: Option<String>,

    #[clap(skip)]
    pub config: Option<String>,
}

#[derive(Args, Debug)]
pub struct BondArgs {
    /// Bond connection and backing device name (must match)
    #[clap(name = "bond_interface")]
    pub ifname: Option<String>,

    /// Bond mode of operation (defaults to ActiveBackup)
    #[clap(value_enum)]
    pub bond_mode: Option<BondMode>,

    /// Bond backing wired device interface names (required for creation and deletion)
    #[clap(name = "slave_interfaces")]
    pub slave_ifnames: Vec<String>,

    #[clap(skip)]
    pub config: Option<String>,
}

#[derive(Args, Debug)]
pub struct StationArgs {
    pub wireless_ifname: Option<String>,

    pub ssid: Option<String>,

    pub password: Option<String>,

    #[clap(skip)]
    pub config: Option<String>,
}
