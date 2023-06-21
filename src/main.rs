use anyhow::{anyhow, Context, Result};
use nm::*;

use clap::{ArgEnum, Args, Parser, Subcommand};

use tracing::{debug, error, info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Parser, Debug)]
pub struct App {
    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Create {
        #[clap(arg_enum)]
        c_type: ConnectionType,

        #[clap(flatten)]
        c_args: ConnectionOpts,
    },
    Delete {
        #[clap(arg_enum)]
        c_type: ConnectionType,

        #[clap(flatten)]
        c_args: ConnectionOpts,
    },
    Status {
        #[clap(arg_enum)]
        c_type: ConnectionType,

        #[clap(flatten)]
        c_args: ConnectionOpts,
    },
}

#[derive(ArgEnum, Clone, Debug)]
enum ConnectionType {
    Bond,
    AccessPoint,
}

#[derive(Args, Debug)]
struct ConnectionOpts {
    // General options
    ifname: Option<String>,

    // Bond-specific options
    wired_ifname1: Option<String>,
    wired_ifname2: Option<String>,

    #[clap(arg_enum)]
    bond_mode: Option<BondMode>,
}

#[derive(ArgEnum, Clone, Debug)]
enum BondMode {
    RoundRobin = 0,
    ActiveBackup = 1,
    XOR = 2,
    Broadcast = 3,
    DynamicLinkAggregation = 4,
    TransmitLoadBalancing = 5,
    AdaptiveLoadBalancing = 6,
}

struct BondOpts {
    bond_ifname: String,
    bond_mode: BondMode,
    wired_ifname1: String,
    wired_ifname2: String,
}

impl TryFrom<ConnectionOpts> for BondOpts {
    type Error = anyhow::Error;

    fn try_from(opts: ConnectionOpts) -> Result<Self, Self::Error> {
        let bond_ifname = match opts.ifname {
            Some(ifname) => ifname,
            None => return Err(anyhow!("Bond interface name not specified")),
        };

        let bond_mode = match opts.bond_mode {
            Some(mode) => mode,
            None => return Err(anyhow!("Bond mode not specified")),
        };

        let wired_ifname1 = match opts.wired_ifname1 {
            Some(ifname) => ifname,
            None => return Err(anyhow!("First wired interface name not specified")),
        };

        let wired_ifname2 = match opts.wired_ifname2 {
            Some(ifname) => ifname,
            None => return Err(anyhow!("Second wired interface name not specified")),
        };

        Ok(BondOpts {
            bond_ifname,
            bond_mode,
            wired_ifname1,
            wired_ifname2,
        })
    }
}

fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_env("RUST_LOG"))
        .init();

    let opts = App::parse();

    let context = glib::MainContext::default();
    context.block_on(run(opts))
}

async fn run(opts: App) -> Result<()> {
    let client = Client::new_future()
        .await
        .context("Failed to create NM Client")?;

    match opts.command {
        Command::Create { c_type, c_args } => create_connection(&client, c_type, c_args).await,
        Command::Delete { c_type, c_args } => delete_connection(&client, c_type, c_args).await,
        Command::Status {
            c_type: _,
            c_args: _,
        } => todo!(),
    }
}

async fn create_connection(
    client: &Client,
    c_type: ConnectionType,
    c_opts: ConnectionOpts,
) -> Result<()> {
    match c_type {
        ConnectionType::Bond => create_bond(&client, c_opts).await,
        ConnectionType::AccessPoint => todo!(),
    }
}

async fn delete_connection(
    client: &Client,
    c_type: ConnectionType,
    c_opts: ConnectionOpts,
) -> Result<()> {
    match c_type {
        ConnectionType::Bond => delete_bond(&client, c_opts).await,
        ConnectionType::AccessPoint => todo!(),
    }
}

