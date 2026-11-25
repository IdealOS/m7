//! NetworkManager, over its D-Bus API on the system bus.
//!
//! Only the parts netd needs: the overall state and Wi-Fi switch, active
//! connections and their addresses, a Wi-Fi device's access points (and asking
//! it to scan), saved connection profiles, and activating, adding, deactivating
//! and deleting connections. Properties are read fresh each time (no caching,
//! no signal subscriptions), which is plenty for a settings screen.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use protocol::net::{Connection, NetState, NetStatus, WifiNetwork};
use zbus::proxy::CacheProperties;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::Error;

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const ACCESS_POINT: &str = "org.freedesktop.NetworkManager.AccessPoint";
const ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const IP4: &str = "org.freedesktop.NetworkManager.IP4Config";
const SETTINGS: &str = "org.freedesktop.NetworkManager.Settings";
const PROFILE: &str = "org.freedesktop.NetworkManager.Settings.Connection";
/// `NM_DEVICE_TYPE_WIFI`.
const TYPE_WIFI: u32 = 2;
/// `NM_ACTIVE_CONNECTION_STATE_*`.
const ACTIVATED: u32 = 2;
const DEACTIVATED: u32 = 4;
/// How long joining a network may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Nm {
    conn: zbus::Connection,
}

impl From<zbus::Error> for Error {
    fn from(e: zbus::Error) -> Error {
        Error::unavailable(format!("NetworkManager: {e}"))
    }
}

type Settings = HashMap<String, HashMap<String, OwnedValue>>;

impl Nm {
    pub async fn connect() -> Result<Nm, Error> {
        let conn = zbus::Connection::system().await?;
        let nm = Nm { conn };
        // Fail now, not on the first request, if NetworkManager isn't there.
        nm.prop::<u32>(NM_PATH, NM, "State").await?;
        Ok(nm)
    }

