//! `ox weather`: a city-level forecast for the header horizon.
//!
//! The location comes only from the system timezone (no IP lookup, no prompt):
//! the IANA zone name is looked up in the tz database's `zone1970.tab` or
//! `zone.tab`, and the coordinates are rounded to two decimals before any
//! network use. The forecast is MET Norway Locationforecast 2.0 (compact),
//! cached on disk. Weather data from MET Norway, CC BY 4.0.
//! Terms: https://developer.yr.no/doc/TermsOfService/

use crate::provider::RelayError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ENDPOINT: &str = "https://api.met.no/weatherapi/locationforecast/2.0/compact";
/// MET asks clients not to poll faster than `Expires`; we also never ask twice in this window.
const MIN_INTERVAL: i64 = 30 * 60;
/// After a 429 or a server error, stay away for an hour.
const BACKOFF: i64 = 60 * 60;
const USER_AGENT_SITE: &str = "github.com/n0rpan/ox";

pub(crate) const USAGE: &str = "\
Usage: ox weather [--json]

Print the weather at the place your system timezone points to. Only the
city-level coordinates of that timezone (two decimals) are sent to api.met.no;
nothing else leaves the machine. Results are cached and the server's Expires
header is honoured. Turn it off with OX_WEATHER=0 or in OX Desktop preferences.
Weather data from MET Norway (CC BY 4.0).";

// ---------------------------------------------------------------- timezone

/// A plausible IANA zone name such as `Europe/Oslo` or `America/Argentina/Buenos_Aires`.
fn valid_zone(zone: &str) -> bool {
    !zone.is_empty()
        && zone.len() < 64
        && !zone.starts_with('/')
        && !zone.contains("..")
        && zone
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'+'))
}

/// The zone named by a `TZ` value, which may carry a leading colon.
fn zone_from_env(tz: &str) -> Option<String> {
    let zone = tz.strip_prefix(':').unwrap_or(tz);
    (valid_zone(zone) && zone.contains('/')).then(|| zone.to_owned())
}

/// The zone named by the target of `/etc/localtime`, on Linux (`/usr/share/zoneinfo/Europe/Oslo`)
/// and macOS (`/var/db/timezone/zoneinfo/Europe/Oslo`).
fn zone_from_link(target: &Path) -> Option<String> {
    let text = target.to_str()?;
    let (_, zone) = text.rsplit_once("zoneinfo/")?;
    (valid_zone(zone) && zone.contains('/')).then(|| zone.to_owned())
}

fn system_zone() -> Option<String> {
    if let Some(zone) = std::env::var("TZ").ok().as_deref().and_then(zone_from_env) {
        return Some(zone);
    }
    if let Some(zone) = std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|target| zone_from_link(&target))
    {
        return Some(zone);
    }
    let out = std::process::Command::new("timedatectl")
        .args(["show", "-p", "Timezone", "--value"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let zone = String::from_utf8(out.stdout).ok()?;
    let zone = zone.trim();
    (out.status.success() && valid_zone(zone)).then(|| zone.to_owned())
}

/// Parse ISO 6709 `+DDMM+DDDMM` or `+DDMMSS+DDDMMSS` into signed decimal degrees.
pub(crate) fn parse_iso6709(text: &str) -> Option<(f64, f64)> {
    let split = text.get(1..)?.find(['+', '-'])? + 1;
    let (lat, lon) = text.split_at(split);
    Some((degrees(lat, 2)?, degrees(lon, 3)?))
}

fn degrees(part: &str, degree_digits: usize) -> Option<f64> {
    let sign = match part.as_bytes().first()? {
        b'+' => 1.0,
        b'-' => -1.0,
        _ => return None,
    };
    let digits = &part[1..];
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number = |range: std::ops::Range<usize>| digits.get(range)?.parse::<f64>().ok();
    let value = match digits.len() - degree_digits {
        2 => number(0..degree_digits)? + number(degree_digits..degree_digits + 2)? / 60.0,
        4 => {
            number(0..degree_digits)?
                + number(degree_digits..degree_digits + 2)? / 60.0
                + number(degree_digits + 2..degree_digits + 4)? / 3600.0
        }
        _ => return None,
    };
    Some(sign * value)
}

/// Find a zone's coordinates in the text of a `zone1970.tab` or `zone.tab` file.
pub(crate) fn lookup_zone(tab: &str, zone: &str) -> Option<(f64, f64)> {
    tab.lines()
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| {
            let mut fields = line.split('\t');
            let (_, coords, name) = (fields.next()?, fields.next()?, fields.next()?);
            (name == zone).then(|| parse_iso6709(coords))?
        })
}

