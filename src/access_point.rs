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
use tracing::{debug, info, instrument};

use crate::{
    cli::AccessPointArgs,
    connection::{get_active_connection, get_connection, get_connection_state_str},
    station::create_sta_connection,
    util::{default_ip4_addr, deserialize_ip4_addr, deserialize_password},
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

    #[serde(deserialize_with = "deserialize_ip4_addr")]
    #[serde(default = "default_ip4_addr")]
    pub ip4_addr: Ipv4Net,
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

        let ip4_addr = match &args.ip4_addr {
            Some(addr) => Ipv4Net::from_str(addr.as_str())?,
            None => default_ip4_addr(),
        };

        Ok(AccessPointOpts {
            wireless_ifname: args.wireless_ifname,
            ssid: args.ssid,
            ip4_addr,
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
        None => return Err(anyhow!("Required wireless interface not specified")),
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
                "Found active standalone wired connection with slave ifname \"{}\", deactivating",
                wireless_ifname
            );
            client.deactivate_connection_future(&c).await?;
        }
        None => debug!(
            "No matching active standalone wired connection for interface \"{}\"",
            wireless_ifname
        ),
    };

    let wireless_dev = match client.device_by_iface(wireless_ifname.as_str()) {
        Some(device) => device,
        None => {
            return Err(anyhow!(
                "Wired device \"{}\" does not exist, quitting...",
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

    let ip4_addr = IPAddress::new(
        libc::AF_INET,
        opts.ip4_addr.addr().to_string().as_str(),
        opts.ip4_addr.prefix_len() as u32,
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
    use ipnet::Ipv4Net;

    use super::*;
    use crate::util::DEFAULT_IP4_ADDR;

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

        let ipv_net = Ipv4Net::from_str(DEFAULT_IP4_ADDR).unwrap();
        assert_eq!(ipv_net.addr(), opts.ip4_addr.addr());
        assert_eq!(ipv_net.prefix_len(), opts.ip4_addr.prefix_len());
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

    #[test]
    #[should_panic]
    fn no_ip4_addr_subnet() {
        let cfg = "
            wireless_interface: \"test_interface\"
            ssid: \"test_ssid\"
            password: \"test_password\"
            ip4_addr: \"172.16.0.1\"
        ";

        parse_access_point_opts(cfg).unwrap();
    }
}