async fn create_bond(client: &Client, c_opts: ConnectionOpts) -> Result<()> {
    let opts = BondOpts::try_from(c_opts)?;

    // Make sure a bond connection with same name does not already exist
    if get_connection(&client, &opts.bond_ifname, DeviceType::Bond).is_some() {
        warn!("Bond connection already exists, quitting...");
        return Err(anyhow!("Bond connection already exists, quitting..."));
    }

    // Get backing devices for provided wired interfaces
    match client.device_by_iface(&opts.wired_ifname1) {
        Some(device) => device,
        None => {
            warn!(
                "Required wired device \"{}\" does not exist, quitting...",
                &opts.wired_ifname1
            );
            return Err(anyhow!(
                "Wired device \"{}\" does not exist, quitting...",
                &opts.wired_ifname1
            ));
        }
    };

    match client.device_by_iface(&opts.wired_ifname2) {
        Some(device) => device,
        None => {
            warn!(
                "Wired device \"{}\" does not exist, quitting...",
                &opts.wired_ifname2
            );
            return Err(anyhow!(
                "Wired device \"{}\" does not exist, quitting...",
                &opts.wired_ifname2
            ));
        }
    };

    // Bond connection doesn't exist and backing ethernet connections exist,
    // so create new bond connection (including backing slave ethernet connections)
    info!("Creating bond connection");
    let bond_conn = create_bond_connection(&opts.bond_ifname, opts.bond_mode)?;
    client.add_connection_future(&bond_conn, true).await?;

    let wired_conn1 = create_wired_connection(&opts.wired_ifname1, &opts.bond_ifname)?;
    client.add_connection_future(&wired_conn1, true).await?;

    let wired_conn2 = create_wired_connection(&opts.wired_ifname2, &opts.bond_ifname)?;
    client.add_connection_future(&wired_conn2, true).await?;

    // Deactivate non-slave ethernet connections. Otherwise, newly-created bond
    // connection will stay in "Activating" state until backing slave connections are
    // active (which the existing non-slave ethernet connections preempt from doing so).
    let existing_wired_conn1 =
        match get_active_connection(&client, &opts.wired_ifname1, DeviceType::Ethernet) {
            Some(c) => c,
            None => {
                return Err(anyhow!(
                    "Required wired connection \"{}\" does not exist, quitting...",
                    &opts.wired_ifname1
                ));
            }
        };

    let existing_wired_conn2 =
        match get_active_connection(&client, &opts.wired_ifname2, DeviceType::Ethernet) {
            Some(c) => c,
            None => {
                return Err(anyhow!(
                    "Required wired connection \"{}\" does not exist, quitting...",
                    &opts.wired_ifname2
                ));
            }
        };

    // Only deactivate both existing if we're able to find both
    // Bond interface will transition to active after these two are successfully downed
    info!("Deactivating existing wired connections which use same interfaces as bond slave wired connection ifnames--\"{}\" and \"{}\"", &opts.wired_ifname1, &opts.wired_ifname2);
    client
        .deactivate_connection_future(&existing_wired_conn1)
        .await?;
    client
        .deactivate_connection_future(&existing_wired_conn2)
        .await?;

    // TODO: Handle case where existing conns are down and downing again doesn't automatically up the Wired IFs
    // Just up all three tbh
    info!(
        "Activating bond slave wired connections with ifnames \"{}\" and \"{}\"",
        &opts.wired_ifname1, &opts.wired_ifname2
    );

    Ok(())
}

async fn delete_bond(client: &Client, c_opts: ConnectionOpts) -> Result<()> {
    let opts = BondOpts::try_from(c_opts)?;

    // Deactivate bond connection
    // Automatically deactivates slave connections on success
    info!("Deactivating bond connection with interface \"{}\" (and associated slave wired connections)", opts.bond_ifname);
    match get_active_connection(&client, &opts.bond_ifname, DeviceType::Bond) {
        Some(c) => {
            client.deactivate_connection_future(&c).await?;
            info!("Bond connection and associated interfaces deactivated");
        }
        None => {
            return Err(anyhow!(
                "Required bond connection \"{}\" does not exist, quitting...",
                &opts.bond_ifname
            ));
        }
    };

    // Delete bond connection
    info!(
        "Deleting bond connection with interface \"{}\"",
        opts.bond_ifname
    );
    match get_connection(&client, &opts.bond_ifname, DeviceType::Bond) {
        Some(c) => {
            c.delete_future().await?;
            info!("Bond connection deleted");
        }
        None => {
            return Err(anyhow!(
                "Required bond connection \"{}\" does not exist, quitting...",
                &opts.bond_ifname
            ));
        }
    };

    // Delete first wired slave connection
    match get_connection(&client, &opts.wired_ifname1, DeviceType::Ethernet) {
        Some(c) => c.delete_future().await?,
        None => {
            return Err(anyhow!(
                "Required wired connection \"{}\" does not exist, quitting...",
                &opts.wired_ifname1
            ));
        }
    };

    // Delete second wired slave connection
    match get_connection(&client, &opts.wired_ifname2, DeviceType::Ethernet) {
        Some(c) => c.delete_future().await?,
        None => {
            return Err(anyhow!(
                "Required wired connection \"{}\" does not exist, quitting...",
                &opts.wired_ifname2
            ));
        }
    };

    Ok(())
}

