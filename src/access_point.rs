use std::fs::File;
use std::rc::Rc;
use std::str;
use std::str::FromStr;
use std::{cell::RefCell, io::Read};

use anyhow::{anyhow, Result};
use futures_channel::oneshot;
use glib::translate::FromGlib;
use ipnet::Ipv4Net;
use nm::*;
use serde::Deserialize;
use tracing::{debug, info, instrument, warn};

use crate::{
    cli::AccessPointArgs,
    connection::{get_active_connection, get_connection, get_connection_state_str},
    station::create_sta_connection,
    util::{deserialize_password, DEFAULT_IP4_ADDR},
};

#[derive(Default, Deserialize, PartialEq, Clone, Debug)]
pub struct AccessPointOpts {
    #[serde(rename = "wireless_interface")]
    #[serde(default)]
    #[serde(with = "serde_with::rust::string_empty_as_none")]
    pub wireless_ifname: Option<String>,

    #[serde(default)]
    #[serde(with = "serde_with::rust::string_empty_as_none")]
    pub ssid: Option<String>,

    /// Must be 8 characters or longer
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_password")]
    pub password: Option<String>,

    #[serde(default)]
    #[serde(with = "serde_with::rust::string_empty_as_none")]
    pub ip4_addr: Option<String>,
}

impl TryFrom<AccessPointArgs> for AccessPointOpts {
    type Error = anyhow::Error;

    fn try_from(args: AccessPointArgs) -> Result<Self, Self::Error> {
        if let Some(cfg) = args.config {
            let mut buf = vec![];
            let mut cfg_file = File::open(cfg)?;
            cfg_file.read_to_end(&mut buf)?;

            let config = str::from_utf8(buf.as_slice())?;
            return parse_access_point_opts(config);
        }

        Ok(AccessPointOpts {
            wireless_ifname: args.wireless_ifname,
            ssid: args.ssid,
            ip4_addr: args.ip4_addr,
            password: args.password,
        })
    }
}

fn parse_access_point_opts(config: &str) -> Result<AccessPointOpts> {
    let opts: AccessPointOpts = serde_yaml::from_str(config)?;
    Ok(opts)
}

#[instrument(skip(client), err)]
pub async fn create_access_point(client: &Client, opts: AccessPointOpts) -> Result<()> {
    let wireless_ifname = match &opts.wireless_ifname {
        Some(ifname) => ifname,
        None => return Err(anyhow!("Required wireless interface not specified")),
    };

    let ssid = match &opts.ssid {
        Some(ssid) => ssid,
        None => return Err(anyhow!("Required SSID not specified")),
    };

    // Create AP struct here so we can comprehensively search
    // for any matching existing connection, should it exist
    // Does not add connection to Network Manager, that happens later
    let ap_conn = create_access_point_connection(&opts)?;

    // Make sure an AP connection with same name does not already exist
    // If bond connection using same devices does not exist, good to continue
    if get_connection(client, DeviceType::Wifi, &ap_conn).is_some() {
        return Err(anyhow!(
            "Access point connection already exists, quitting..."
        ));
    }

    // Check for and deactivate any existing active station connections
    // which share the same wireless interface.
    //
    // Station connection added for searching purposes. Does not add
    // connection to Network Manager, it is purely local
    let sta_conn = create_sta_connection(&opts.clone().into())?;

    match get_active_connection(client, DeviceType::Wifi, &sta_conn) {
        Some(c) => {
            debug!(
                "Found active wireless connection with ifname \"{}\", deactivating",
                wireless_ifname
            );
            client.deactivate_connection_future(&c).await?;
        }
        None => debug!(
            "No matching active wireless connections for interface \"{}\"",
            wireless_ifname
        ),
    };

    let wireless_dev = match client.device_by_iface(wireless_ifname.as_str()) {
        Some(device) => device,
        None => {
            return Err(anyhow!(
                "Wireless device \"{}\" does not exist, quitting...",
                wireless_ifname
            ));
        }
    };

    info!("Creating access point connection \"{}\"", ssid);
    let ap_conn = client.add_connection_future(&ap_conn, true).await?;

    info!("Activating access point connection \"{}\"", ssid);
    let ap_conn = client
        .activate_connection_future(Some(&ap_conn), Some(&wireless_dev), None)
        .await?;

    // Poll until AP is fully activated
    let (sender, receiver) = oneshot::channel::<Result<()>>();
    let sender = Rc::new(RefCell::new(Some(sender)));

    // TODO: Impl timeout
    ap_conn.connect_state_changed(move |_, state, _| {
        let sender = sender.clone();

        glib::MainContext::ref_thread_default().spawn_local(async move {
            let state = unsafe { ActiveConnectionState::from_glib(state as _) };
            debug!("Connection state: {}", get_connection_state_str(state));

            let exit = match state {
                ActiveConnectionState::Activating => None,
                ActiveConnectionState::Activated => Some(Ok(())),
                _ => Some(Err(anyhow!("Unexpected connection state"))),
            };

            if let Some(result) = exit {
                let sender = sender.borrow_mut().take();

                if let Some(sender) = sender {
                    sender.send(result).expect("Sender dropped");
                }
            }
        });
    });

    let res = receiver.await?;

    if res.is_ok() {
        info!("Activated access point connection \"{}\"", ssid);
    }
    res
}

