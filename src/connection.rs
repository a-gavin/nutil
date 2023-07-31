use std::cell::RefCell;
use std::rc::Rc;

use anyhow::{anyhow, Result};
use futures_channel::oneshot;
use glib::translate::FromGlib;
use nm::*;
use tracing::{debug, error, instrument, warn};

// Create a wired SimpleConnection for use in activating, deactivating, finding, etc
// If bond_ifname is Some, create the wired connection as a bond slave with bond_ifname as master.
// If bond_ifname is Some and "ANY", this connection will match to any other slave wired connection
// when searching for wired connections, assuming all other fields match.
//
// NOTE: SimpleConnection are owned by this program. ActiveConnection and RemoteConnection
//       are owned by the NetworkManager library
pub fn create_wired_connection(
    wired_ifname: &str,
    bond_ifname: Option<&str>,
) -> Result<SimpleConnection> {
    let connection = SimpleConnection::new();

    let s_connection = SettingConnection::new();

    // General settings
    s_connection.set_type(Some(SETTING_WIRED_SETTING_NAME));
    s_connection.set_id(Some(wired_ifname));
    s_connection.set_interface_name(Some(wired_ifname));

    // Master is bond interface name, slave type is type of master interface (i.e. bond)
    if let Some(bond_ifname) = bond_ifname {
        s_connection.set_master(Some(bond_ifname));
        s_connection.set_slave_type(Some(SETTING_BOND_SETTING_NAME));
    }

    connection.add_setting(s_connection);

    Ok(connection)
}

// Search for connection that matches the specified
// device type and properties in provided connection.
//
// Will continue to search for connections with matching ifnames after match found
// This done to enable verbose logging
#[instrument(skip(client, conn), parent=None)]
pub fn get_connection(
    client: &Client,
    device_type: DeviceType,
    conn: &SimpleConnection,
) -> Option<RemoteConnection> {
    let ifname = conn.interface_name()?;
    debug!("Searching for connection with ifname \"{}\"", ifname);

    // Only Bond and Ethernet DeviceType supported
    if device_type != DeviceType::Bond
        && device_type != DeviceType::Ethernet
        && device_type != DeviceType::Wifi
    {
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
            DeviceType::Bond => matching_bond_connection(conn, &cmp_conn),
            DeviceType::Ethernet => matching_wired_connection(conn, &cmp_conn),
            DeviceType::Wifi => matching_wifi_connection(conn, &cmp_conn),
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
#[instrument(skip(client, conn), parent=None)]
pub fn get_active_connection(
    client: &Client,
    device_type: DeviceType,
    conn: &SimpleConnection,
) -> Option<ActiveConnection> {
    let ifname = conn.interface_name()?;
    debug!("Searching for active connection with ifname \"{}\"", ifname);

    // Only Bond, Ethernet, and Wifi (STA and AP) DeviceType supported
    if device_type != DeviceType::Bond
        && device_type != DeviceType::Ethernet
        && device_type != DeviceType::Wifi
    {
        error!(
            "Unsupported device type \"{}\" for get_connection()",
            device_type
        );
        return None;
    }

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
            DeviceType::Bond => matching_bond_connection(conn, &cmp_conn),
            DeviceType::Ethernet => matching_wired_connection(conn, &cmp_conn),
            DeviceType::Wifi => matching_wifi_connection(conn, &cmp_conn),
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

#[instrument(skip(client), parent=None)]
pub fn get_slave_connections(
    client: &Client,
    master_ifname: &str,
    slave_device_type: DeviceType,
) -> Option<Vec<RemoteConnection>> {
    debug!(
        "Searching for slave connection with master ifname \"{}\"",
        master_ifname
    );

    // Only Ethernet DeviceType supported
    if slave_device_type != DeviceType::Ethernet {
        error!(
            "Unsupported device type \"{}\" for get_connection()",
            slave_device_type
        );
        return None;
    }

    let mut slave_conns: Vec<RemoteConnection> = vec![];

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

                    // Expect unwrap to succeed as we just upcasted from a RemoteConnection earlier
                    slave_conns.push(conn.downcast::<RemoteConnection>().unwrap());
                }
            }
            None => {
                debug!("Skipping connection without master \"{}\"", conn_id_str);
                continue;
            }
        }
    }

    Some(slave_conns)
}

