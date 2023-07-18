use anyhow::Result;
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
#[instrument(skip(client, conn), parent=None)]
pub fn get_connection(
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

// Determine if provided connection for comparison `cmp_conn` is a bond connection
// and matches desired connection `conn`
//
// Don't compare granular settings like bond mode, miimon, or backing network devices,
// just backing interface name
#[instrument(skip_all, parent=None)]
pub fn matching_bond_connection(conn: &SimpleConnection, cmp_conn: &Connection) -> bool {
    // Get SettingConnection obj for both connection and compared connection
    let cmp_conn_settings = match cmp_conn.setting_connection() {
        Some(c) => c,
        None => {
            error!("Unable to get connection settings");
            return false;
        }
    };

    // Get connection id for compared connection
    let cmp_conn_id = match cmp_conn_settings.id() {
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

        // ANY master is reserved to indicate we're searching for
        // any wired connection with all matching properties save
        // the master device.
        //
        // In other words, we're looking for any wired connection we want to mess with
        // that's already being used for something else.
        if conn_master != cmp_conn_master && conn_master != "ANY" {
            debug!(
                "Connection \"{}\" and compared connection \"{}\" have different master devices",
                conn_id_str, cmp_conn_id_str
            );
            return false;
        } else if conn_master == "ANY" {
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