fn zone_coordinates(zone: &str) -> Option<(f64, f64)> {
    // zone1970.tab merges zones with identical history (Oslo is listed under Berlin),
    // so zone.tab is the fallback for the names it omits.
    ["zone1970.tab", "zone.tab"].iter().find_map(|file| {
        ["/usr/share/zoneinfo", "/var/db/timezone/zoneinfo"]
            .iter()
            .find_map(|dir| {
                let tab = std::fs::read_to_string(Path::new(dir).join(file)).ok()?;
                lookup_zone(&tab, zone)
            })
    })
}

pub(crate) fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

// ----------------------------------------------------------------- symbols

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sky {
    pub condition: &'static str,
    pub night: bool,
}

/// Map a MET symbol code such as `lightrainshowersandthunder_night` to a coarse condition.
pub(crate) fn map_symbol(code: &str) -> Option<Sky> {
    let night = code.ends_with("_night");
    let base = code
        .strip_suffix("_night")
        .or_else(|| code.strip_suffix("_day"))
        .or_else(|| code.strip_suffix("_polartwilight"))
        .unwrap_or(code);
    let condition = if base.contains("thunder") {
        "thunder"
    } else if base.contains("sleet") {
        "sleet"
    } else if base.contains("snow") {
        "snow"
    } else if base.contains("rain") {
        "rain"
    } else if base == "fog" {
        "fog"
    } else if base == "partlycloudy" {
        "partly_cloudy"
    } else if base == "cloudy" {
        "cloudy"
    } else if base == "clearsky" || base == "fair" {
        "clear"
    } else {
        return None;
    };
    Some(Sky { condition, night })
}

// ------------------------------------------------------------------- cache

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Cache {
    pub lat: f64,
    pub lon: f64,
    /// Unix seconds of the last request we made, answered or not.
    pub fetched_at: i64,
    /// Unix seconds before which we must not ask again.
    pub next_allowed: i64,
    pub last_modified: Option<String>,
    pub updated: Option<String>,
    pub symbol: Option<String>,
    pub temperature: Option<f64>,
}

impl Cache {
    fn same_place(&self, lat: f64, lon: f64) -> bool {
        (self.lat - lat).abs() < 0.005 && (self.lon - lon).abs() < 0.005
    }

    /// True while a request would be too early for this place.
    pub(crate) fn fresh(&self, lat: f64, lon: f64, now: i64) -> bool {
        self.same_place(lat, lon) && now < self.next_allowed
    }
}

/// What the server said, reduced to what the cache needs.
pub(crate) struct Reply {
    pub status: u16,
    pub expires: Option<i64>,
    pub last_modified: Option<String>,
    pub body: String,
}

/// Fold a reply into the cache. Pure, so the freshness rules are testable offline.
pub(crate) fn absorb(prev: Option<Cache>, lat: f64, lon: f64, now: i64, reply: &Reply) -> Cache {
    let mut cache = prev.filter(|c| c.same_place(lat, lon)).unwrap_or(Cache {
        lat,
        lon,
        ..Cache::default()
    });
    cache.lat = lat;
    cache.lon = lon;
    cache.fetched_at = now;
    let polite = now + MIN_INTERVAL;
    match reply.status {
        200 | 203 => {
            if let Some((updated, symbol, temperature)) = parse_forecast(&reply.body) {
                cache.updated = updated;
                cache.symbol = symbol;
                cache.temperature = temperature;
                cache.last_modified = reply.last_modified.clone();
            }
            cache.next_allowed = reply.expires.unwrap_or(0).max(polite);
        }
        304 => cache.next_allowed = reply.expires.unwrap_or(0).max(polite),
        // 429 (and server trouble): keep the old data and stay away longer.
        _ => cache.next_allowed = now + BACKOFF,
    }
    cache
}

fn parse_forecast(body: &str) -> Option<(Option<String>, Option<String>, Option<f64>)> {
    let value: Value = serde_json::from_str(body).ok()?;
    let props = value.get("properties")?;
    let first = props.get("timeseries")?.get(0)?;
    let data = first.get("data")?;
    let symbol = ["next_1_hours", "next_6_hours", "next_12_hours"]
        .iter()
        .find_map(|key| data.get(key)?.pointer("/summary/symbol_code")?.as_str())
        .map(str::to_owned);
    let temperature = data
        .pointer("/instant/details/air_temperature")
        .and_then(Value::as_f64);
    let updated = props
        .pointer("/meta/updated_at")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some((updated, symbol, temperature))
}

