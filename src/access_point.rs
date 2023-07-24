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
    util::{default_ip4_addr, default_wireless_ifname, deserialize_ip4_addr, deserialize_password},
};

#[derive(Default, Deserialize, PartialEq, Clone, Debug)]
pub struct AccessPointOpts {
    #[serde(rename = "wireless_interface")]
    #[serde(default = "default_wireless_ifname")]
    pub wireless_ifname: String,

    pub ssid: String,

    /// Must be 8 characters or longer
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

        let wireless_ifname = match args.wireless_ifname {
            Some(ifname) => ifname,
            None => "".to_string(),
        };

        let ssid = match args.ssid {
            Some(ssid) => ssid,
            None => return Err(anyhow!("Required SSID not specified")),
        };

        let ip4_addr = match &args.ip4_addr {
            Some(addr) => Ipv4Net::from_str(addr.as_str())?,
            None => default_ip4_addr(),
        };

        Ok(AccessPointOpts {
            wireless_ifname,
            ssid,
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
    if opts.wireless_ifname.is_empty() {
        return Err(anyhow!("Required wireless interface not specified"));
    }

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
                &opts.wireless_ifname
            );
            client.deactivate_connection_future(&c).await?;
        }
        None => debug!(
            "No matching active standalone wired connection for interface \"{}\"",
            &opts.wireless_ifname
        ),
    };

    let wireless_dev = match client.device_by_iface(&opts.wireless_ifname) {
        Some(device) => device,
        None => {
            return Err(anyhow!(
                "Wired device \"{}\" does not exist, quitting...",
                &opts.wireless_ifname
            ));
        }
    };

    info!("Creating access point connection");
    let ap_conn = client.add_connection_future(&ap_conn, true).await?;

    info!("Activating access point connection");
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

    receiver.await?
}

pub fn create_access_point_connection(opts: &AccessPointOpts) -> Result<SimpleConnection> {
    let connection = SimpleConnection::new();

    let s_connection = SettingConnection::new();
    let s_wireless = SettingWireless::new();
    let s_ip4 = SettingIP4Config::new();

    // General connection settings
    s_connection.set_type(Some(SETTING_WIRELESS_SETTING_NAME));
    s_connection.set_id(Some(&opts.ssid));
    s_connection.set_interface_name(Some(&opts.wireless_ifname));
    s_connection.set_autoconnect(false);

    // Wifi-specific settings
    s_wireless.set_ssid(Some(&(opts.ssid.as_bytes().into())));
    //s_wireless.set_band(Some("bg"));
    s_wireless.set_hidden(false);
    s_wireless.set_mode(Some(SETTING_WIRELESS_MODE_AP));

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
