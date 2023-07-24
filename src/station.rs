use std::fs::File;

use anyhow::{anyhow, Result};
use ipnet::Ipv4Net;
use nm::*;
use serde::Deserialize;
use tracing::instrument;

use crate::{
    access_point::AccessPointOpts,
    cli::StationArgs,
    util::{default_ip4_addr, deserialize_ip4_addr, deserialize_password},
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

    #[serde(deserialize_with = "deserialize_ip4_addr")]
    #[serde(default = "default_ip4_addr")]
    pub ip4_addr: Ipv4Net,
}

#[instrument(err)]
pub fn parse_access_point_opts(config: Option<String>, args: StationArgs) -> Result<StationOpts> {
    match config {
        Some(cfg) => {
            let cfg_file = File::open(cfg)?;
            let opts: StationOpts = serde_yaml::from_reader(cfg_file)?;
            Ok(opts)
        }
        None => StationOpts::try_from(args),
    }
}

impl TryFrom<StationArgs> for StationOpts {
    type Error = anyhow::Error;

    fn try_from(args: StationArgs) -> Result<Self, Self::Error> {
        Ok(StationOpts {
            wireless_ifname: args.wireless_ifname,
            ..Default::default()
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

pub fn create_sta_connection(opts: &StationOpts) -> Result<SimpleConnection> {
    let connection = SimpleConnection::new();

    let s_connection = SettingConnection::new();
    let s_wireless = SettingWireless::new();

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

    connection.add_setting(s_connection);
    connection.add_setting(s_wireless);

    Ok(connection)
}