/// Parse an HTTP date such as `Wed, 25 Nov 2026 12:00:00 GMT` into unix seconds.
pub(crate) fn parse_http_date(text: &str) -> Option<i64> {
    let mut parts = text.split_whitespace();
    parts.next()?;
    let day: i64 = parts.next()?.parse().ok()?;
    let name = parts.next()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == name)? as i64
        + 1;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':').map(|n| n.parse::<i64>().ok());
    let (h, m, s) = (clock.next()??, clock.next()??, clock.next()??);
    // Days from civil (Howard Hinnant).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + m * 60 + s)
}

// ------------------------------------------------------------------ output

/// Whether weather is switched off by the environment or the desktop preference.
fn disabled(env: Option<&str>, preferences: Option<&str>) -> bool {
    if env.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no"
        )
    }) {
        return true;
    }
    preferences
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .and_then(|v| v.get("live_weather")?.as_bool())
        == Some(false)
}

fn state_dir() -> Option<PathBuf> {
    let absolute = |name: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    absolute("XDG_STATE_HOME")
        .or_else(|| {
            absolute("HOME").map(|h| {
                h.join(if cfg!(target_os = "macos") {
                    "Library/Application Support"
                } else {
                    ".local/state"
                })
            })
        })
        .map(|d| d.join("ox"))
}

fn read_cache(path: &Path) -> Option<Cache> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_cache(path: &Path, cache: &Cache) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let Some(dir) = path.parent() else { return };
    let temp = dir.join(format!(".weather.{}.tmp", std::process::id()));
    let result = std::fs::create_dir_all(dir)
        .and_then(|()| {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&temp)?;
            file.write_all(&serde_json::to_vec(cache)?)
        })
        .and_then(|()| std::fs::rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
}

