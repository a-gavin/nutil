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
    Unset = -1,
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

        // TODO: Further verification of args depending on operation.
        //       Allow for some empty types (e.g. no mode or backing wired ifnames if getting status of bond)
        //       Will require not throwing errors above if not found and setting sane defaults (e.g. "")
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
        Command::Status { c_type, c_args } => connection_status(&client, c_type, c_args),
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

fn connection_status(
    client: &Client,
    c_type: ConnectionType,
    c_opts: ConnectionOpts,
) -> Result<()> {
    match c_type {
        ConnectionType::Bond => connection_status_bond(&client, c_opts),
        ConnectionType::AccessPoint => todo!(),
    }
}

async fn create_bond(client: &Client, c_opts: ConnectionOpts) -> Result<()> {
    let opts = BondOpts::try_from(c_opts)?;

    // Create bond structs here so we can comprehensively search
    // for any matching existing connection, should it exist
    // Does not add connection to Network Manager, that happens later
    let bond_conn = create_bond_connection(&opts.bond_ifname, opts.bond_mode)?;

    // Make sure a bond connection with same name does not already exist
    // If bond connection using same devices does not exist, good to continue
    // TODO: What if using only one of backing Wired interfaces????
    if get_connection(&client, DeviceType::Bond, &bond_conn).is_some() {
        warn!("Bond connection already exists, quitting...");
        return Err(anyhow!("Bond connection already exists, quitting..."));
    }

    // Deactivate matching active ethernet connections. Otherwise, newly-created bond
    // connection will stay in "Activating" state until backing slave connections are
    // active (which the existing non-slave ethernet connections preempt from doing so).
    info!("Deactivating any existing wired connections which use same interfaces as bond slave wired connection ifnames--\"{}\" and \"{}\"", &opts.wired_ifname1, &opts.wired_ifname2);

    // Create non-bond interface ethernet connections, otherwise searching
    // for the connections may return a bond slave connection. We want the non-bond connection
    let existing_wired_conn1 = create_wired_connection(&opts.wired_ifname1, None)?;
    let existing_wired_conn2 = create_wired_connection(&opts.wired_ifname2, None)?;
    match get_active_connection(&client, DeviceType::Ethernet, &existing_wired_conn1) {
        Some(c1) => {
            client.deactivate_connection_future(&c1).await?;
        }
        None => debug!(
            "No matching active wired connection for interface \"{}\"",
            &opts.wired_ifname1
        ),
    };

    match get_active_connection(&client, DeviceType::Ethernet, &existing_wired_conn2) {
        Some(c2) => {
            client.deactivate_connection_future(&c2).await?;
        }
        None => debug!(
            "No matching active wired connection for interface \"{}\"",
            &opts.wired_ifname2
        ),
    };

    // Check that backing devices for provided wired interfaces exist
    let wired_dev1 = match client.device_by_iface(&opts.wired_ifname1) {
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

    let wired_dev2 = match client.device_by_iface(&opts.wired_ifname2) {
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

    // Bond connection doesn't exist and backing ethernet devices exist,
    // so create new bond connection (using newly-created wired connections
    // which are backed by existing wired devices)
    info!("Creating bond connection");
    let wired_conn1 = create_wired_connection(&opts.wired_ifname1, Some(&opts.bond_ifname))?;
    let wired_conn2 = create_wired_connection(&opts.wired_ifname2, Some(&opts.bond_ifname))?;

    client.add_connection_future(&bond_conn, true).await?;
    let wired_conn1 = client.add_connection_future(&wired_conn1, true).await?;
    let wired_conn2 = client.add_connection_future(&wired_conn2, true).await?;

    // Connections are created, connect backing devices to enable the connections.
    // If everything is normal, adding the connections should activate them as
    // we have already downed any other connections that were using the backing devices.
    //
    // On off chance that devices are deactivating using the `ip link set down`
    // command, for example, this will reactivate the devices.
    //
    // Non-Network Manager deactivating devices thru software will result in NetworkManager
    // not realizing that the devices or connections are inactive. Simply re-activating
    // the connection will reset this, assuming no other software gets in the way.
    info!(
        "Activating bond slave wired connections with ifnames \"{}\" and \"{}\"",
        &opts.wired_ifname1, &opts.wired_ifname2
    );
    client
        .activate_connection_future(Some(&wired_conn1), Some(&wired_dev1), None)
        .await?;
    client
        .activate_connection_future(Some(&wired_conn2), Some(&wired_dev2), None)
        .await?;

    Ok(())
}

async fn delete_bond(client: &Client, c_opts: ConnectionOpts) -> Result<()> {
    let opts = BondOpts::try_from(c_opts)?;

    let bond_conn = create_bond_connection(&opts.bond_ifname, opts.bond_mode)?;
    let wired_conn1 = create_wired_connection(&opts.wired_ifname1, Some(&opts.bond_ifname))?;
    let wired_conn2 = create_wired_connection(&opts.wired_ifname2, Some(&opts.bond_ifname))?;

    // Deactivate bond connection
    // Automatically deactivates slave connections on success
    info!("Deactivating bond connection with interface \"{}\" (and associated slave wired connections)", opts.bond_ifname);
    match get_active_connection(&client, DeviceType::Bond, &bond_conn) {
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
    match get_connection(&client, DeviceType::Bond, &bond_conn) {
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
    match get_connection(&client, DeviceType::Ethernet, &wired_conn1) {
        Some(c) => c.delete_future().await?,
        None => {
            return Err(anyhow!(
                "Required wired connection \"{}\" does not exist, quitting...",
                &opts.wired_ifname1
            ));
        }
    };

    // Delete second wired slave connection
    match get_connection(&client, DeviceType::Ethernet, &wired_conn2) {
        Some(c) => c.delete_future().await?,
        None => {
            return Err(anyhow!(
                "Required wired connection \"{}\" does not exist, quitting...",
                &opts.wired_ifname2
            ));
        }
    };

    // TODO: Up any previously-existing interfaces?

    Ok(())
}

fn connection_status_bond(client: &Client, c_opts: ConnectionOpts) -> Result<()> {
    let opts = BondOpts::try_from(c_opts)?;

    // Create bond struct here so we can comprehensively search
    // for any matching existing connection, should it exist
    // Does not add connection to Network Manager, that happens later
    let bond_conn = create_bond_connection(&opts.bond_ifname, opts.bond_mode)?;

    // Only possibly active, so assume deactivated until proven otherwise
    let mut conn_state: ActiveConnectionState = ActiveConnectionState::Deactivated;
    let mut ip4_addr_strs: Vec<String> = vec![];
    match get_active_connection(&client, DeviceType::Bond, &bond_conn) {
        Some(c) => {
            conn_state = c.state();

            // Gather active IPv4 info
            if let Some(cfg) = c.ip4_config() {
                // Active IPv4 addresses (i.e. non-NetworkManager configured)
                for ip4_addr in cfg.addresses() {
                    let addr = ip4_addr.address().unwrap(); // TODO
                    let addr_str = addr.as_str();
                    ip4_addr_strs.push(format!("{}\t(active)", addr_str));
                }
            } else {
                // Expected when bond is waiting to get IP information.
                // Possible when backing devices are used for other
                // non-bond child connections but bond connection is active
                warn!(
                    "Unable to get IPv4 config for active bond connection \"{}\"",
                    opts.bond_ifname
                )
            }
        }
        None => (),
    };

    // Try to get connection that matches what we want from NetworkManager
    // If it doesn't exist, no sense continuing
    let bond_remote_conn = match get_connection(&client, DeviceType::Bond, &bond_conn) {
        Some(c) => c,
        None => {
            error!("Bond connection \"{}\" does not exist", &opts.bond_ifname);
            return Err(anyhow!(
                "Bond connection \"{}\" does not exist",
                &opts.bond_ifname
            ));
        }
    };
    let bond_conn = bond_remote_conn.upcast::<Connection>();

    // Gather bond static info
    let bond_ip4_settings = match bond_conn.setting_ip4_config() {
        Some(c) => c,
        None => {
            error!("Unable to get connection ip4 settings");
            return Err(anyhow!("Unable to get connection ip4 settings"));
        }
    };

    let ip4_method_gstr = match bond_ip4_settings.method() {
        Some(m) => m,
        None => return Err(anyhow!("")), // TODO
    };
    let ip4_method = ip4_method_gstr.as_str();

    // Static IPv4 addresses
    for ix in 0..bond_ip4_settings.num_addresses() {
        match bond_ip4_settings.address(ix as i32) {
            // Why does this take a signed int lmao
            Some(c) => match c.address() {
                Some(addr) => {
                    ip4_addr_strs.push(format!("{}\t(static)", addr));
                }
                None => warn!("Unable to get address string with index \"{}\"", ix),
            },
            None => warn!("Unable to get address with index \"{}\"", ix),
        }
    }

    let child_conns = get_child_connections(&client, &opts.bond_ifname, DeviceType::Ethernet);

    // Begin printing status info
    println!("Name:\t\t{}", &opts.bond_ifname);
    println!("Active:\t\t{}", get_connection_state_str(conn_state));

    // Backing connections/devices
    print!("Slave devices:");
    if let Some(child_conns) = child_conns {
        if child_conns.len() == 0 {
            // Print first addr on same line, but if no addrs, need newline
            println!();
        }

        let mut slave_ifnames: Vec<String> = vec![];
        for conn in child_conns {
            match conn.setting_connection() {
                Some(setting) => {
                    if let Some(slave_ifname) = setting.interface_name() {
                        slave_ifnames.push(format!("{}", slave_ifname.as_str()));
                    }
                }
                None => {
                    // TODO: Log err
                }
            }
        }

        for (ix, ifname) in slave_ifnames.iter().enumerate() {
            if ix == 0 {
                // Print first ifname on same line as "Slave devices"
                println!("\t{}", ifname);
                continue;
            }
            println!("\t\t{}", ifname);
        }
    }

    // IPv4 status info
    println!("IPv4:");
    println!("  Method:\t{}", ip4_method);

    print!("  Addresses:");
    if ip4_addr_strs.len() == 0 {
        // Print first addr on same line, but if no addrs, need newline
        println!();
    }
    for (ix, addr) in ip4_addr_strs.iter().enumerate() {
        if ix == 0 {
            // Print first IP addr on same line as "Addresses"
            println!("\t{}", addr);
            continue;
        }
        println!("\t\t{}", addr);
    }

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

// Create a wired SimpleConnection for use in activating, deactivating, finding, etc
// If bond_ifname is Some, create the wired connection as a bond slave with bond_ifname as master.
//
// NOTE: SimpleConnection are owned by this program. ActiveConnection and RemoteConnection
//       are owned by the NetworkManager library
fn create_wired_connection(
    wired_ifname: &str,
    bond_ifname: Option<&str>,
) -> Result<SimpleConnection> {
    let connection = SimpleConnection::new();

    let s_connection = SettingConnection::new();

    // General settings
    s_connection.set_type(Some(&SETTING_WIRED_SETTING_NAME));
    s_connection.set_id(Some(wired_ifname));
    s_connection.set_interface_name(Some(wired_ifname));

    // Master is bond interface name, slave type is type of master interface (i.e. bond)
    if let Some(bond_ifname) = bond_ifname {
        s_connection.set_master(Some(bond_ifname));
        s_connection.set_slave_type(Some(&SETTING_BOND_SETTING_NAME));
    }

    connection.add_setting(s_connection);

    Ok(connection)
}

// Search for connection that matches the specified
// device type and properties in provided connection.
//
// Will continue to search for connections with matching ifnames after match found
// This done to enable verbose logging
fn get_connection(
    client: &Client,
    device_type: DeviceType,
    conn: &SimpleConnection,
) -> Option<RemoteConnection> {
    let ifname = conn.interface_name()?;
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

    for cmp_remote_conn in client.connections().into_iter() {
        let cmp_conn = cmp_remote_conn.upcast::<Connection>();

        // Get connection interface name for logging
        let cmp_conn_ifname = match cmp_conn.interface_name() {
            Some(c) => c,
            None => {
                error!(
                    "Unable to get connection interface name for connection {}",
                    cmp_conn
                );
                continue;
            }
        };

        let found_matching = match device_type {
            DeviceType::Bond => matching_bond_connection(&conn, &cmp_conn),
            DeviceType::Ethernet => matching_wired_connection(&conn, &cmp_conn),
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

            let cmp_remote_conn = match cmp_conn.downcast::<RemoteConnection>() {
                Ok(c) => c,
                Err(_) => {
                    error!("Unable to downcast Connection to RemoteConnection");
                    continue;
                }
            };

            matching_conn = Some(cmp_remote_conn);
        } else if found_matching {
            // Already found and saved a matching connection, log any further connections
            debug!(
                "Ignoring duplicate connection with matching interface name \"{}\"",
                ifname
            );
        } else {
            debug!(
                "Skipping non-matching connection with interface name \"{}\"",
                cmp_conn_ifname
            );
        }
    }

    matching_conn
}

// Search for active connection that matches the specified
// device type and properties in provided connection.
//
// Will continue to search for connections with matching ifnames after match found
// This done to enable verbose logging
fn get_active_connection(
    client: &Client,
    device_type: DeviceType,
    conn: &SimpleConnection,
) -> Option<ActiveConnection> {
    let ifname = conn.interface_name()?;
    debug!("Searching for active connection with ifname \"{}\"", ifname);

    let mut matching_conn: Option<ActiveConnection> = None;

    for cmp_active_conn in client.active_connections().into_iter() {
        // Convert to Connection, so we can work with it
        let cmp_remote_conn = match cmp_active_conn.connection() {
            Some(c) => c,
            None => {
                error!("Unable to convert ActiveConnection to RemoteConnection for connection \"{:?}\"", cmp_active_conn);
                return None;
            }
        };

        let cmp_conn = cmp_remote_conn.upcast::<Connection>();

        // Get connection interface name for logging
        let cmp_conn_ifname = match cmp_conn.interface_name() {
            Some(c) => c,
            None => {
                error!(
                    "Unable to get connection interface name for connection {}",
                    cmp_conn
                );
                continue;
            }
        };

        let found_matching = match device_type {
            DeviceType::Bond => matching_bond_connection(&conn, &cmp_conn),
            DeviceType::Ethernet => matching_wired_connection(&conn, &cmp_conn),
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
            matching_conn = Some(cmp_active_conn);
        } else if found_matching {
            // Already found and saved a matching connection, log any further connections
            warn!(
                "Ignoring duplicate connection with matching interface name \"{}\"",
                ifname
            );
        } else {
            debug!(
                "Skipping non-matching connection with interface name \"{}\"",
                cmp_conn_ifname
            );
        }
    }

    matching_conn
}

fn get_child_connections(
    client: &Client,
    master_ifname: &str,
    child_device_type: DeviceType,
) -> Option<Vec<RemoteConnection>> {
    debug!(
        "Searching for child connection with parent ifname \"{}\"",
        master_ifname
    );

    // Only Ethernet DeviceType supported
    if child_device_type != DeviceType::Ethernet {
        error!(
            "Unsupported device type \"{}\" for get_connection()",
            child_device_type
        );
        return None;
    }

    let mut child_conns: Vec<RemoteConnection> = vec![];

    // Iterate through connections attempting to match connection's master ifname with provided
    for conn in client.connections().into_iter() {
        let conn = conn.upcast::<Connection>();

        let conn_settings = match conn.setting_connection() {
            Some(c) => c,
            None => {
                error!("Unable to get connection settings");
                continue;
            }
        };

        let conn_id = match conn_settings.id() {
            Some(c) => c,
            None => {
                error!("Unable to get connection id");
                return None;
            }
        };
        let conn_id_str = conn_id.as_str();

        if conn.setting_wired().is_none() {
            debug!("Skipping non-wired connection \"{}\"", conn_id_str);
            continue;
        }

        match conn_settings.master() {
            Some(conn_master) => {
                if conn_master != master_ifname {
                    debug!(
                        "Master interface \"{}\" for connection \"{}\" does not match desired master interface \"{}\"",
                        conn_master, conn_id_str, master_ifname
                    );
                } else {
                    debug!(
                        "Master interface \"{}\" for connection \"{}\" matches desired master interface \"{}\"",
                        conn_master, conn_id_str, master_ifname
                    );
                    child_conns.push(conn.downcast::<RemoteConnection>().unwrap());
                    // TODO: Revisit this. Should always be okay?
                }
            }
            None => {
                debug!("Skipping connection without master \"{}\"", conn_id_str);
                continue;
            }
        }
    }

    Some(child_conns)
}

// Determine if provided connection for comparison `cmp_conn` is a bond connection
// and matches desired connection `conn`
//
// Don't compare granular settings like bond mode, miimon, or backing network devices,
// just backing interface name
fn matching_bond_connection(conn: &SimpleConnection, cmp_conn: &Connection) -> bool {
    // Get SettingConnection obj for both connection and compared connection
    let conn_settings = match conn.setting_connection() {
        Some(c) => c,
        None => {
            error!("Unable to get connection settings");
            return false;
        }
    };

    // Get connection id for compared connection
    let cmp_conn_id = match conn_settings.id() {
        Some(c) => c,
        None => {
            error!("Unable to get connection id");
            return false;
        }
    };
    let cmp_conn_id_str = cmp_conn_id.as_str();

    // Ensure compared connection is a bond (assume connection desired is a bond)
    match cmp_conn.setting_bond() {
        Some(c) => {
            debug!("Connection \"{}\" is bond connection", cmp_conn_id_str);
            c
        }
        None => {
            debug!("Connection \"{}\" is not bond connection", cmp_conn_id_str);
            return false;
        }
    };

    // Get ifname for both bond connections
    let conn_ifname = match conn.interface_name() {
        Some(ifname) => ifname,
        None => {
            error!("Unable to get interface name");
            return false;
        }
    };

    let cmp_conn_ifname = match cmp_conn.interface_name() {
        Some(ifname) => ifname,
        None => {
            error!("Unable to get interface name");
            return false;
        }
    };

    // Compare backing ifnames
    if conn_ifname != cmp_conn_ifname {
        debug!(
            "Connection \"{}\" ifname \"{}\" does not match desired ifname \"{}\"",
            cmp_conn_id_str, cmp_conn_ifname, conn_ifname
        );
        return false;
    }

    true
}

// Determine if provided connection for comparison `cmp_conn` is a bond connection
// and matches desired connection `conn`
//
// In addition to comparing backing interface name, also compare slave settings
// (e.g. master name, slave type) if connection is determined to be a slave connection.
fn matching_wired_connection(conn: &SimpleConnection, cmp_conn: &Connection) -> bool {
    // Get SettingConnection obj for both connection and compared connection
    let conn_settings = match conn.setting_connection() {
        Some(c) => c,
        None => {
            error!("Unable to get connection settings");
            return false;
        }
    };

    let cmp_conn_settings = match cmp_conn.setting_connection() {
        Some(c) => c,
        None => {
            error!("Unable to get connection settings");
            return false;
        }
    };

    // Get connection id for compared connection
    let conn_id = match conn_settings.id() {
        Some(c) => c,
        None => {
            error!("Unable to get connection id");
            return false;
        }
    };
    let conn_id_str = conn_id.as_str();

    let cmp_conn_id = match cmp_conn_settings.id() {
        Some(c) => c,
        None => {
            error!("Unable to get connection id");
            return false;
        }
    };
    let cmp_conn_id_str = cmp_conn_id.as_str();

    // Ensure compared connection is wired (assume connection desired is wired)
    match cmp_conn.setting_wired() {
        Some(c) => {
            debug!("Connection \"{}\" is wired", cmp_conn_id_str);
            c
        }
        None => {
            debug!("Connection \"{}\" is not wired", cmp_conn_id_str);
            return false;
        }
    };

    // Get ifname for both wired connections
    let conn_ifname = match conn.interface_name() {
        Some(c) => c,
        None => {
            error!("Unable to get interface name");
            return false;
        }
    };

    let cmp_conn_ifname = match cmp_conn.interface_name() {
        Some(c) => c,
        None => {
            error!("Unable to get interface name");
            return false;
        }
    };

    // Compare backing ifnames
    if conn_ifname != cmp_conn_ifname {
        debug!(
            "Connection \"{}\" ifname \"{}\" does not match desired ifname \"{}\"",
            cmp_conn_id_str, cmp_conn_ifname, conn_ifname
        );
        return false;
    }

    // Compare both's master connections, if either is a slave connection
    let conn_master = conn_settings.master();
    let cmp_conn_master = conn_settings.master();

    if conn_master.is_none() && cmp_conn_master.is_some() {
        debug!(
            "Connection \"{}\" is not a slave device but compared connection \"{}\" is",
            conn_id_str, cmp_conn_id_str
        );
        return false;
    } else if conn_master.is_some() && cmp_conn_master.is_none() {
        debug!(
            "Connection \"{}\" is a slave device but compared connection \"{}\" is not",
            conn_id_str, cmp_conn_id_str
        );
        return false;
    }

    if conn_master.is_some() && cmp_conn_master.is_some() {
        let conn_master = conn_master.unwrap();
        let cmp_conn_master = cmp_conn_master.unwrap();

        if conn_master != cmp_conn_master {
            debug!(
                "Connection \"{}\" and compared connection \"{}\" have different master devices",
                conn_id_str, cmp_conn_id_str
            );
            return false;
        }
    }

    // Determine if either connection is a slave
    let conn_slave_type = conn_settings.slave_type();
    let cmp_conn_slave_type = cmp_conn_settings.slave_type();

    if conn_slave_type.is_none() && cmp_conn_slave_type.is_some() {
        debug!(
            "Connection \"{}\" is not a slave but compared connection \"{}\" is",
            conn_id_str, cmp_conn_id_str
        );
        return false;
    } else if conn_slave_type.is_some() && cmp_conn_slave_type.is_none() {
        debug!(
            "Connection \"{}\" is a slave but compared connection \"{}\" is not",
            conn_id_str, cmp_conn_id_str
        );
        return false;
    }

    // Both connections are slaves, compare slave type
    if conn_slave_type.is_some() && cmp_conn_slave_type.is_some() {
        let conn_slave_type = conn_slave_type.unwrap();
        let cmp_conn_slave_type = cmp_conn_slave_type.unwrap();

        if conn_slave_type != cmp_conn_slave_type {
            debug!(
                "Connection \"{}\" has different slave type than compared connection \"{}\"",
                conn_id_str, cmp_conn_id_str
            );
            return false;
        }
    }

    true
}

fn get_bond_mode_str(mode: BondMode) -> &'static str {
    match mode {
        BondMode::Unset => "NOT SET",
        BondMode::RoundRobin => todo!(),
        BondMode::ActiveBackup => "active-backup",
        BondMode::XOR => todo!(),
        BondMode::Broadcast => todo!(),
        BondMode::DynamicLinkAggregation => todo!(),
        BondMode::TransmitLoadBalancing => todo!(),
        BondMode::AdaptiveLoadBalancing => todo!(),
    }
}

fn get_connection_state_str(state: ActiveConnectionState) -> &'static str {
    match state {
        ActiveConnectionState::Activated => "activated",
        ActiveConnectionState::Activating => "activating",
        ActiveConnectionState::Deactivated => "deactivated",
        ActiveConnectionState::Deactivating => "deactivating",
        ActiveConnectionState::Unknown => "unknown",
        _ => panic!("Unexpected connection state \"{}\"", state),
    }
}