#[instrument(skip(client), err)]
pub async fn delete_access_point(client: &Client, opts: AccessPointOpts) -> Result<()> {
    let wireless_ifname = match &opts.wireless_ifname {
        Some(ifname) => ifname,
        None => return Err(anyhow!("Required wireless interface not specified")),
    };

    let ssid = match &opts.ssid {
        Some(ssid) => ssid,
        None => return Err(anyhow!("Required SSID not specified")),
    };

    let ap_conn = create_access_point_connection(&opts)?;

    // Use created SimpleConnection to find matching connections from NetworkManager
    let ap_remote_conn = match get_connection(client, DeviceType::Wifi, &ap_conn) {
        Some(c) => c,
        None => {
            return Err(anyhow!(
                "Required access point connection \"{}\" does not exist, quitting...",
                &ssid
            ));
        }
    };

    // Deactivate access_point connection
    // Automatically deactivates slave connections on success
    info!(
        "Deactivating access point connection \"{}\" with interface \"{}\"",
        ssid, wireless_ifname
    );
    match get_active_connection(client, DeviceType::Wifi, &ap_conn) {
        Some(c) => {
            client.deactivate_connection_future(&c).await?;
            info!("Access point connection deactivated");
        }
        None => {
            info!(
                "Required access point connection \"{}\" is not active",
                &ssid
            );
        }
    };

    // Delete access_point connection
    info!(
        "Deleting access point connection \"{}\" with interface \"{}\"",
        ssid, wireless_ifname,
    );
    ap_remote_conn.delete_future().await?;
    info!("Access point connection deleted");

    Ok(())
}

