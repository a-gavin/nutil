use std::fs::File;

use anyhow::{anyhow, Result};
use clap::ValueEnum;
use nm::*;
use serde::Deserialize;
use tracing::{debug, error, info, instrument, warn};

use crate::cli::BondArgs;
use crate::connection::*;

#[derive(Default, ValueEnum, Deserialize, PartialEq, Clone, Debug)]
pub enum BondMode {
    RoundRobin = 0,
    #[default]
    ActiveBackup = 1,
    XOR = 2,
    Broadcast = 3,
    DynamicLinkAggregation = 4,
    TransmitLoadBalancing = 5,
    AdaptiveLoadBalancing = 6,
}

#[derive(Default, Deserialize, PartialEq, Clone, Debug)]
pub struct BondOpts {
    #[serde(rename = "bond_interface")]
    bond_ifname: String,

    #[serde(default)]
    bond_mode: BondMode,

    #[serde(default, rename = "slave_interfaces")]
    slave_ifnames: Vec<String>,
}

#[instrument(err)]
pub fn parse_bond_opts(config: Option<String>, args: BondArgs) -> Result<BondOpts> {
    match config {
        Some(cfg) => {
            let cfg_file = File::open(cfg)?;
            let opts: BondOpts = serde_yaml::from_reader(cfg_file)?;
            Ok(opts)
        }
        None => BondOpts::try_from(args),
    }
}

impl TryFrom<BondArgs> for BondOpts {
    type Error = anyhow::Error;

    fn try_from(args: BondArgs) -> Result<Self, Self::Error> {
        let bond_ifname = match args.ifname {
            Some(ifname) => ifname,
            None => return Err(anyhow!("Bond interface name not specified")),
        };

        let bond_mode = match args.bond_mode {
            Some(mode) => mode,
            None => {
                info!("Bond mode not specified, defaulting to \"ActiveBackup\"");
                BondMode::ActiveBackup
            }
        };

        if args
            .slave_ifnames
            .iter()
            .map(|name: &String| name == "ANY")
            .any(|x| x)
        {
            return Err(anyhow!("Slave interface name \"ANY\" is reserved"));
        }

        Ok(BondOpts {
            bond_ifname,
            bond_mode,
            slave_ifnames: args.slave_ifnames,
        })
    }
}

#[instrument(skip(client), err)]
pub async fn create_bond(client: &Client, opts: BondOpts) -> Result<()> {
    if opts.slave_ifnames.len() == 0 {
        return Err(anyhow!(
            "One or more slave interfaces required to create a bond connection"
        ));
    }

    // Create bond structs here so we can comprehensively search
    // for any matching existing connection, should it exist
    // Does not add connection to Network Manager, that happens later
    let bond_conn = create_bond_connection(&opts.bond_ifname, opts.bond_mode)?;

    // Make sure a bond connection with same name does not already exist
    // If bond connection using same devices does not exist, good to continue
    if get_connection(&client, DeviceType::Bond, &bond_conn).is_some() {
        return Err(anyhow!("Bond connection already exists, quitting..."));
    }

    // Deactivate matching active ethernet connections. Otherwise, newly-created bond
    // connection will stay in "Activating" state until backing slave connections are
    // active (which the existing non-slave ethernet connections preempt from doing so).
    info!(
        "Deactivating any existing wired connections which use same interfaces as bond \
         slave wired connection ifnames: \"{:?}\"",
        &opts.slave_ifnames
    );

    for slave_ifname in opts.slave_ifnames.iter() {
        let existing_wired_conn = create_wired_connection(slave_ifname, None)?;
        match get_active_connection(&client, DeviceType::Ethernet, &existing_wired_conn) {
            Some(c) => {
                debug!(
                    "Found active standalone wired connection with slave ifname \"{}\", deactivating",
                    slave_ifname
                );
                client.deactivate_connection_future(&c).await?;
                continue;
            }
            None => debug!(
                "No matching active standalone wired connection for interface \"{}\"",
                slave_ifname
            ),
        };

        // If detect an active slave connection with desired slave interface then error and exit
        let existing_wired_conn_slave = create_wired_connection(slave_ifname, Some("ANY"))?;
        match get_active_connection(&client, DeviceType::Ethernet, &existing_wired_conn_slave) {
            Some(_) => {
                return Err(anyhow!(
                    "Found existing slave wired connection with ifname \"{}\" matching desired slave ifname",
                    slave_ifname
                ))
            }
            None => debug!(
                "No matching active slave wired connection for interface \"{}\"",
                slave_ifname
            ),
        };
    }

    // Check that backing devices for provided wired interfaces exist
    let mut wired_devs: Vec<Device> = vec![];
    for slave_ifname in opts.slave_ifnames.iter() {
        let wired_dev = match client.device_by_iface(slave_ifname) {
            Some(device) => device,
            None => {
                return Err(anyhow!(
                    "Wired device \"{}\" does not exist, quitting...",
                    slave_ifname
                ));
            }
        };
        wired_devs.push(wired_dev);
    }

    // Bond connection doesn't exist and backing ethernet devices exist,
    // so create new bond connection (using newly-created wired connections
    // which are backed by existing wired devices)
    info!("Creating bond connection");
    client.add_connection_future(&bond_conn, true).await?;

    for (wired_dev, slave_ifname) in wired_devs.iter().zip(opts.slave_ifnames.iter()) {
        let wired_conn = create_wired_connection(slave_ifname, Some(&opts.bond_ifname))?;

        // Created and configured connection, send it off to NetworkManager
        let wired_conn = client.add_connection_future(&wired_conn, true).await?;

        // Connections are created, connect backing devices to enable the connections.
        // If everything is normal, adding the connections should activate them as
        // we have already downed any other connections that were using the backing devices.
        //
        // On off chance that devices are deactivating using the `ip link set down`
        // command, for example, this will reactivate the devices.
        //
        // Non-Network Manager device deactivation thru software will result in NetworkManager
        // not realizing that the devices or connections are inactive. Simply re-activating
        // the connection will reset this, assuming no other software gets in the way.
        client
            .activate_connection_future(Some(&wired_conn), Some(wired_dev), None)
            .await?;
    }
    Ok(())
}