fn create_bond_connection(bond_ifname: &str, bond_mode: BondMode) -> Result<SimpleConnection> {
    let connection = SimpleConnection::new();

    let s_connection = SettingConnection::new();
    let s_bond = SettingBond::new();

    // General connection settings
    s_connection.set_type(Some(&SETTING_BOND_SETTING_NAME));
    s_connection.set_id(Some(bond_ifname));
    s_connection.set_interface_name(Some(bond_ifname));

    // Bond-specific settings
    let bond_mode = get_bond_mode_str(bond_mode);
    if !s_bond.add_option(&SETTING_BOND_OPTION_MODE, bond_mode) {
        error!("Unable to set bond mode option to \"{}\"", bond_mode);
        return Err(anyhow!(
            "Unable to set bond mode option to \"{}\"",
            bond_mode
        ));
    }
    if !s_bond.add_option(&SETTING_BOND_OPTION_MIIMON, "100") {
        error!("Unable to set bond MIIMON option to \"{}\"", "100");
        return Err(anyhow!("Unable to set bond MIIMON option to \"{}\"", "100"));
    }

    connection.add_setting(s_connection);
    connection.add_setting(s_bond);

    Ok(connection)
}

fn create_wired_connection(wired_ifname: &str, bond_ifname: &str) -> Result<SimpleConnection> {
    let connection = SimpleConnection::new();

    let s_connection = SettingConnection::new();

    // General settings
    s_connection.set_type(Some(&SETTING_WIRED_SETTING_NAME));
    s_connection.set_id(Some(wired_ifname));
    s_connection.set_interface_name(Some(wired_ifname));

    // Master is bond interface name, slave type is type of master interface (i.e. bond)
    s_connection.set_master(Some(bond_ifname));
    s_connection.set_slave_type(Some(&SETTING_BOND_SETTING_NAME));

    connection.add_setting(s_connection);

    Ok(connection)
}

// Find connection of specified DeviceType with matching ifname
//
// Will continue to search for connections with matching ifnames after match found
// This done to enable verbose logging
fn get_connection(
    client: &Client,
    ifname: &str,
    device_type: DeviceType,
) -> Option<RemoteConnection> {
    debug!("Searching for connection with ifname \"{}\"", ifname);

    // Only Bond and Ethernet DeviceType supported
    if device_type != DeviceType::Bond && device_type != DeviceType::Ethernet {
        error!(
            "Unsupported device type \"{}\" for get_connection()",
            device_type
        );
        return None;
    }

    let mut matching_conn: Option<RemoteConnection> = None;

    for remote_conn in client.connections().into_iter() {
        let conn = remote_conn.upcast::<Connection>();

        // Get connection interface name for logging
        let conn_ifname = match conn.interface_name() {
            Some(c_ifname) => c_ifname,
            None => {
                error!(
                    "Unable to get connection interface name for connection {}",
                    conn
                );
                continue;
            }
        };

        let found_matching = match device_type {
            DeviceType::Bond => matching_bond_connection(&conn, ifname),
            DeviceType::Ethernet => matching_wired_connection(&conn, ifname),
            _ => {
                // Should never get here given check at beginning of func
                panic!(
                    "Unsupported device type \"{}\" for get_connection()",
                    device_type
                )
            }
        };

        if found_matching && matching_conn.is_none() {
            // Found matching for first time. Save matching and continue
            // to log any other connections with the same interface name
            info!(
                "Found connection with matching interface name \"{}\"",
                ifname
            );

            let remote_conn = match conn.downcast::<RemoteConnection>() {
                Ok(c) => c,
                Err(_) => {
                    error!("Unable to downcast Connection to RemoteConnection");
                    continue;
                }
            };

            matching_conn = Some(remote_conn);
        } else if found_matching {
            // Already found and saved a matching connection, log any further connections
            debug!(
                "Ignoring duplicate connection with matching interface name \"{}\"",
                ifname
            );
        } else {
            debug!(
                "Skipping non-matching connection with interface name \"{}\"",
                conn_ifname
            );
        }
    }

    matching_conn
}

