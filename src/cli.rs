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
    // Configure NetworkManager-managed station (wireless client) connections
    Station {
        #[clap(value_enum)]
        action: Action,

        #[clap(flatten)]
        c_args: StationArgs,
    },
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
    /// During connection creation, any connections which share interfaces
    /// with the desired connection are deactivated but not deleted. This
    /// includes any slave interfaces, if the desired connection uses such.
    Create,
    /// During connection deletion, any slave interfaces specified that are
    /// are associated with the connection to be deleted are also deleted.
    Delete,
    Status,
}

#[derive(Args, Debug)]
pub struct StationArgs {
    /// SSID used for station association
    pub ssid: Option<String>,

    /// Wireless radio used to create station
    pub wireless_ifname: Option<String>,

    /// Password for SSID (currently WPA-PSK only). If not specified, default to Open
    pub password: Option<String>,

    /// Static IPv4 address. If not specified, default to DHCP
    pub ip4_addr: Option<String>,

    #[clap(skip)]
    pub config: Option<String>,
}

#[derive(Args, Debug)]
pub struct AccessPointArgs {
    /// SSID used for access point
    pub ssid: Option<String>,

    /// Wireless radio used to create access point
    pub wireless_ifname: Option<String>,

    /// Static IPv4 address. If not specified, default to DHCP
    /// When specified, include subnet mask, e.g. "192.168.0.10/24"
    pub ip4_addr: Option<String>,

    /// Password for SSID (currently WPA-PSK only). If not specified, default to Open
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

    /// Static IPv4 address. Use "DHCP" if no static IPv4 address desired.
    /// When specified, nclude subnet mask, e.g. "192.168.0.10/24"
    // TODO: Make this truly optional (after slave_ifnames,
    //       won't compile that way tho as slave_ifnames is variable length)
    pub ip4_addr: Option<String>,

    /// Bond backing wired device interface names (required for creation and deletion)
    #[clap(name = "slave_interfaces")]
    pub slave_ifnames: Vec<String>,

    #[clap(skip)]
    pub config: Option<String>,
}
