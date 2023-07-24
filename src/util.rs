use std::str::FromStr;

use anyhow::{anyhow, Result};
use ipnet::Ipv4Net;
use nm::utils_wpa_psk_valid;
use serde::{de::Error, Deserialize, Deserializer};

pub const DEFAULT_IP4_ADDR: &str = "192.0.2.1/24";

pub fn default_ip4_addr() -> Ipv4Net {
    Ipv4Net::from_str(DEFAULT_IP4_ADDR).unwrap()
}

pub fn deserialize_password<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;

    if s.is_empty() {
        Ok(None)
    } else if s.len() < 8 {
        Err(anyhow!("Password must be 8 chars or longer")).map_err(D::Error::custom)
    } else if !utils_wpa_psk_valid(s.as_str()) {
        Err(anyhow!("libnm says your PSK is invalid ¯\\_(ツ)_/¯")).map_err(D::Error::custom)
    } else {
        Ok(Some(s))
    }
}

pub fn deserialize_ip4_addr<'de, D>(deserializer: D) -> Result<Ipv4Net, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;

    Ipv4Net::from_str(&s).map_err(D::Error::custom)
}