fn get_active_connection(
    client: &Client,
    ifname: &str,
    // conn: &SimpleConnection,
    device_type: DeviceType,
) -> Option<ActiveConnection> {
    debug!("Searching for active connection with ifname \"{}\"", ifname);

    let mut matching_conn: Option<ActiveConnection> = None;

    for active_conn in client.active_connections().into_iter() {
        // Convert to Connection, so we can work with it
        let remote_conn = match active_conn.connection() {
            Some(conn) => conn,
            None => {
                error!("Unable to convert ActiveConnection to RemoteConnection for connection \"{:?}\"", active_conn);
                return None;
            }
        };

        let conn = remote_conn.upcast::<Connection>();

        // Get connection interface name for logging
        let conn_ifname = match conn.interface_name() {
            Some(c_ifname) => c_ifname,
            None => {
                error!(
                    "Unable to get connection interface name for connection {}",
                    conn
                );
                continue;
            }
        };

        let found_matching = match device_type {
            DeviceType::Bond => matching_bond_connection(&conn, ifname),
            DeviceType::Ethernet => matching_wired_connection(&conn, ifname),
            _ => {
                // Should never get here given check at beginning of func
                panic!(
                    "Unsupported device type \"{}\" for get_connection()",
                    device_type
                )
            }
        };

        if found_matching && matching_conn.is_none() {
            // Found matching for first time. Save matching and continue
            // to log any other connections with the same interface name
            debug!(
                "Found connection with matching interface name \"{}\"",
                ifname
            );
            matching_conn = Some(active_conn);
        } else if found_matching {
            // Already found and saved a matching connection, log any further connections
            warn!(
                "Ignoring duplicate connection with matching interface name \"{}\"",
                ifname
            );
        } else {
            debug!(
                "Skipping non-matching connection with interface name \"{}\"",
                conn_ifname
            );
        }
    }

    matching_conn
}

fn matching_bond_connection(conn: &Connection, ifname: &str) -> bool {
    let conn_settings = match get_connection_settings(conn) {
        Ok(conn_settings) => conn_settings,
        Err(_) => {
            error!("Unable to get connection settings");
            return false;
        }
    };

    let conn_id = match conn_settings.id() {
        Some(c) => c,
        None => {
            error!("Unable to get connection id");
            return false;
        }
    };
    let conn_id_str = conn_id.as_str();

    let _conn_settings_bond = match conn.setting_bond() {
        Some(c) => {
            debug!("Connection \"{}\" is bond connection", conn_id_str);
            c
        }
        None => {
            debug!("Connection \"{}\" is not bond connection", conn_id_str);
            return false;
        }
    };

    match conn.interface_name() {
        Some(found_ifname) => {
            if found_ifname == ifname {
                debug!(
                    "Bond connection \"{}\" with ifname \"{}\" matches",
                    conn_id_str, found_ifname
                );
                true
            } else {
                debug!(
                    "Bond connection \"{}\" with ifname \"{}\" does not match",
                    conn_id_str, found_ifname
                );
                false
            }
        }
        None => {
            error!("Unable to get interface name");
            false
        }
    }
}

fn matching_wired_connection(conn: &Connection, ifname: &str) -> bool {
    let conn_settings = match get_connection_settings(conn) {
        Ok(conn_settings) => conn_settings,
        Err(_) => {
            error!("Unable to get connection settings");
            return false;
        }
    };

    let conn_id = match conn_settings.id() {
        Some(c) => c,
        None => {
            error!("Unable to get connection id");
            return false;
        }
    };
    let conn_id_str = conn_id.as_str();

    let _conn_settings_wired = match conn.setting_wired() {
        Some(c) => {
            debug!("Connection \"{}\" is wired", conn_id_str);
            c
        }
        None => {
            debug!("Connection \"{}\" is not wired", conn_id_str);
            return false;
        }
    };

    match conn.interface_name() {
        Some(found_ifname) => {
            if found_ifname == ifname {
                debug!(
                    "Wired connection \"{}\" with ifname \"{}\" matches",
                    conn_id_str, found_ifname
                );
                true
            } else {
                debug!(
                    "Wired connection \"{}\" with ifname \"{}\" does not match",
                    conn_id_str, found_ifname
                );
                false
            }
        }
        None => {
            error!("Unable to get interface name");
            false
        }
    }
}

fn get_connection_settings(conn: &Connection) -> Result<SettingConnection> {
    match conn.setting_connection() {
        Some(c) => Ok(c),
        None => {
            error!("Unable to get connection settings");
            Err(anyhow!("Unable to get connection settings"))
        }
    }
}

fn get_bond_mode_str(mode: BondMode) -> &'static str {
    match mode {
        BondMode::RoundRobin => todo!(),
        BondMode::ActiveBackup => "active-backup",
        BondMode::XOR => todo!(),
        BondMode::Broadcast => todo!(),
        BondMode::DynamicLinkAggregation => todo!(),
        BondMode::TransmitLoadBalancing => todo!(),
        BondMode::AdaptiveLoadBalancing => todo!(),
    }
}
