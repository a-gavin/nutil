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
    connection::get_active_connection,
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
    let _sta_conn = client
        .activate_connection_future(Some(&sta_conn), Some(&wireless_dev), None)
        .await?;

    // TODO: Impl wait for connection to be Activated state

    Ok(())
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

// TODO: Impl unit tests for create_sta_connection