    async fn proxy<'a>(&'a self, path: &'a str, iface: &'a str) -> Result<zbus::Proxy<'a>, Error> {
        Ok(zbus::proxy::Builder::new(&self.conn)
            .destination(NM)?
            .path(path)?
            .interface(iface)?
            .cache_properties(CacheProperties::No)
            .build()
            .await?)
    }

    async fn prop<T>(&self, path: &str, iface: &str, name: &str) -> Result<T, Error>
    where
        T: TryFrom<OwnedValue>,
        T::Error: Into<zbus::Error>,
    {
        Ok(self.proxy(path, iface).await?.get_property::<T>(name).await?)
    }

    async fn paths(&self, path: &str, iface: &str, name: &str) -> Result<Vec<String>, Error> {
        let v: Vec<OwnedObjectPath> = self.prop(path, iface, name).await?;
        Ok(v.into_iter().map(|p| p.to_string()).collect())
    }

    /// The first Wi-Fi device's path.
    async fn wifi_device(&self) -> Result<Option<String>, Error> {
        for dev in self.paths(NM_PATH, NM, "Devices").await? {
            if self.prop::<u32>(&dev, DEVICE, "DeviceType").await? == TYPE_WIFI {
                return Ok(Some(dev));
            }
        }
        Ok(None)
    }

    pub async fn status(&self) -> Result<NetStatus, Error> {
        let state = match self.prop::<u32>(NM_PATH, NM, "State").await? {
            70 => NetState::Connected,
            50 | 60 => NetState::Limited,
            40 => NetState::Connecting,
            0 => NetState::Unavailable,
            _ => NetState::Disconnected,
        };
        let mut connections = Vec::new();
        for active in self.paths(NM_PATH, NM, "ActiveConnections").await? {
            let name: String = self.prop(&active, ACTIVE, "Id").await?;
            let kind: String = self.prop(&active, ACTIVE, "Type").await?;
            let device = match self.paths(&active, ACTIVE, "Devices").await?.first() {
                Some(d) => self.prop::<String>(d, DEVICE, "Interface").await?,
                None => String::new(),
            };
            let ip4 = match self.prop::<OwnedObjectPath>(&active, ACTIVE, "Ip4Config").await {
                Ok(p) if p.as_str() != "/" => self.address(p.as_str()).await,
                _ => None,
            };
            let kind = match kind.as_str() {
                "802-11-wireless" => "wifi".to_owned(),
                "802-3-ethernet" => "ethernet".to_owned(),
                other => other.to_owned(),
            };
            if kind == "loopback" {
                continue;
            }
            connections.push(Connection { name, kind, device, ip4 });
        }
        Ok(NetStatus {
            state,
            connections,
            wifi_enabled: self.prop(NM_PATH, NM, "WirelessEnabled").await?,
            wifi_present: self.wifi_device().await?.is_some(),
        })
    }

    /// An IP4Config's first address, as `address/prefix`.
    async fn address(&self, config: &str) -> Option<String> {
        let data: Vec<HashMap<String, OwnedValue>> = self.prop(config, IP4, "AddressData").await.ok()?;
        let first = data.first()?;
        let address: String = first.get("address")?.try_clone().ok()?.try_into().ok()?;
        let prefix: u32 =
            first.get("prefix").and_then(|v| v.try_clone().ok()).and_then(|v| v.try_into().ok()).unwrap_or(0);
        Some(format!("{address}/{prefix}"))
    }

    /// Saved Wi-Fi profiles, by SSID.
    async fn profiles(&self) -> Result<HashMap<String, String>, Error> {
        let settings = self.proxy(SETTINGS_PATH, SETTINGS).await?;
        let list: Vec<OwnedObjectPath> = settings.call("ListConnections", &()).await?;
        let mut out = HashMap::new();
        for path in list {
            let s: Settings = self.proxy(path.as_str(), PROFILE).await?.call("GetSettings", &()).await?;
            if let Some(ssid) =
                s.get("802-11-wireless").and_then(|w| w.get("ssid")).and_then(|v| v.try_clone().ok())
                && let Ok(bytes) = Vec::<u8>::try_from(ssid)
            {
                out.insert(String::from_utf8_lossy(&bytes).into_owned(), path.to_string());
            }
        }
        Ok(out)
    }

    /// Access points in range: (path, SSID, strength, secure), strongest first,
    /// one per SSID.
    async fn access_points(&self, device: &str) -> Result<Vec<(String, String, u8, bool)>, Error> {
        let wireless = self.proxy(device, WIRELESS).await?;
        let list: Vec<OwnedObjectPath> = wireless.call("GetAllAccessPoints", &()).await?;
        let mut aps = Vec::new();
        for path in list {
            let p = path.as_str();
            let ssid: Vec<u8> = self.prop(p, ACCESS_POINT, "Ssid").await?;
            if ssid.is_empty() {
                continue; // hidden
            }
            let strength: u8 = self.prop(p, ACCESS_POINT, "Strength").await?;
            let flags: u32 = self.prop(p, ACCESS_POINT, "Flags").await?;
            let wpa: u32 = self.prop(p, ACCESS_POINT, "WpaFlags").await?;
            let rsn: u32 = self.prop(p, ACCESS_POINT, "RsnFlags").await?;
            aps.push((
                p.to_owned(),
                String::from_utf8_lossy(&ssid).into_owned(),
                strength,
                flags & 1 != 0 || wpa != 0 || rsn != 0,
            ));
        }
        aps.sort_by_key(|a| std::cmp::Reverse(a.2));
        let mut seen = HashSet::new();
        aps.retain(|a| seen.insert(a.1.clone()));
        Ok(aps)
    }

    pub async fn networks(&self, rescan: bool) -> Result<Vec<WifiNetwork>, Error> {
        let Some(device) = self.wifi_device().await? else { return Ok(Vec::new()) };
        if rescan {
            let wireless = self.proxy(&device, WIRELESS).await?;
            let opts: HashMap<&str, Value> = HashMap::new();
            // A scan already under way is fine.
            let _: Result<(), _> = wireless.call("RequestScan", &(opts,)).await;
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        let active: OwnedObjectPath = self.prop(&device, WIRELESS, "ActiveAccessPoint").await?;
        let known = self.profiles().await?;
        Ok(self
            .access_points(&device)
            .await?
            .into_iter()
            .map(|(path, ssid, strength, secure)| WifiNetwork {
                active: path == active.as_str(),
                known: known.contains_key(&ssid),
                ssid,
                strength,
                secure,
            })
            .collect())
    }

    pub async fn connect_wifi(&self, ssid: &str, password: Option<&str>) -> Result<(), Error> {
        let device = self.wifi_device().await?.ok_or_else(|| Error::unavailable("no Wi-Fi device"))?;
        let nm = self.proxy(NM_PATH, NM).await?;
        let dev = ObjectPath::try_from(device.as_str()).map_err(zbus::Error::from)?;
        let none = ObjectPath::try_from("/").map_err(zbus::Error::from)?;
        let (active, made): (OwnedObjectPath, Option<OwnedObjectPath>) =
            match self.profiles().await?.get(ssid) {
                Some(profile) => {
                    let profile = ObjectPath::try_from(profile.as_str()).map_err(zbus::Error::from)?;
                    (nm.call("ActivateConnection", &(profile, &dev, &none)).await?, None)
                }
                None => {
                    let (ap, _, _, secure) =
                        self.access_points(&device).await?.into_iter().find(|a| a.1 == ssid).ok_or_else(
                            || Error::not_found(format!("no network called {ssid:?} in range")),
                        )?;
                    let mut settings: HashMap<&str, HashMap<&str, Value>> = HashMap::new();
                    settings.insert(
                        "connection",
                        HashMap::from([("id", Value::from(ssid)), ("type", Value::from("802-11-wireless"))]),
                    );
                    settings.insert(
                        "802-11-wireless",
                        HashMap::from([
                            ("ssid", Value::from(ssid.as_bytes().to_vec())),
                            ("mode", Value::from("infrastructure")),
                        ]),
                    );
                    if secure {
                        let psk =
                            password.ok_or_else(|| Error::failed(format!("{ssid:?} needs a password")))?;
                        settings.insert(
                            "802-11-wireless-security",
                            HashMap::from([("key-mgmt", Value::from("wpa-psk")), ("psk", Value::from(psk))]),
                        );
                    }
                    let ap = ObjectPath::try_from(ap.as_str()).map_err(zbus::Error::from)?;
                    let (profile, active): (OwnedObjectPath, OwnedObjectPath) =
                        nm.call("AddAndActivateConnection", &(settings, &dev, ap)).await?;
                    (active, Some(profile))
                }
            };
        let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
        loop {
            // The active connection object goes away when activation fails.
            let state = self.prop::<u32>(active.as_str(), ACTIVE, "State").await.unwrap_or(DEACTIVATED);
            if state == ACTIVATED {
                return Ok(());
            }
            if state == DEACTIVATED || tokio::time::Instant::now() > deadline {
                // Don't keep a profile with a password that didn't work.
                if let Some(profile) = made {
                    let _: Result<(), _> =
                        self.proxy(profile.as_str(), PROFILE).await?.call("Delete", &()).await;
                }
                return Err(Error::failed(format!(
                    "couldn't join {ssid:?}: wrong password, or out of range?"
                )));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    pub async fn disconnect(&self) -> Result<(), Error> {
        let device = self.wifi_device().await?.ok_or_else(|| Error::unavailable("no Wi-Fi device"))?;
        let _: () = self.proxy(&device, DEVICE).await?.call("Disconnect", &()).await?;
        Ok(())
    }

    pub async fn forget(&self, ssid: &str) -> Result<(), Error> {
        let profiles = self.profiles().await?;
        let profile =
            profiles.get(ssid).ok_or_else(|| Error::not_found(format!("{ssid:?} isn't a saved network")))?;
        let _: () = self.proxy(profile, PROFILE).await?.call("Delete", &()).await?;
        Ok(())
    }

    pub async fn set_wifi(&self, on: bool) -> Result<(), Error> {
        self.proxy(NM_PATH, NM)
            .await?
            .set_property("WirelessEnabled", on)
            .await
            .map_err(zbus::Error::from)?;
        Ok(())
    }
}