/// The JSON `ox weather --json` prints. `cache` is whatever forecast we hold.
pub(crate) fn report(
    zone: Option<&str>,
    place: Option<(f64, f64)>,
    cache: Option<&Cache>,
    off: bool,
) -> Value {
    let mut out = json!({ "zone": zone, "condition": null });
    let object = out.as_object_mut().expect("object literal");
    if let Some((lat, lon)) = place {
        object.insert("lat".into(), json!(lat));
        object.insert("lon".into(), json!(lon));
    }
    if off {
        object.insert("disabled".into(), json!(true));
        return out;
    }
    let Some(cache) = cache else { return out };
    let Some(sky) = cache.symbol.as_deref().and_then(map_symbol) else {
        return out;
    };
    object.insert("condition".into(), json!(sky.condition));
    object.insert("night".into(), json!(sky.night));
    object.insert("symbol".into(), json!(cache.symbol));
    object.insert("temperature".into(), json!(cache.temperature));
    object.insert("updated".into(), json!(cache.updated));
    object.insert("expires".into(), json!(cache.next_allowed));
    out
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

async fn fetch(prev: Option<&Cache>, lat: f64, lon: f64) -> Option<Reply> {
    let client = reqwest::Client::builder()
        .user_agent(format!("ox/{} {USER_AGENT_SITE}", crate::VERSION))
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;
    let mut request = client
        .get(ENDPOINT)
        .query(&[("lat", format!("{lat:.2}")), ("lon", format!("{lon:.2}"))]);
    if let Some(since) = prev.and_then(|c| c.last_modified.as_deref()) {
        request = request.header("If-Modified-Since", since);
    }
    let response = request.send().await.ok()?;
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let status = response.status().as_u16();
    let expires = header("expires").and_then(|v| parse_http_date(&v));
    let last_modified = header("last-modified");
    let body = if matches!(status, 200 | 203) {
        // A compact forecast is tens of kilobytes; refuse anything absurd.
        if response.content_length().is_some_and(|n| n > 4 << 20) {
            return None;
        }
        response.text().await.ok()?
    } else {
        String::new()
    };
    Some(Reply {
        status,
        expires,
        last_modified,
        body,
    })
}

pub(crate) async fn run(args: &[OsString]) -> Result<u8, RelayError> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        [flag] if flag == "-h" || flag == "--help" => {
            println!("{USAGE}");
            return Ok(0);
        }
        _ => {
            return Err(format!(
                "unexpected argument\n  fix: {}",
                USAGE.lines().next().unwrap_or("")
            )
            .into());
        }
    };
    let zone = system_zone();
    let place = zone
        .as_deref()
        .and_then(zone_coordinates)
        .map(|(lat, lon)| (round2(lat), round2(lon)));
    let preferences =
        state_dir().and_then(|d| std::fs::read_to_string(d.join("desktop/preferences.json")).ok());
    let off = disabled(
        std::env::var("OX_WEATHER").ok().as_deref(),
        preferences.as_deref(),
    );
    let path = state_dir().map(|d| d.join("weather.json"));
    let mut cache = path.as_deref().and_then(read_cache);
    if let (false, Some((lat, lon))) = (off, place) {
        let now = now_secs();
        if !cache.as_ref().is_some_and(|c| c.fresh(lat, lon, now)) {
            let prev = cache.clone().filter(|c| c.same_place(lat, lon));
            let reply = fetch(prev.as_ref(), lat, lon).await.unwrap_or(Reply {
                status: 0,
                expires: None,
                last_modified: None,
                body: String::new(),
            });
            let next = absorb(prev, lat, lon, now, &reply);
            if let Some(path) = &path {
                write_cache(path, &next);
            }
            cache = Some(next);
        }
    }
    let shown = cache
        .as_ref()
        .filter(|c| place.is_some_and(|(lat, lon)| c.same_place(lat, lon)));
    let out = report(zone.as_deref(), place, shown, off);
    if json {
        println!("{out}");
    } else if let Some(condition) = out["condition"].as_str() {
        let temperature = out["temperature"]
            .as_f64()
            .map(|t| format!(", {t:.0} C"))
            .unwrap_or_default();
        println!(
            "{}: {}{temperature}\nWeather data from MET Norway (CC BY 4.0).",
            zone.as_deref().unwrap_or("unknown zone"),
            condition.replace('_', " ")
        );
    } else if off {
        println!("Live weather is off (OX_WEATHER or OX Desktop preferences).");
    } else {
        println!("No weather available for this timezone right now.");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.005
    }

    #[test]
    fn iso6709_both_forms() {
        let (lat, lon) = parse_iso6709("+5530+01322").unwrap();
        assert!(close(lat, 55.5) && close(lon, 13.0 + 22.0 / 60.0));
        let (lat, lon) = parse_iso6709("+595400+0104500").unwrap();
        assert!(close(lat, 59.9) && close(lon, 10.75));
        let (lat, lon) = parse_iso6709("-3352+15113").unwrap();
        assert!(lat < -33.8 && lon > 151.2);
        let (lat, lon) = parse_iso6709("-345400-0583800").unwrap();
        assert!(close(lat, -34.9) && close(lon, -(58.0 + 38.0 / 60.0)));
        assert_eq!(parse_iso6709("5530+01322"), None);
        assert_eq!(parse_iso6709("+55+013"), None);
        assert_eq!(parse_iso6709(""), None);
    }

    #[test]
    fn zone_lookup_and_names() {
        let tab =
            "# comment\nNO\t+5955+01045\tEurope/Oslo\t\nDE,DK\t+5230+01322\tEurope/Berlin\tmost\n";
        let (lat, lon) = lookup_zone(tab, "Europe/Oslo").unwrap();
        assert!(close(lat, 59.0 + 55.0 / 60.0) && close(lon, 10.0 + 45.0 / 60.0));
        assert_eq!(lookup_zone(tab, "Europe/Paris"), None);
        assert_eq!(
            zone_from_env(":Europe/Oslo").as_deref(),
            Some("Europe/Oslo")
        );
        assert_eq!(zone_from_env("UTC"), None);
        assert_eq!(zone_from_env("/etc/passwd"), None);
        assert_eq!(
            zone_from_link(Path::new("/usr/share/zoneinfo/Europe/Oslo")).as_deref(),
            Some("Europe/Oslo")
        );
        assert_eq!(
            zone_from_link(Path::new("/var/db/timezone/zoneinfo/America/New_York")).as_deref(),
            Some("America/New_York")
        );
        assert_eq!(zone_from_link(Path::new("/etc/elsewhere")), None);
        assert_eq!(round2(59.91667), 59.92);
    }

    #[test]
    fn symbols_map_to_conditions() {
        let cases = [
            ("clearsky_day", "clear", false),
            ("fair_night", "clear", true),
            ("partlycloudy_day", "partly_cloudy", false),
            ("cloudy", "cloudy", false),
            ("lightrain", "rain", false),
            ("heavyrainshowers_night", "rain", true),
            ("lightsleetshowers_day", "sleet", false),
            ("snow", "snow", false),
            ("fog", "fog", false),
            ("lightrainandthunder", "thunder", false),
            ("heavysnowshowersandthunder_night", "thunder", true),
        ];
        for (code, condition, night) in cases {
            assert_eq!(map_symbol(code), Some(Sky { condition, night }), "{code}");
        }
        assert_eq!(map_symbol("mystery"), None);
    }

    const BODY: &str = r#"{"properties":{"meta":{"updated_at":"2026-11-25T12:00:00Z"},"timeseries":[{"data":{"instant":{"details":{"air_temperature":3.4}},"next_1_hours":{"summary":{"symbol_code":"lightsnow"}}}}]}}"#;

    #[test]
    fn http_dates_parse() {
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:10 GMT"), Some(10));
        assert_eq!(
            parse_http_date("Wed, 25 Nov 2026 12:00:00 GMT"),
            Some(1_795_608_000)
        );
        assert_eq!(parse_http_date("garbage"), None);
    }

    #[test]
    fn cache_freshness_and_backoff() {
        let ok = Reply {
            status: 200,
            expires: Some(1_000 + 600),
            last_modified: Some("Wed, 25 Nov 2026 11:50:00 GMT".into()),
            body: BODY.into(),
        };
        let cache = absorb(None, 59.92, 10.75, 1_000, &ok);
        assert_eq!(cache.symbol.as_deref(), Some("lightsnow"));
        assert_eq!(cache.temperature, Some(3.4));
        // Expires is only 10 minutes out, but we never ask more than once per 30.
        assert_eq!(cache.next_allowed, 1_000 + MIN_INTERVAL);
        assert!(cache.fresh(59.92, 10.75, 1_000 + 1_799));
        assert!(!cache.fresh(59.92, 10.75, 1_000 + 1_800));
        assert!(
            !cache.fresh(48.0, 2.0, 1_100),
            "another place is never fresh"
        );
        // A long Expires wins over the 30 minute floor.
        let long = Reply {
            expires: Some(1_000 + 7_200),
            ..ok
        };
        assert_eq!(absorb(None, 59.92, 10.75, 1_000, &long).next_allowed, 8_200);
        // 304 keeps the data and moves the clock.
        let unchanged = Reply {
            status: 304,
            expires: Some(9_000),
            last_modified: None,
            body: String::new(),
        };
        let again = absorb(Some(cache.clone()), 59.92, 10.75, 3_000, &unchanged);
        assert_eq!(again.symbol, cache.symbol);
        assert_eq!(again.last_modified, cache.last_modified);
        assert_eq!(again.next_allowed, 9_000);
        // 429 backs off for an hour and keeps the stale data.
        let limited = Reply {
            status: 429,
            expires: None,
            last_modified: None,
            body: String::new(),
        };
        let backed = absorb(Some(cache.clone()), 59.92, 10.75, 3_000, &limited);
        assert_eq!(backed.next_allowed, 3_000 + BACKOFF);
        assert_eq!(backed.symbol, cache.symbol);
        // A network failure (status 0) behaves the same.
        let failed = Reply {
            status: 0,
            ..limited
        };
        assert_eq!(absorb(None, 1.0, 2.0, 5, &failed).next_allowed, 5 + BACKOFF);
    }

    #[test]
    fn report_shapes_and_disable() {
        let cache = Cache {
            lat: 59.92,
            lon: 10.75,
            next_allowed: 77,
            symbol: Some("lightsnow".into()),
            temperature: Some(-1.0),
            updated: Some("2026-11-25T12:00:00Z".into()),
            ..Cache::default()
        };
        let out = report(
            Some("Europe/Oslo"),
            Some((59.92, 10.75)),
            Some(&cache),
            false,
        );
        assert_eq!(out["condition"], "snow");
        assert_eq!(out["expires"], 77);
        assert_eq!(out["zone"], "Europe/Oslo");
        let offline = report(Some("Europe/Oslo"), None, None, false);
        assert_eq!(offline, json!({"zone": "Europe/Oslo", "condition": null}));
        let off = report(
            Some("Europe/Oslo"),
            Some((59.92, 10.75)),
            Some(&cache),
            true,
        );
        assert!(off["condition"].is_null() && off["disabled"] == true);
        assert!(disabled(Some("0"), None));
        assert!(disabled(Some("off"), None));
        assert!(!disabled(Some("1"), None));
        assert!(disabled(None, Some(r#"{"live_weather":false}"#)));
        assert!(!disabled(None, Some(r#"{"font_size":16}"#)));
        assert!(!disabled(None, None));
    }
}
