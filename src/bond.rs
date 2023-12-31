use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::str;
use std::str::FromStr;

use anyhow::{anyhow, Result};
use clap::ValueEnum;
use ipnet::Ipv4Net;
use nm::*;
use serde::Deserialize;
use tracing::{debug, error, info, instrument, warn};

use crate::cli::BondArgs;
use crate::connection::*;

#[derive(Default, ValueEnum, Deserialize, PartialEq, Copy, Clone, Debug)]
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
    /// Required for all commands, so no default if unspecified
    #[serde(rename = "bond_interface")]
    #[serde(default)]
    #[serde(with = "serde_with::rust::string_empty_as_none")]
    bond_ifname: Option<String>,

    #[serde(default)]
    bond_mode: BondMode,

    #[serde(default, rename = "slave_interfaces")]
    slave_ifnames: HashSet<String>,

    #[serde(default)]
    #[serde(with = "serde_with::rust::string_empty_as_none")]
    pub ip4_addr: Option<String>,
}

impl TryFrom<BondArgs> for BondOpts {
    type Error = anyhow::Error;

    fn try_from(args: BondArgs) -> Result<Self, Self::Error> {
        if let Some(cfg) = args.config {
            let mut buf = vec![];
            let mut cfg_file = File::open(cfg)?;
            cfg_file.read_to_end(&mut buf)?;

            let config = str::from_utf8(buf.as_slice())?;
            return parse_bond_opts(config);
        }

        let bond_mode = match args.bond_mode {
            Some(mode) => mode,
            None => {
                let mode: BondMode = Default::default();
                info!("Bond mode not specified, defaulting to \"{}\"", get_bond_mode_str(mode));
                mode
            }
        };

        Ok(BondOpts {
            bond_ifname: args.ifname,
            bond_mode,
            slave_ifnames: HashSet::from_iter(args.slave_ifnames.into_iter()),
            ip4_addr: args.ip4_addr,
        })
    }
}

fn parse_bond_opts(config: &str) -> Result<BondOpts> {
    let opts: BondOpts = serde_yaml::from_str(config)?;
    Ok(opts)
}