// Spawn a new helper thread to poll until connection is fully activated
pub async fn wait_for_connection_to_activate(conn: &ActiveConnection) -> Result<()> {
    // No sense polling for activated if already up
    if conn.state() == ActiveConnectionState::Activated {
        return Ok(());
    }

    let (sender, receiver) = oneshot::channel::<Result<()>>();
    let sender = Rc::new(RefCell::new(Some(sender)));

    // TODO: Impl timeout
    conn.connect_state_changed(move |_, state, _| {
        let sender = sender.clone();

        glib::MainContext::ref_thread_default().spawn_local(async move {
            let state = unsafe { ActiveConnectionState::from_glib(state as _) };
            debug!("Connection state: {}", get_connection_state_str(state));

            let exit = match state {
                ActiveConnectionState::Activating => None,
                ActiveConnectionState::Activated => Some(Ok(())),
                _ => Some(Err(anyhow!(
                    "Unexpected connection state \"{}\"",
                    get_connection_state_str(state)
                ))),
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

// Determine if provided connection for comparison `cmp_conn` is a bond connection
// and matches desired connection `conn`
//
// Don't compare granular settings like bond mode, miimon, or backing network devices,
// just backing interface name
#[instrument(skip_all, parent=None)]
pub fn matching_bond_connection(conn: &SimpleConnection, cmp_conn: &Connection) -> bool {
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

    // Get connection id for each connection
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

    // Ensure both connections are bond (don't assume connection desired is a bond)
    let conn_type = match conn_settings.type_() {
        Some(c) => c,
        None => {
            error!("Unable to get connection id");
            return false;
        }
    };

    if conn_type.as_str() != SETTING_BOND_SETTING_NAME {
        debug!("Connection \"{}\" is not bond connection", conn_id_str);
        return false;
    }

    let cmp_conn_type = match cmp_conn_settings.type_() {
        Some(c) => c,
        None => {
            error!("Unable to get connection id");
            return false;
        }
    };

    if cmp_conn_type.as_str() != SETTING_BOND_SETTING_NAME {
        debug!("Connection \"{}\" is not bond connection", cmp_conn_id_str);
        return false;
    }

    // Compare backing bond interface names,
    // if exists in connection to compare against
    if let Some(conn_ifname) = conn.interface_name() {
        let cmp_conn_ifname = match cmp_conn.interface_name() {
            Some(ifname) => ifname,
            None => {
                error!("Unable to get interface name");
                return false;
            }
        };

        if conn_ifname != cmp_conn_ifname {
            debug!(
                "Connection \"{}\" ifname \"{}\" does not match desired ifname \"{}\"",
                cmp_conn_id_str, cmp_conn_ifname, conn_ifname
            );
            return false;
        }
    }

    true
}

// Determine if provided connection for comparison `cmp_conn` is a bond connection
// and matches desired connection `conn`
//
// In addition to comparing backing interface name, also compare slave settings
// (e.g. master name, slave type) if connection is determined to be a slave connection.
#[instrument(skip_all, parent=None)]
pub fn matching_wired_connection(conn: &SimpleConnection, cmp_conn: &Connection) -> bool {
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

    // Ensure both connections are wired
    let conn_type = match conn_settings.type_() {
        Some(c) => c,
        None => {
            error!("Unable to get connection id");
            return false;
        }
    };

    if conn_type != SETTING_WIRED_SETTING_NAME {
        debug!("Connection \"{}\" is not bond connection", conn_id_str);
        return false;
    }

    let cmp_conn_type = match cmp_conn_settings.type_() {
        Some(c) => c,
        None => {
            error!("Unable to get connection id");
            return false;
        }
    };

    if cmp_conn_type != SETTING_WIRED_SETTING_NAME {
        debug!("Connection \"{}\" is not bond connection", cmp_conn_id_str);
        return false;
    }

    // Get ifname for both wired connections
    if let Some(conn_ifname) = conn.interface_name() {
        let cmp_conn_ifname = match cmp_conn.interface_name() {
            Some(ifname) => ifname,
            None => {
                error!("Unable to get interface name");
                return false;
            }
        };

        if conn_ifname != cmp_conn_ifname {
            debug!(
                "Connection \"{}\" ifname \"{}\" does not match desired ifname \"{}\"",
                cmp_conn_id_str, cmp_conn_ifname, conn_ifname
            );
            return false;
        }
    }

    // TODO
    // Compare both's master connections, if either is a slave connection
    let conn_master = conn_settings.master();
    let cmp_conn_master = cmp_conn_settings.master();

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

        // Empty string master is reserved to indicate we're searching for
        // any wired connection with all matching properties save
        // the master device.
        //
        // In other words, we're looking for any wired connection we want to mess with
        // that's already being used for something else.
        if conn_master != cmp_conn_master && conn_master != "" {
            debug!(
                "Connection \"{}\" and compared connection \"{}\" have different master devices",
                conn_id_str, cmp_conn_id_str
            );
            return false;
        } else if conn_master == "" {
            debug!(
                "Connection \"{}\" and compared connection \"{}\" have different master devices, but match otherwise",
                conn_id_str, cmp_conn_id_str
            );
            return true;
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

    debug!(
        "Connection \"{}\" matches compared connection \"{}\"",
        conn_id_str, cmp_conn_id_str
    );

    true
}

#[instrument(skip_all, parent=None)]
pub fn matching_wifi_connection(conn: &SimpleConnection, cmp_conn: &Connection) -> bool {
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

    // Get connection id for each connection
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

    // Ensure both connections are wireless
    let conn_type = match conn_settings.type_() {
        Some(c) => c,
        None => {
            error!("Unable to get connection id");
            return false;
        }
    };

    if conn_type != SETTING_WIRELESS_SETTING_NAME {
        debug!("Connection \"{}\" is not bond connection", conn_id_str);
        return false;
    }

    let cmp_conn_type = match cmp_conn_settings.type_() {
        Some(c) => c,
        None => {
            error!("Unable to get connection id");
            return false;
        }
    };

    if cmp_conn_type != SETTING_WIRELESS_SETTING_NAME {
        debug!("Connection \"{}\" is not bond connection", cmp_conn_id_str);
        return false;
    }

    // Compare backing wireless interface names,
    // if exists in connection to compare against
    if let Some(conn_ifname) = conn.interface_name() {
        let cmp_conn_ifname = match cmp_conn.interface_name() {
            Some(ifname) => ifname,
            None => {
                error!("Unable to get interface name");
                return false;
            }
        };

        if conn_ifname != cmp_conn_ifname {
            debug!(
                "Connection \"{}\" ifname \"{}\" does not match desired ifname \"{}\"",
                cmp_conn_id_str, cmp_conn_ifname, conn_ifname
            );
            return false;
        }
    }

    // Get wireless settings for both connections (required for both)
    let conn_wireless_settings = match conn.setting_wireless() {
        Some(setting) => setting,
        None => {
            error!("Unable to get wireless settings");
            return false;
        }
    };

    let cmp_conn_wireless_settings = match cmp_conn.setting_wireless() {
        Some(setting) => setting,
        None => {
            error!("Unable to get wireless settings");
            return false;
        }
    };

    // Compare wireless mode if exists in connection to compare against
    if let Some(conn_mode) = conn_wireless_settings.mode() {
        let cmp_conn_mode = match cmp_conn_wireless_settings.mode() {
            Some(mode) => mode,
            None => {
                error!("Unable to get mode");
                return false;
            }
        };

        if conn_mode.as_str() != cmp_conn_mode.as_str() {
            debug!(
                "Connection \"{}\" wireless mode \"{}\" does not match desired wireless mode \"{}\"",
                cmp_conn_id_str, cmp_conn_mode, conn_mode
            );
            return false;
        }
    };

    // Compare SSID if exists in connection to compare against
    if let Some(conn_ssid) = conn_wireless_settings.ssid() {
        let cmp_conn_ssid = match cmp_conn_wireless_settings.ssid() {
            Some(ssid) => ssid,
            None => {
                error!("Unable to get ssid");
                return false;
            }
        };

        if conn_ssid != cmp_conn_ssid {
            debug!(
                "Connection \"{}\" SSID does not match desired SSID",
                cmp_conn_id_str,
            );
            return false;
        }

        // TODO: Get SSID strs for logging purposes w/o resorting to nightly
        //let cmp_conn_ssid = match cmp_conn_ssid.as_ref().as_ascii() {
        //    Some(ssid) => ssid,
        //    None => {
        //        error!("Connection \"{}\" SSID is not ASCII", cmp_conn_id_str);
        //        return false;
        //    }
        //};
    }

    true
}

pub fn get_connection_state_str(state: ActiveConnectionState) -> &'static str {
    match state {
        ActiveConnectionState::Activated => "activated",
        ActiveConnectionState::Activating => "activating",
        ActiveConnectionState::Deactivated => "deactivated",
        ActiveConnectionState::Deactivating => "deactivating",
        ActiveConnectionState::Unknown => "unknown",
        _ => panic!("Unexpected connection state \"{}\"", state),
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use ipnet::Ipv4Net;

    use super::*;
    use crate::util::DEFAULT_IP4_ADDR;

    const TEST_ID: &str = "test_id";
    const TEST_IFNAME: &str = "test_ifname";
    const TEST_MASTER_IFNAME: &str = "test_master_ifname";
    const TEST_SSID: &str = "test_ssid";
    const TEST_PASSWORD: &str = "test_password";

    /// Creates connection with base connection fields initialized to test values.
    fn create_base_connection() -> SimpleConnection {
        let connection = SimpleConnection::new();
        let s_ip4 = SettingIP4Config::new();

        // General connection settings
        let s_connection = SettingConnection::new();
        s_connection.set_id(Some(TEST_ID));
        s_connection.set_autoconnect(false);
        s_connection.set_interface_name(Some(TEST_IFNAME));

        // IPv4 settings
        let ip4_net = Ipv4Net::from_str(DEFAULT_IP4_ADDR).unwrap();
        let ip4_addr = IPAddress::new(
            libc::AF_INET,
            ip4_net.addr().to_string().as_str(),
            ip4_net.prefix_len() as u32,
        )
        .unwrap();

        s_ip4.add_address(&ip4_addr);
        s_ip4.set_method(Some(SETTING_IP4_CONFIG_METHOD_MANUAL));

        connection.add_setting(s_connection);
        connection.add_setting(s_ip4);

        connection
    }

    fn create_bond_connection() -> SimpleConnection {
        let connection = create_base_connection();
        let s_bond = SettingBond::new();

        // General connection settings
        let s_connection = connection.setting_connection().unwrap();
        s_connection.set_type(Some(SETTING_BOND_SETTING_NAME));

        // Bond-specific settings
        s_bond.add_option(SETTING_BOND_OPTION_MODE, "active-backup");
        s_bond.add_option(SETTING_BOND_OPTION_MIIMON, "100");

        connection.add_setting(s_bond);

        connection
    }

    fn create_wired_connection() -> SimpleConnection {
        let connection = create_base_connection();

        // General connection settings
        let s_connection = connection.setting_connection().unwrap();
        s_connection.set_type(Some(SETTING_WIRED_SETTING_NAME));
        s_connection.set_master(Some(TEST_MASTER_IFNAME));
        s_connection.set_slave_type(Some(SETTING_BOND_SETTING_NAME));

        connection
    }

    /// Creates WiFi connection with fields initialized to test values
    /// except the wireless setting mode. Mode defaults to STA, but seems to be
    /// only when added to NetworkManager (i.e. is indeterminate/unset before, haven't bothered to check).
    ///
    /// IPv4 settings are set to static with the default IPv4 address and subnet
    fn create_wifi_connection() -> SimpleConnection {
        let connection = create_base_connection();

        let s_wireless = SettingWireless::new();
        let s_wireless_security = SettingWirelessSecurity::new();

        // General connection settings
        let s_connection = connection.setting_connection().unwrap();
        s_connection.set_type(Some(SETTING_WIRELESS_SETTING_NAME));

        // Wifi settings
        s_wireless.set_ssid(Some(&(TEST_SSID.as_bytes().into())));

        // Wifi security settings
        s_wireless_security.set_key_mgmt(Some("wpa-psk"));
        s_wireless_security.set_psk(Some(TEST_PASSWORD));

        connection.add_setting(s_wireless);
        connection.add_setting(s_wireless_security);

        connection
    }

    fn create_ap_connection() -> SimpleConnection {
        let conn = create_wifi_connection();

        let s_wireless = conn.setting_wireless().unwrap();
        s_wireless.set_mode(Some(SETTING_WIRELESS_MODE_AP));

        conn
    }

    fn create_sta_connection() -> SimpleConnection {
        let conn = create_wifi_connection();

        let s_wireless = conn.setting_wireless().unwrap();
        s_wireless.set_mode(Some(SETTING_WIRELESS_MODE_INFRA));

        conn
    }

    #[test]
    fn compare_bond_conns_conn_type() {
        // 1. All bond connection fields same, expect pass
        //    (covers all equal field test cases as nothing is changed)
        let base_conn = create_bond_connection();
        let cmp_conn = create_bond_connection().upcast::<Connection>();
        assert!(matching_bond_connection(&base_conn, &cmp_conn));

        // 2. Base has different type, expect fail
        let base_conn = create_sta_connection();
        let cmp_conn = create_bond_connection().upcast::<Connection>();
        assert!(!matching_bond_connection(&base_conn, &cmp_conn));

        // 3. Compare has different type, expect fail
        let base_conn = create_bond_connection();
        let cmp_conn = create_sta_connection().upcast::<Connection>();
        assert!(!matching_bond_connection(&base_conn, &cmp_conn));
    }

    #[test]
    fn compare_bond_conns_ifnames() {
        // 1. No base interface name, should pass as matching
        //    function should ignore this field when None
        let base_conn = create_bond_connection();
        let cmp_conn = create_bond_connection().upcast::<Connection>();

        let s_conn = base_conn.setting_connection().unwrap();
        s_conn.set_interface_name(None);
        base_conn.add_setting(s_conn);

        assert!(matching_bond_connection(&base_conn, &cmp_conn));

        // 2. Different base interface name, should fail
        let base_conn = create_bond_connection();
        let cmp_conn = create_bond_connection().upcast::<Connection>();

        let s_conn = base_conn.setting_connection().unwrap();
        s_conn.set_interface_name(Some("wrong_ifname"));
        base_conn.add_setting(s_conn);

        assert!(!matching_bond_connection(&base_conn, &cmp_conn));

        // 3. Different compare interface name, should fail
        let base_conn = create_bond_connection();
        let cmp_conn = create_bond_connection().upcast::<Connection>();

        let s_conn = cmp_conn.setting_connection().unwrap();
        s_conn.set_interface_name(Some("wrong_ifname"));
        cmp_conn.add_setting(s_conn);

        assert!(!matching_bond_connection(&base_conn, &cmp_conn));
    }

    #[test]
    fn compare_wired_conns_conn_type() {
        // 1. All wired connection fields same, expect pass
        //    (covers all equal field test cases as nothing is changed)
        let base_conn = create_wired_connection();
        let cmp_conn = create_wired_connection().upcast::<Connection>();
        assert!(matching_wired_connection(&base_conn, &cmp_conn));

        // 2. Base has different type, expect fail
        let base_conn = create_sta_connection();
        let cmp_conn = create_wired_connection().upcast::<Connection>();
        assert!(!matching_wired_connection(&base_conn, &cmp_conn));

        // 3. Compare has different type, expect fail
        let base_conn = create_wired_connection();
        let cmp_conn = create_sta_connection().upcast::<Connection>();
        assert!(!matching_wired_connection(&base_conn, &cmp_conn));
    }

    #[test]
    fn compare_wired_conns_ifnames() {
        // 1. No base interface name, should pass as matching
        //    function should ignore this field when None
        let base_conn = create_wired_connection();
        let cmp_conn = create_wired_connection().upcast::<Connection>();

        let s_conn = base_conn.setting_connection().unwrap();
        s_conn.set_interface_name(None);
        base_conn.add_setting(s_conn);

        assert!(matching_wired_connection(&base_conn, &cmp_conn));

        // 2. Different base interface name, should fail
        let base_conn = create_wired_connection();
        let cmp_conn = create_wired_connection().upcast::<Connection>();

        let s_conn = base_conn.setting_connection().unwrap();
        s_conn.set_interface_name(Some("wrong_ifname"));
        base_conn.add_setting(s_conn);

        assert!(!matching_wired_connection(&base_conn, &cmp_conn));

        // 3. Different compare interface name, should fail
        let base_conn = create_wired_connection();
        let cmp_conn = create_wired_connection().upcast::<Connection>();

        let s_conn = cmp_conn.setting_connection().unwrap();
        s_conn.set_interface_name(Some("wrong_ifname"));
        cmp_conn.add_setting(s_conn);

        assert!(!matching_wired_connection(&base_conn, &cmp_conn));
    }

    // TODO: Other wired conn tests

    #[test]
    fn compare_wifi_conns_wireless_settings() {
        // 1. All wifi connection fields same, expect pass
        //    (covers all equal field test cases as nothing is changed)
        let base_conn = create_ap_connection();
        let cmp_conn = create_ap_connection().upcast::<Connection>();
        assert!(matching_wifi_connection(&base_conn, &cmp_conn));

        // 2. No base conn wireless settings, expect fail
        let base_conn = create_base_connection();
        let cmp_conn = create_ap_connection().upcast::<Connection>();
        assert!(!matching_wifi_connection(&base_conn, &cmp_conn));

        // 3. No cmp conn wireless settings, expect fail
        let base_conn = create_ap_connection();
        let cmp_conn = create_base_connection().upcast::<Connection>();
        assert!(!matching_wifi_connection(&base_conn, &cmp_conn));
    }

    #[test]
    fn compare_wifi_conns_ifnames() {
        // 1. No base interface name, should pass as matching
        //    function should ignore this field when None
        let base_conn = create_ap_connection();
        let cmp_conn = create_ap_connection().upcast::<Connection>();

        let s_conn = base_conn.setting_connection().unwrap();
        s_conn.set_interface_name(None);
        base_conn.add_setting(s_conn);

        assert!(matching_wifi_connection(&base_conn, &cmp_conn));

        // 2. Different base interface name, should fail
        let base_conn = create_ap_connection();
        let cmp_conn = create_ap_connection().upcast::<Connection>();

        let s_conn = base_conn.setting_connection().unwrap();
        s_conn.set_interface_name(Some("wrong_ifname"));
        base_conn.add_setting(s_conn);

        assert!(!matching_wifi_connection(&base_conn, &cmp_conn));

        // 3. Different compare interface name, should fail
        let base_conn = create_ap_connection();
        let cmp_conn = create_ap_connection().upcast::<Connection>();

        let s_conn = cmp_conn.setting_connection().unwrap();
        s_conn.set_interface_name(Some("wrong_ifname"));
        cmp_conn.add_setting(s_conn);

        assert!(!matching_wifi_connection(&base_conn, &cmp_conn));
    }

    #[test]
    fn compare_wifi_mode() {
        // 1. Different base mode, should fail as connection created as an AP but changed to STA
        let base_conn = create_ap_connection();
        let cmp_conn = create_sta_connection().upcast::<Connection>();
        assert!(!matching_wifi_connection(&base_conn, &cmp_conn));

        // 2. Different cmp mode, should fail as connection created as an AP but changed to STA
        let base_conn = create_sta_connection();
        let cmp_conn = create_ap_connection().upcast::<Connection>();
        assert!(!matching_wifi_connection(&base_conn, &cmp_conn));
    }

    #[test]
    fn compare_wifi_ssid() {
        // 1. No SSID, should pass as matching function
        //    should ignore this field when None
        let base_conn = create_ap_connection();
        let cmp_conn = create_ap_connection().upcast::<Connection>();

        let s_wireless = base_conn.setting_wireless().unwrap();
        s_wireless.set_ssid(None);
        base_conn.add_setting(s_wireless);

        assert!(matching_wifi_connection(&base_conn, &cmp_conn));

        // 2. Different base SSID, should fail
        let base_conn = create_ap_connection();
        let cmp_conn = create_ap_connection().upcast::<Connection>();

        let s_wireless = base_conn.setting_wireless().unwrap();
        s_wireless.set_ssid(Some(&("wrong_ssid".as_bytes().into())));
        base_conn.add_setting(s_wireless);

        assert!(!matching_wifi_connection(&base_conn, &cmp_conn));

        // 3. Different cmp SSID, should fail
        let base_conn = create_ap_connection();
        let cmp_conn = create_ap_connection().upcast::<Connection>();

        let s_wireless = cmp_conn.setting_wireless().unwrap();
        s_wireless.set_ssid(Some(&("wrong_ssid".as_bytes().into())));
        cmp_conn.add_setting(s_wireless);

        assert!(!matching_wifi_connection(&base_conn, &cmp_conn));
    }
}