#[instrument(skip(client), err)]
pub fn access_point_status(client: &Client, opts: AccessPointOpts) -> Result<()> {
    let ssid = match &opts.ssid {
        Some(ssid) => ssid,
        None => return Err(anyhow!("Required SSID not specified")),
    };

    // Create AP struct here so we can comprehensively search
    // for any matching existing connection, should it exist
    // Does not add connection to Network Manager, that happens later
    let ap_conn = create_access_point_connection(&opts)?;

    // Only possibly active, so assume deactivated until proven otherwise
    let mut conn_state: ActiveConnectionState = ActiveConnectionState::Deactivated;
    let mut ip4_addr_strs: Vec<String> = vec![];
    if let Some(c) = get_active_connection(client, DeviceType::Wifi, &ap_conn) {
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
                "Unable to get IPv4 config for active access point connection \"{}\"",
                ssid
            )
        }
    };

    // Try to get connection that matches what we want from NetworkManager
    // If it doesn't exist, no sense continuing
    let bond_remote_conn = match get_connection(client, DeviceType::Wifi, &ap_conn) {
        Some(c) => c,
        None => {
            return Err(anyhow!(
                "Access point connection \"{}\" does not exist",
                ssid
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

    // Begin printing status info
    println!("Name:\t\t{}", &ssid);
    println!("Type:\t\taccess point");
    println!("Active:\t\t{}", get_connection_state_str(conn_state));

    // IPv4 status info
    println!("IPv4:");
    println!("  Method:\t{}", ip4_method);

    print!("  Addresses:");
    if ip4_addr_strs.is_empty() {
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

pub fn create_access_point_connection(opts: &AccessPointOpts) -> Result<SimpleConnection> {
    let connection = SimpleConnection::new();

    let s_connection = SettingConnection::new();
    let s_wireless = SettingWireless::new();
    let s_ip4 = SettingIP4Config::new();

    // General connection settings
    s_connection.set_type(Some(SETTING_WIRELESS_SETTING_NAME));
    s_connection.set_autoconnect(false);

    match &opts.ssid {
        Some(ssid) => {
            s_connection.set_id(Some(ssid));
        }
        None => return Err(anyhow!("Required SSID not specified")),
    };

    // TODO: Allow for this to be None. That way user doesn't need to
    // specify interface for deletion/status as it's rly not required
    // Allows for more interesting matching as well
    match &opts.wireless_ifname {
        Some(ifname) => s_connection.set_interface_name(Some(ifname)),
        None => return Err(anyhow!("Required wireless interface not specified")),
    };

    // Wifi settings
    //s_wireless.set_band(Some("bg"));
    s_wireless.set_hidden(false);
    s_wireless.set_mode(Some(SETTING_WIRELESS_MODE_AP));

    match &opts.ssid {
        Some(ssid) => {
            s_wireless.set_ssid(Some(&(ssid.as_bytes().into())));
        }
        None => return Err(anyhow!("Required SSID not specified")),
    };

    // Wifi security settings
    if let Some(password) = &opts.password {
        let s_wireless_security = SettingWirelessSecurity::new();
        s_wireless_security.set_key_mgmt(Some("wpa-psk"));
        s_wireless_security.set_psk(Some(password));
        connection.add_setting(s_wireless_security);
    }

    // IPv4 settings
    let ip4_net = match &opts.ip4_addr {
        Some(addr) => Ipv4Net::from_str(addr)?,
        None => Ipv4Net::from_str(DEFAULT_IP4_ADDR)?,
    };
    let ip4_addr = IPAddress::new(
        libc::AF_INET,
        ip4_net.addr().to_string().as_str(),
        ip4_net.prefix_len() as u32,
    )?;

    s_ip4.add_address(&ip4_addr);
    s_ip4.set_method(Some(SETTING_IP4_CONFIG_METHOD_MANUAL));

    connection.add_setting(s_connection);
    connection.add_setting(s_wireless);
    connection.add_setting(s_ip4);

    Ok(connection)
}

#[cfg(test)]
mod test {
    use super::*;

    // Expect empty interface which should be caught later on
    // when attempting to create connection
    #[test]
    fn no_wireless_interface() {
        let cfg = "
            ssid: \"test_ssid\"
            password: \"test_password\"
            ip4_addr: \"172.16.0.1/24\"
        ";

        let opts = parse_access_point_opts(cfg).unwrap();
        assert!(opts.wireless_ifname.is_none());
    }

    #[test]
    fn empty_wireless_interface() {
        let cfg = "
            wireless_interface: \"\"
            ssid: \"test_ssid\"
            password: \"test_password\"
            ip4_addr: \"172.16.0.1/24\"
        ";

        let opts = parse_access_point_opts(cfg).unwrap();
        assert!(opts.wireless_ifname.is_none());
    }

    #[test]
    fn no_ssid() {
        let cfg = "
            wireless_interface: \"test_interface\"
            password: \"test_password\"
            ip4_addr: \"172.16.0.1/24\"
        ";

        let opts = parse_access_point_opts(cfg).unwrap();
        assert!(opts.ssid.is_none());
    }

    // Expect empty interface which should be caught later on
    // when attempting to create connection
    #[test]
    fn empty_ssid() {
        let cfg = "
            wireless_interface: \"test_interface\"
            ssid: \"\"
            password: \"test_password\"
            ip4_addr: \"172.16.0.1/24\"
        ";

        let opts = parse_access_point_opts(cfg).unwrap();
        assert!(opts.ssid.is_none());
    }

    #[test]
    fn no_password() {
        let cfg = "
            wireless_interface: \"test_interface\"
            ssid: \"test_ssid\"
            ip4_addr: \"172.16.0.1/24\"
        ";

        let opts = parse_access_point_opts(cfg).unwrap();
        assert!(opts.password.is_none())
    }

    #[test]
    fn empty_password() {
        let cfg = "
            wireless_interface: \"test_interface\"
            ssid: \"test_ssid\"
            password: \"\"
            ip4_addr: \"172.16.0.1/24\"
        ";

        let opts = parse_access_point_opts(cfg).unwrap();
        assert!(opts.password.is_none())
    }

    #[test]
    #[should_panic]
    fn less_than_8_char_password() {
        let cfg = "
            wireless_interface: \"test_interface\"
            ssid: \"test_ssid\"
            password: \"123\"
            ip4_addr: \"172.16.0.1/24\"
        ";

        parse_access_point_opts(cfg).unwrap();
    }

    // Make sure use default address/subnet when no ip4_addr specified
    #[test]
    fn no_ip4_addr() {
        let cfg = "
            wireless_interface: \"test_interface\"
            ssid: \"test_ssid\"
            password: \"test_password\"
        ";

        let opts = parse_access_point_opts(cfg).unwrap();
        assert!(opts.ip4_addr.is_none());
    }

    #[test]
    #[should_panic]
    fn empty_ip4_addr() {
        let cfg = "
            wireless_interface: \"test_interface\"
            ssid: \"test_ssid\"
            password: \"123\"
            ip4_addr: \"\"
        ";

        parse_access_point_opts(cfg).unwrap();
    }
}
