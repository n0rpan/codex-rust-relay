//! What `tailscale serve status --json` says about OX's HTTPS port. Shared by
//! the desktop's Remote access setup and `ox host doctor`, so both judge
//! Funnel and conflicting mappings the same way.
use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Config {
    /// Nothing serves the port.
    Free,
    /// The port serves exactly the expected origin, proxied to OX's loopback listener.
    Owned,
    /// Something else uses the port.
    Conflict,
    /// Funnel publishes the port to the internet.
    Public,
}

/// `https` is the Serve HTTPS port ("8443"), `proxy` the loopback URL OX
/// expects (`http://127.0.0.1:48162`).
pub fn configuration(v: &Value, authority: &str, https: &str, proxy: &str) -> Config {
    let port_match = |name: &str| name.rsplit_once(':').is_some_and(|(_, p)| p == https);
    if v["AllowFunnel"]
        .as_object()
        .is_some_and(|m| m.iter().any(|(k, b)| port_match(k) && b == true))
    {
        return Config::Public;
    }
    if let Some(fg) = v["Foreground"].as_object() {
        for f in fg.values() {
            match configuration(f, authority, https, proxy) {
                Config::Public => return Config::Public,
                Config::Free => (),
                _ => return Config::Conflict,
            }
        }
    }
    let tcp = &v["TCP"][https];
    let mut webs = v["Web"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| port_match(k));
    match (webs.next(), webs.next()) {
        (None, None) if tcp.is_null() => Config::Free,
        (Some((key, web)), None)
            if key == authority
                && tcp["HTTPS"] == true
                && web["Handlers"].as_object().is_some_and(|m| m.len() == 1)
                && web["Handlers"]["/"]["Proxy"] == proxy =>
        {
            Config::Owned
        }
        _ => Config::Conflict,
    }
}
