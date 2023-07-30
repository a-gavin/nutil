use std::fs::File;
use std::io::Read;
use std::str;
use std::str::FromStr;

use anyhow::{anyhow, Result};
use ipnet::Ipv4Net;
use nm::*;
use serde::Deserialize;
use tracing::{debug, info, instrument};

use crate::{
    access_point::{create_access_point_connection, AccessPointOpts},
    cli::StationArgs,
    connection::{get_active_connection, wait_for_connection_to_activate},
    util::deserialize_password,
};

#[derive(Default, Deserialize, PartialEq, Clone, Debug)]
pub struct StationOpts {
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

impl TryFrom<StationArgs> for StationOpts {
    type Error = anyhow::Error;

    fn try_from(args: StationArgs) -> Result<Self, Self::Error> {
        if let Some(cfg) = args.config {
            let mut buf = vec![];
            let mut cfg_file = File::open(cfg)?;
            cfg_file.read_to_end(&mut buf)?;

            let config = str::from_utf8(buf.as_slice())?;
            return parse_station_opts(config);
        }

        Ok(StationOpts {
            wireless_ifname: args.wireless_ifname,
            ssid: args.ssid,
            ip4_addr: args.ip4_addr,
            password: args.password,
        })
    }
}

impl From<AccessPointOpts> for StationOpts {
    fn from(opts: AccessPointOpts) -> StationOpts {
        StationOpts {
            wireless_ifname: opts.wireless_ifname,
            ssid: opts.ssid,
            password: opts.password,
            ip4_addr: opts.ip4_addr,
        }
    }
}

fn parse_station_opts(config: &str) -> Result<StationOpts> {
    let opts: StationOpts = serde_yaml::from_str(config)?;
    Ok(opts)
}

#[instrument(skip(client), err)]
pub async fn create_station(client: &Client, opts: StationOpts) -> Result<()> {
    let wireless_ifname = match &opts.wireless_ifname {
        Some(ifname) => ifname,
        None => return Err(anyhow!("Required wireless interface not specified")),
    };

    let ssid = match &opts.ssid {
        Some(ssid) => ssid,
        None => return Err(anyhow!("Required SSID not specified")),
    };

    // Create STA struct here so we can comprehensively search
    // for any matching existing connection, should it exist
    // Does not add connection to Network Manager, that happens later
    //
    // AP connection added for searching purposes. Does not add
    // connection to Network Manager, it is purely local
    let sta_conn = create_sta_connection(&opts)?;
    let ap_conn = create_access_point_connection(&opts.clone().into())?;

    // Check for and deactivate any existing active station connections
    // which share the same wireless interface.
    //
    // Station connection added for searching purposes. Does not add
    // connection to Network Manager, it is purely local
    match get_active_connection(client, DeviceType::Wifi, &sta_conn) {
        Some(c) => {
            debug!(
                "Found active station connection with ifname \"{}\", deactivating",
                wireless_ifname
            );
            client.deactivate_connection_future(&c).await?;
        }
        None => debug!(
            "No matching active wireless station connections for interface \"{}\"",
            wireless_ifname
        ),
    };

    // Check for and deactivate any matching AP conn
    match get_active_connection(client, DeviceType::Wifi, &ap_conn) {
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
    let sta_conn = client.add_connection_future(&sta_conn, true).await?;

    info!("Activating access point connection \"{}\"", ssid);
    let sta_conn = client
        .activate_connection_future(Some(&sta_conn), Some(&wireless_dev), None)
        .await?;

    // Waits until station is up and associated, not sure we want that
    let res = wait_for_connection_to_activate(&sta_conn).await;

    if res.is_ok() {
        info!("Activated access point connection \"{}\"", ssid);
    }
    res
}

pub fn create_sta_connection(opts: &StationOpts) -> Result<SimpleConnection> {
    let connection = SimpleConnection::new();

    let s_connection = SettingConnection::new();
    let s_wireless = SettingWireless::new();
    let s_ip4 = SettingIP4Config::new();

    // General connection settings
    s_connection.set_type(Some(SETTING_WIRELESS_SETTING_NAME));

    match &opts.ssid {
        Some(ssid) => {
            s_connection.set_id(Some(ssid));
        }
        None => return Err(anyhow!("Required SSID not specified")),
    };

    match &opts.wireless_ifname {
        Some(ifname) => s_connection.set_interface_name(Some(ifname)),
        None => return Err(anyhow!("Required wireless interface not specified")),
    };

    // Wifi-specific settings
    s_wireless.set_mode(Some(SETTING_WIRELESS_MODE_INFRA));

    match &opts.ssid {
        Some(ssid) => {
            s_wireless.set_ssid(Some(&(ssid.as_bytes().into())));
        }
        None => return Err(anyhow!("Required SSID not specified")),
    };

    // Wifi security settings
    if let Some(password) = &opts.password {
        let s_wireless_security = SettingWirelessSecurity::new();
        s_wireless_security.set_key_mgmt(Some("wpa-psk")); // TODO
        s_wireless_security.set_psk(Some(password));
        connection.add_setting(s_wireless_security);
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

        let opts = parse_station_opts(cfg).unwrap();
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

        let opts = parse_station_opts(cfg).unwrap();
        assert!(opts.wireless_ifname.is_none());
    }

    #[test]
    fn no_ssid() {
        let cfg = "
            wireless_interface: \"test_interface\"
            password: \"test_password\"
            ip4_addr: \"172.16.0.1/24\"
        ";

        let opts = parse_station_opts(cfg).unwrap();
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

        let opts = parse_station_opts(cfg).unwrap();
        assert!(opts.ssid.is_none());
    }

    #[test]
    fn no_password() {
        let cfg = "
            wireless_interface: \"test_interface\"
            ssid: \"test_ssid\"
            ip4_addr: \"172.16.0.1/24\"
        ";

        let opts = parse_station_opts(cfg).unwrap();
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

        let opts = parse_station_opts(cfg).unwrap();
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

        parse_station_opts(cfg).unwrap();
    }

    // Make sure use default address/subnet when no ip4_addr specified
    #[test]
    fn no_ip4_addr() {
        let cfg = "
            wireless_interface: \"test_interface\"
            ssid: \"test_ssid\"
            password: \"test_password\"
        ";

        let opts = parse_station_opts(cfg).unwrap();
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

        parse_station_opts(cfg).unwrap();
    }
}
