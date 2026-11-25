//! A pretend network, for developing on machines without NetworkManager and for
//! tests: a wired connection that's always up, and three Wi-Fi networks. "Home"
//! and "Neighbor" are secure, and their password is `password`.

use std::collections::HashSet;
use std::sync::Mutex;

use protocol::net::{Connection, NetState, NetStatus, WifiNetwork};

use crate::Error;

pub struct Fake {
    state: Mutex<State>,
}

struct State {
    wifi: bool,
    joined: Option<String>,
    known: HashSet<String>,
}

const NETWORKS: &[(&str, u8, bool)] = &[("Home", 82, true), ("Cafe", 55, false), ("Neighbor", 31, true)];

impl Default for Fake {
    fn default() -> Fake {
        Fake { state: Mutex::new(State { wifi: true, joined: None, known: HashSet::new() }) }
    }
}

impl Fake {
    pub fn status(&self) -> NetStatus {
        let s = self.state.lock().unwrap();
        let mut connections = vec![Connection {
            name: "Wired connection 1".into(),
            kind: "ethernet".into(),
            device: "eth0".into(),
            ip4: Some("10.0.2.15/24".into()),
        }];
        if let Some(ssid) = &s.joined {
            connections.push(Connection {
                name: ssid.clone(),
                kind: "wifi".into(),
                device: "wlan0".into(),
                ip4: Some("192.168.1.42/24".into()),
            });
        }
        NetStatus { state: NetState::Connected, connections, wifi_enabled: s.wifi, wifi_present: true }
    }

    pub fn networks(&self) -> Result<Vec<WifiNetwork>, Error> {
        let s = self.state.lock().unwrap();
        if !s.wifi {
            return Ok(Vec::new());
        }
        Ok(NETWORKS
            .iter()
            .map(|&(ssid, strength, secure)| WifiNetwork {
                ssid: ssid.into(),
                strength,
                secure,
                active: s.joined.as_deref() == Some(ssid),
                known: s.known.contains(ssid),
            })
            .collect())
    }

    pub fn connect(&self, ssid: &str, password: Option<&str>) -> Result<(), Error> {
        let mut s = self.state.lock().unwrap();
        if !s.wifi {
            return Err(Error::unavailable("Wi-Fi is off"));
        }
        let Some(&(_, _, secure)) = NETWORKS.iter().find(|n| n.0 == ssid) else {
            return Err(Error::not_found(format!("no network called {ssid:?} in range")));
        };
        if secure && !s.known.contains(ssid) && password != Some("password") {
            return Err(Error::failed("couldn't join: wrong password?"));
        }
        s.known.insert(ssid.into());
        s.joined = Some(ssid.into());
        Ok(())
    }

    pub fn disconnect(&self) {
        self.state.lock().unwrap().joined = None;
    }

    pub fn forget(&self, ssid: &str) -> Result<(), Error> {
        let mut s = self.state.lock().unwrap();
        if !s.known.remove(ssid) {
            return Err(Error::not_found(format!("{ssid:?} isn't a saved network")));
        }
        if s.joined.as_deref() == Some(ssid) {
            s.joined = None;
        }
        Ok(())
    }

    pub fn set_wifi(&self, on: bool) {
        let mut s = self.state.lock().unwrap();
        s.wifi = on;
        if !on {
            s.joined = None;
        }
    }
}