#[instrument(skip(client), err)]
pub async fn create_bond(client: &Client, opts: BondOpts) -> Result<()> {
    let bond_ifname = match &opts.bond_ifname {
        Some(ifname) => ifname,
        None => return Err(anyhow!("Required bond interface not specified")),
    };

    // Only need to check if no or empty slave ifnames specified.
    // Duplicates taken care of by HashSet, and existence of interface
    // check by NetworkManager itself (which we handle the error of).
    if opts.slave_ifnames.is_empty() {
        return Err(anyhow!(
            "One or more slave interfaces required to create a bond connection"
        ));
    } else if opts.slave_ifnames.iter().any(|c| c.is_empty()) {
        return Err(anyhow!("Empty string is not a valid slave interface name"));
    }

    // Create bond structs here so we can comprehensively search
    // for any matching existing connection, should it exist
    // Does not add connection to Network Manager, that happens later
    let bond_conn = create_bond_connection(&opts)?;

    // Make sure a bond connection with same name does not already exist
    // If bond connection using same devices does not exist, good to continue
    if get_connection(client, DeviceType::Bond, &bond_conn).is_some() {
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
        // Find and deactivate any existing standalone wired connection with same ifname
        let existing_wired_conn = create_wired_connection(slave_ifname, None)?;
        match get_active_connection(client, DeviceType::Ethernet, &existing_wired_conn) {
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

        // Find and deactivate any existing slave wired connection with same ifname
        let existing_wired_conn_slave = create_wired_connection(slave_ifname, Some(""))?;
        match get_active_connection(client, DeviceType::Ethernet, &existing_wired_conn_slave) {
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
    info!("Creating bond connection \"{}\"", bond_ifname);
    client.add_connection_future(&bond_conn, true).await?;

    info!("Activating bond connection \"{}\"", bond_ifname);
    for (wired_dev, slave_ifname) in wired_devs.iter().zip(opts.slave_ifnames.iter()) {
        let wired_conn = create_wired_connection(slave_ifname, Some(bond_ifname))?;

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

    let bond_conn = match get_active_connection(client, DeviceType::Bond, &bond_conn) {
        Some(c) => c,
        None => return Err(anyhow!("Bond connection \"{}\" not active", &bond_ifname)),
    };
    let res = wait_for_connection_to_activate(&bond_conn).await;

    if res.is_ok() {
        info!("Activated bond connection \"{}\"", &bond_ifname);
    }
    res
}

#[instrument(skip(client), err)]
pub async fn delete_bond(client: &Client, opts: BondOpts) -> Result<()> {
    let bond_ifname = match &opts.bond_ifname {
        Some(ifname) => ifname,
        None => return Err(anyhow!("Required bond interface not specified")),
    };

    if opts.slave_ifnames.iter().any(|c| c.is_empty()) {
        return Err(anyhow!("Empty string is not a valid slave interface name"));
    }

    // Create matching bond SimpleConnection for comparison
    let bond_conn = create_bond_connection(&opts)?;

    // Use created SimpleConnection to find matching connections from NetworkManager
    let bond_remote_conn = match get_connection(client, DeviceType::Bond, &bond_conn) {
        Some(c) => c,
        None => {
            return Err(anyhow!(
                "Required bond connection \"{}\" does not exist, quitting...",
                &bond_ifname
            ));
        }
    };

    // Deactivate bond connection
    // Automatically deactivates slave connections on success
    info!("Deactivating bond connection with interface \"{}\" (and associated slave wired connections)", bond_ifname);
    match get_active_connection(client, DeviceType::Bond, &bond_conn) {
        Some(c) => {
            client.deactivate_connection_future(&c).await?;
            info!("Bond connection and associated interfaces deactivated");
        }
        None => {
            info!(
                "Required bond connection \"{}\" is not active",
                &bond_ifname
            );
        }
    };

    // Delete bond connection
    info!(
        "Deleting bond connection with interface \"{}\"",
        bond_ifname
    );
    bond_remote_conn.delete_future().await?;
    info!("Bond connection deleted");

    let slave_conns = get_slave_connections(client, bond_ifname, DeviceType::Ethernet);

    let mut slave_ifnames: Vec<String> = vec![];
    if let Some(slave_conns) = slave_conns {
        for (ix, conn) in slave_conns.iter().enumerate() {
            match conn.setting_connection() {
                Some(setting) => {
                    if let Some(slave_ifname) = setting.interface_name() {
                        slave_ifnames.push(slave_ifname.as_str().to_string());
                    }
                }
                None => warn!("Unable to get address string with index \"{}\"", ix),
            }
        }
    }

    // Optionally delete wired slave connections if associated with bond connection to be deleted
    for slave_ifname in opts.slave_ifnames.iter() {
        let wired_conn = create_wired_connection(slave_ifname, Some(bond_ifname))?;

        if !slave_ifnames.contains(slave_ifname) {
            warn!(
                "Not deleting wired connection \"{}\" which is not associated with bond \"{}\"",
                slave_ifname, bond_ifname
            );
            continue;
        }

        match get_connection(client, DeviceType::Ethernet, &wired_conn) {
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
    let bond_ifname = match &opts.bond_ifname {
        Some(ifname) => ifname,
        None => return Err(anyhow!("Required bond interface not specified")),
    };

    if opts.slave_ifnames.iter().any(|c| c.is_empty()) {
        return Err(anyhow!("Empty string is not a valid slave interface name"));
    }

    // Create bond struct here so we can comprehensively search
    // for any matching existing connection, should it exist
    // Does not add connection to Network Manager, that happens later
    let bond_conn = create_bond_connection(&opts)?;

    // Only possibly active, so assume deactivated until proven otherwise
    let mut conn_state: ActiveConnectionState = ActiveConnectionState::Deactivated;
    let mut ip4_addr_strs: Vec<String> = vec![];
    if let Some(c) = get_active_connection(client, DeviceType::Bond, &bond_conn) {
        conn_state = c.state();

        // Gather active IPv4 info
        if let Some(cfg) = c.ip4_config() {
            // Active IPv4 addresses (i.e. non-NetworkManager configured)
            for ip4_addr in cfg.addresses() {
                let addr = ip4_addr.address().unwrap(); // TODO
                let addr_str = addr.as_str();
                ip4_addr_strs.push(format!("{addr_str}\t(active)"));
            }
        } else {
            // Expected when bond is waiting to get IP information.
            // Possible when backing devices are used for other
            // non-bond slave connections but bond connection is active
            warn!(
                "Unable to get IPv4 config for active bond connection \"{}\"",
                bond_ifname
            )
        }
    };

    // Try to get connection that matches what we want from NetworkManager
    // If it doesn't exist, no sense continuing
    let bond_remote_conn = match get_connection(client, DeviceType::Bond, &bond_conn) {
        Some(c) => c,
        None => {
            return Err(anyhow!(
                "Bond connection \"{}\" does not exist",
                &bond_ifname
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
                    ip4_addr_strs.push(format!("{addr}\t(static)"));
                }
                None => warn!("Unable to get address string with index \"{}\"", ix),
            },
            None => warn!("Unable to get address with index \"{}\"", ix),
        }
    }

    let slave_conns = get_slave_connections(client, bond_ifname, DeviceType::Ethernet);

    // Begin printing status info
    println!("Name:\t\t{}", &bond_ifname);
    println!("Type:\t\tbond");
    println!("Active:\t\t{}", get_connection_state_str(conn_state));

    // Backing connections/devices
    print!("Slave devices:");
    if let Some(slave_conns) = slave_conns {
        if slave_conns.is_empty() {
            // Print first addr on same line, but if no addrs, need newline
            println!();
        }

        let mut slave_ifnames: Vec<String> = vec![];
        for (ix, conn) in slave_conns.iter().enumerate() {
            match conn.setting_connection() {
                Some(setting) => {
                    if let Some(slave_ifname) = setting.interface_name() {
                        slave_ifnames.push(slave_ifname.as_str().to_string());
                    }
                }
                None => warn!("Unable to get address string with index \"{}\"", ix),
            }
        }

        for (ix, ifname) in slave_ifnames.iter().enumerate() {
            if ix == 0 {
                // Print first ifname on same line as "Slave devices"
                println!("\t{ifname}");
                continue;
            }
            println!("\t\t{ifname}");
        }
    }

    // IPv4 status info
    println!("IPv4:");
    println!("  Method:\t{ip4_method}");

    print!("  Addresses:");
    if ip4_addr_strs.is_empty() {
        // Print first addr on same line, but if no addrs, need newline
        println!();
    }
    for (ix, addr) in ip4_addr_strs.iter().enumerate() {
        if ix == 0 {
            // Print first IP addr on same line as "Addresses"
            println!("\t{addr}");
            continue;
        }
        println!("\t\t{addr}");
    }

    Ok(())
}

pub fn create_bond_connection(opts: &BondOpts) -> Result<SimpleConnection> {
    let connection = SimpleConnection::new();

    let s_connection = SettingConnection::new();
    let s_bond = SettingBond::new();
    let s_ip4 = SettingIP4Config::new();

    // General connection settings
    s_connection.set_type(Some(SETTING_BOND_SETTING_NAME));

    match &opts.bond_ifname {
        Some(ifname) => {
            s_connection.set_id(Some(ifname));
            s_connection.set_interface_name(Some(ifname));
        }
        None => return Err(anyhow!("Required bond interface not specified")),
    }

    // Bond-specific settings
    let bond_mode = get_bond_mode_str(opts.bond_mode);
    if !s_bond.add_option(SETTING_BOND_OPTION_MODE, bond_mode) {
        error!("Unable to set bond mode option to \"{}\"", bond_mode);
        return Err(anyhow!(
            "Unable to set bond mode option to \"{}\"",
            bond_mode
        ));
    }
    if !s_bond.add_option(SETTING_BOND_OPTION_MIIMON, "100") {
        error!("Unable to set bond MIIMON option to \"{}\"", "100");
        return Err(anyhow!("Unable to set bond MIIMON option to \"{}\"", "100"));
    }

    // IPv4 settings
    match &opts.ip4_addr {
        Some(addr) => {
            let ip4_net = Ipv4Net::from_str(addr)?;

            let ip4_addr = IPAddress::new(
                libc::AF_INET,
                ip4_net.addr().to_string().as_str(),
                ip4_net.prefix_len() as u32,
            )?;

            s_ip4.add_address(&ip4_addr);
            s_ip4.set_method(Some(SETTING_IP4_CONFIG_METHOD_MANUAL));
        }
        None => {
            s_ip4.set_method(Some(SETTING_IP4_CONFIG_METHOD_AUTO));
        }
    }

    connection.add_setting(s_connection);
    connection.add_setting(s_bond);
    connection.add_setting(s_ip4);

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

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn no_bond_ifname() {
        let cfg = "
            bond_mode: !ActiveBackup
            slave_interfaces:
                - enp2s0
        ";

        let opts = parse_bond_opts(cfg).unwrap();
        assert!(opts.bond_ifname.is_none());
    }

    #[test]
    fn empty_bond_ifname() {
        let cfg = "
            bond_interface: \"\"
            bond_mode: !ActiveBackup
            slave_interfaces:
                - enp2s0
        ";

        let opts = parse_bond_opts(cfg).unwrap();
        assert!(opts.bond_ifname.is_none());
    }

    // Expect to default to BondMode default
    #[test]
    fn no_bond_mode() {
        let cfg = "
            bond_interface: bond0
            slave_interfaces:
                - enp2s0
        ";
        let default_mode: BondMode = Default::default();

        let opts = parse_bond_opts(cfg).unwrap();
        assert_eq!(opts.bond_mode, default_mode);
    }

    #[test]
    #[should_panic]
    fn empty_bond_mode() {
        let cfg = "
            bond_interface: bond0
            bond_mode: \"\"
            slave_interfaces:
                - enp2s0
        ";

        parse_bond_opts(cfg).unwrap();
    }

    #[test]
    #[should_panic]
    fn unexpected_bond_mode() {
        let cfg = "
            bond_interface: bond0
            bond_mode: !UnexpectedMode
            slave_interfaces:
                - enp2s0
        ";

        parse_bond_opts(cfg).unwrap();
    }

    // Command-specific behaviour for "slave_interfaces" field. Create and delete
    // need slave interfaces; status does not. Create and delete thus are required
    // to validate that user specified slave interfaces.
    // Expect empty Vec of interface names when unspecified.
    #[test]
    fn no_bond_slave_interfaces() {
        let cfg = "
            bond_interface: bond0
            bond_mode: !ActiveBackup
        ";

        let opts = parse_bond_opts(cfg).unwrap();
        assert!(opts.slave_ifnames.is_empty());
    }

    // See above "slave_interfaces" comment
    #[test]
    fn empty_slave_interfaces() {
        let cfg = "
            bond_interface: bond0
            bond_mode: !ActiveBackup
            slave_interfaces:
        ";

        let opts = parse_bond_opts(cfg).unwrap();
        assert!(opts.slave_ifnames.is_empty());
    }
}