#[instrument(skip(client), err)]
pub async fn delete_bond(client: &Client, opts: BondOpts) -> Result<()> {
    // Create matching bond SimpleConnection for comparison
    let bond_conn = create_bond_connection(&opts.bond_ifname, opts.bond_mode)?;

    // Use created SimpleConnection to find matching connections from NetworkManager
    let bond_remote_conn = match get_connection(&client, DeviceType::Bond, &bond_conn) {
        Some(c) => c,
        None => {
            return Err(anyhow!(
                "Required bond connection \"{}\" does not exist, quitting...",
                &opts.bond_ifname
            ));
        }
    };

    // Deactivate bond connection
    // Automatically deactivates slave connections on success
    info!("Deactivating bond connection with interface \"{}\" (and associated slave wired connections)", opts.bond_ifname);
    match get_active_connection(&client, DeviceType::Bond, &bond_conn) {
        Some(c) => {
            client.deactivate_connection_future(&c).await?;
            info!("Bond connection and associated interfaces deactivated");
        }
        None => {
            info!(
                "Required bond connection \"{}\" is not active",
                &opts.bond_ifname
            );
        }
    };

    // Delete bond connection
    info!(
        "Deleting bond connection with interface \"{}\"",
        opts.bond_ifname
    );
    bond_remote_conn.delete_future().await?;
    info!("Bond connection deleted");

    // Optionally delete wired slave connections
    for slave_ifname in opts.slave_ifnames.iter() {
        let wired_conn = create_wired_connection(slave_ifname, Some(&opts.bond_ifname))?;

        match get_connection(&client, DeviceType::Ethernet, &wired_conn) {
            Some(c) => c.delete_future().await?,
            None => {
                warn!(
                    "Cannot delete wired connection \"{}\" which doesn't exist",
                    slave_ifname
                );
            }
        };
    }

    Ok(())
}

#[instrument(skip(client), err)]
pub fn bond_status(client: &Client, opts: BondOpts) -> Result<()> {
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
                // non-bond slave connections but bond connection is active
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
            return Err(anyhow!("Unable to get connection ip4 settings"));
        }
    };

    let ip4_method_gstr = match bond_ip4_settings.method() {
        Some(m) => m,
        None => return Err(anyhow!("Unable to get ip4 configuration method")),
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

    let slave_conns = get_slave_connections(&client, &opts.bond_ifname, DeviceType::Ethernet);

    // Begin printing status info
    println!("Name:\t\t{}", &opts.bond_ifname);
    println!("Active:\t\t{}", get_connection_state_str(conn_state));

    // Backing connections/devices
    print!("Slave devices:");
    if let Some(slave_conns) = slave_conns {
        if slave_conns.len() == 0 {
            // Print first addr on same line, but if no addrs, need newline
            println!();
        }

        let mut slave_ifnames: Vec<String> = vec![];
        for (ix, conn) in slave_conns.iter().enumerate() {
            match conn.setting_connection() {
                Some(setting) => {
                    if let Some(slave_ifname) = setting.interface_name() {
                        slave_ifnames.push(format!("{}", slave_ifname.as_str()));
                    }
                }
                None => warn!("Unable to get address string with index \"{}\"", ix),
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

pub fn create_bond_connection(bond_ifname: &str, bond_mode: BondMode) -> Result<SimpleConnection> {
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
