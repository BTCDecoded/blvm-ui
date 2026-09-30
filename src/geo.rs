//! Rough peer locations from a local IP database. Nothing leaves this machine.
//!
//! Uses the DB-IP "IP to Country Lite" database (CC BY 4.0, <https://db-ip.com>), a
//! MaxMind-format `.mmdb` file of about 8 MB. It is looked up at `BLVM_UI_GEO_DB`
//! (runtime, then build time), next to the binary under `geo/`, or in this crate's
//! `geo/` folder. The larger City Lite file also works if that is what is present.
//! Without a file, peers simply have no location.
//!
//! The country file has no coordinates, so each country is placed at the average of
//! its tzdb time zone cities (built into the binary), and peers in the same country
//! get a small fixed offset so their dots do not stack.
//!
//! This node's own dot follows `BLVM_UI_SELF_GEO` (runtime env, else the value
//! baked in at build time, else `auto`):
//!
//! - `auto`: public address the node reports for itself, else the time zone
//! - `ip`: only the node-reported address
//! - `timezone`: only the machine time zone (`TZ`, `/etc/localtime`, `/etc/timezone`)
//! - `fixed:LAT,LON` or `fixed:LAT,LON,Label`: a set position (for boxes like
//!   Umbrel / Start9 whose containers run in UTC)
//! - `off`: no dot

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::OnceLock;

use maxminddb::{geoip2, Mmap, Reader};
use serde::Serialize;

use crate::settings::setting;

/// Looked for in this order; the country file is the default download.
pub const DB_FILES: [&str; 2] = ["dbip-country-lite.mmdb", "dbip-city-lite.mmdb"];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PeerGeo {
    pub lat: f64,
    pub lon: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// How this location was found: `"ip"` or `"timezone"` (only set for this node).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<&'static str>,
}

static READER: OnceLock<Option<Reader<Mmap>>> = OnceLock::new();

fn db_path() -> Option<PathBuf> {
    if let Some(p) = setting("BLVM_UI_GEO_DB", option_env!("BLVM_UI_GEO_DB")) {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.join("geo"));
        }
    }
    dirs.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("geo"));
    dirs.iter()
        .flat_map(|d| DB_FILES.iter().map(move |f| d.join(f)))
        .find(|p| p.is_file())
}

fn reader() -> Option<&'static Reader<Mmap>> {
    READER
        .get_or_init(|| {
            let path = db_path()?;
            // SAFETY: the database file is read-only data we downloaded; it is not
            // modified while the console runs.
            match unsafe { Reader::open_mmap(&path) } {
                Ok(r) => {
                    tracing::info!("peer geolocation: using {}", path.display());
                    Some(r)
                }
                Err(e) => {
                    tracing::warn!("peer geolocation disabled: {}: {e}", path.display());
                    None
                }
            }
        })
        .as_ref()
}

/// IP from a node `addr` field: `1.2.3.4:18333`, `[2001:db8::1]:18333`, or a bare IP.
pub fn peer_ip(addr: &str) -> Option<IpAddr> {
    let addr = addr.trim();
    addr.parse::<SocketAddr>()
        .map(|s| s.ip())
        .or_else(|_| addr.trim_matches(['[', ']']).parse::<IpAddr>())
        .ok()
}

/// Only public addresses have a meaningful location.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                || (o[0] == 100 && (o[1] & 0xc0) == 64))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80)
        }
    }
}

/// Rough location for a peer address, or `None` (private IP, unknown, or no database).
pub fn locate(addr: &str) -> Option<PeerGeo> {
    let ip = peer_ip(addr)?;
    if !is_public(ip) {
        return None;
    }
    // `City` decodes both files; the country file just leaves city/location empty.
    let rec: geoip2::City = reader()?.lookup(ip).ok()?.decode().ok()??;
    let country = rec.country.names.english.map(str::to_string);
    if let (Some(lat), Some(lon)) = (rec.location.latitude, rec.location.longitude) {
        return Some(PeerGeo {
            lat,
            lon,
            city: rec.city.names.english.map(str::to_string),
            country,
            source: None,
        });
    }
    let (lat, lon) = country_center(rec.country.iso_code?)?;
    let (lat, lon) = spread(lat, lon, &ip.to_string());
    Some(PeerGeo { lat, lon, city: None, country, source: None })
}

/// Rough center of a country (ISO 3166 alpha-2), from its tzdb zone cities.
pub fn country_center(iso: &str) -> Option<(f64, f64)> {
    static CENTERS: OnceLock<std::collections::HashMap<String, (f64, f64)>> = OnceLock::new();
    CENTERS.get_or_init(|| country_centers(ZONE_TAB)).get(&iso.to_ascii_uppercase()).copied()
}

/// Average position of every zone city per country. Longitude uses a circular mean so
/// countries across the 180° line (Fiji, Kiribati, Russia) stay in the right place.
fn country_centers(tab: &str) -> std::collections::HashMap<String, (f64, f64)> {
    let mut acc: std::collections::HashMap<String, (f64, f64, f64, u32)> = std::collections::HashMap::new();
    for line in tab.lines().filter(|l| !l.starts_with('#')) {
        let cols: Vec<&str> = line.split('\t').collect();
        let (Some(cc), Some(coords)) = (cols.first(), cols.get(1)) else {
            continue;
        };
        let Some((lat, lon)) = parse_iso6709(coords) else {
            continue;
        };
        let e = acc.entry(cc.to_string()).or_insert((0.0, 0.0, 0.0, 0));
        e.0 += lat;
        e.1 += lon.to_radians().sin();
        e.2 += lon.to_radians().cos();
        e.3 += 1;
    }
    acc.into_iter()
        .map(|(cc, (lat, s, c, n))| (cc, (lat / n as f64, s.atan2(c).to_degrees())))
        .collect()
}

/// Fixed small offset per IP (about 1.5° at most) so peers in one country do not
/// sit on the exact same spot.
fn spread(lat: f64, lon: f64, ip: &str) -> (f64, f64) {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in ip.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    let angle = (h & 0xffff) as f64 / 65_536.0 * std::f64::consts::TAU;
    let radius = 0.4 + ((h >> 16) & 0xffff) as f64 / 65_536.0 * 1.1;
    let lat = (lat + radius * angle.sin()).clamp(-89.0, 89.0);
    let mut lon = lon + radius * angle.cos();
    if lon > 180.0 {
        lon -= 360.0;
    } else if lon < -180.0 {
        lon += 360.0;
    }
    (lat, lon)
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelfMode {
    Auto,
    Ip,
    Timezone,
    Fixed(PeerGeo),
    Off,
}

/// Parse a `BLVM_UI_SELF_GEO` value. Unknown values fall back to `auto`.
pub fn parse_self_mode(v: &str) -> SelfMode {
    let v = v.trim();
    match v.to_ascii_lowercase().as_str() {
        "" | "auto" => return SelfMode::Auto,
        "ip" => return SelfMode::Ip,
        "timezone" | "tz" => return SelfMode::Timezone,
        "off" | "none" | "0" => return SelfMode::Off,
        _ => {}
    }
    if let Some(rest) = v.strip_prefix("fixed:") {
        let mut parts = rest.splitn(3, ',');
        let lat = parts.next().and_then(|x| x.trim().parse::<f64>().ok());
        let lon = parts.next().and_then(|x| x.trim().parse::<f64>().ok());
        let label = parts.next().map(|x| x.trim().to_string()).filter(|x| !x.is_empty());
        if let (Some(lat), Some(lon)) = (lat, lon) {
            if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) {
                return SelfMode::Fixed(PeerGeo { lat, lon, city: label, country: None, source: Some("fixed") });
            }
        }
    }
    tracing::warn!("unknown BLVM_UI_SELF_GEO={v:?}; using auto");
    SelfMode::Auto
}

fn self_mode() -> &'static SelfMode {
    static MODE: OnceLock<SelfMode> = OnceLock::new();
    MODE.get_or_init(|| {
        parse_self_mode(&setting("BLVM_UI_SELF_GEO", option_env!("BLVM_UI_SELF_GEO")).unwrap_or_default())
    })
}

/// Where this node is, roughly, following [`self_mode`]. Fully offline in every mode.
pub fn self_geo(net: Option<&serde_json::Value>, peers: &[serde_json::Value]) -> Option<PeerGeo> {
    match self_mode() {
        SelfMode::Off => None,
        SelfMode::Fixed(g) => Some(g.clone()),
        SelfMode::Timezone => timezone_geo().cloned(),
        SelfMode::Ip => self_ip_geo(net, peers),
        SelfMode::Auto => self_ip_geo(net, peers).or_else(|| timezone_geo().cloned()),
    }
}

/// Public address the node reports for itself (`localaddresses`, or a peer's
/// `addrlocal`), looked up in the local database.
fn self_ip_geo(net: Option<&serde_json::Value>, peers: &[serde_json::Value]) -> Option<PeerGeo> {
    let mut cands: Vec<String> = Vec::new();
    if let Some(list) = net.and_then(|n| n.get("localaddresses")).and_then(|v| v.as_array()) {
        for a in list {
            if let Some(ip) = a.get("address").and_then(|v| v.as_str()) {
                cands.push(ip.to_string());
            }
        }
    }
    for p in peers {
        if let Some(a) = p.get("addrlocal").and_then(|v| v.as_str()) {
            if !a.is_empty() {
                cands.push(a.to_string());
            }
        }
    }
    cands.into_iter().find_map(|c| {
        locate(&c).map(|mut g| {
            g.source = Some("ip");
            g
        })
    })
}

/// Public-domain tzdb table built into the binary, for systems or containers
/// without `/usr/share/zoneinfo`.
const ZONE_TAB: &str = include_str!("../geo/zone.tab");

static TZ_GEO: OnceLock<Option<PeerGeo>> = OnceLock::new();

fn timezone_geo() -> Option<&'static PeerGeo> {
    TZ_GEO
        .get_or_init(|| {
            let tz = local_tz_name()?;
            for tab in ["/usr/share/zoneinfo/zone1970.tab", "/usr/share/zoneinfo/zone.tab"] {
                if let Ok(text) = std::fs::read_to_string(tab) {
                    if let Some(g) = tz_from_tab(&text, &tz) {
                        return Some(g);
                    }
                }
            }
            tz_from_tab(ZONE_TAB, &tz)
        })
        .as_ref()
}

/// IANA zone name (`America/New_York`) from `TZ`, the `/etc/localtime` link
/// (macOS, most Linux), or `/etc/timezone` (Debian/Ubuntu images).
fn local_tz_name() -> Option<String> {
    if let Ok(tz) = std::env::var("TZ") {
        if let Some(name) = tz_name_from(&tz) {
            return Some(name);
        }
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        if let Some(name) = tz_name_from(&target.to_string_lossy()) {
            return Some(name);
        }
    }
    std::fs::read_to_string("/etc/timezone").ok().and_then(|s| tz_name_from(&s))
}

/// `America/New_York`, `:America/New_York`, or a path ending in `zoneinfo/America/New_York`.
fn tz_name_from(s: &str) -> Option<String> {
    let s = s.trim().trim_start_matches(':');
    let s = s.rsplit_once("zoneinfo/").map(|(_, tz)| tz).unwrap_or(s);
    (s.contains('/') && !s.starts_with('/')).then(|| s.to_string())
}

/// Find `tz` in a `zone.tab` / `zone1970.tab` file (`CC<TAB>coords<TAB>TZ...`).
fn tz_from_tab(text: &str, tz: &str) -> Option<PeerGeo> {
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() >= 3 && cols[2] == tz {
            let (lat, lon) = parse_iso6709(cols[1])?;
            let city = tz.rsplit('/').next().map(|c| c.replace('_', " "));
            return Some(PeerGeo { lat, lon, city, country: None, source: Some("timezone") });
        }
    }
    None
}

/// `+404251-0740023` (±DDMMSS±DDDMMSS) or `+4043-07400` (±DDMM±DDDMM).
fn parse_iso6709(s: &str) -> Option<(f64, f64)> {
    let split = s.char_indices().skip(1).find(|(_, c)| *c == '+' || *c == '-')?.0;
    let (a, b) = s.split_at(split);
    Some((dms(a, 2)?, dms(b, 3)?))
}

fn dms(s: &str, deg_digits: usize) -> Option<f64> {
    let sign = if s.starts_with('-') { -1.0 } else { 1.0 };
    let d = &s[1..];
    let num = |r: std::ops::Range<usize>| d.get(r).and_then(|x| x.parse::<f64>().ok());
    let deg = num(0..deg_digits)?;
    let min = num(deg_digits..deg_digits + 2)?;
    let sec = if d.len() >= deg_digits + 4 { num(deg_digits + 2..deg_digits + 4)? } else { 0.0 };
    Some(sign * (deg + min / 60.0 + sec / 3600.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_peer_addrs() {
        assert_eq!(peer_ip("52.18.153.97:18333"), Some("52.18.153.97".parse().unwrap()));
        assert_eq!(peer_ip("[2001:db8::1]:18333"), Some("2001:db8::1".parse().unwrap()));
        assert_eq!(peer_ip("8.8.8.8"), Some("8.8.8.8".parse().unwrap()));
        assert_eq!(peer_ip("not an ip"), None);
    }

    #[test]
    fn skips_private_and_local() {
        for a in ["10.21.48.230", "192.168.1.2", "127.0.0.1", "100.64.1.1", "::1", "fe80::1", "fd00::1"] {
            assert!(!is_public(a.parse().unwrap()), "{a} should not be public");
        }
        for a in ["52.18.153.97", "2a01:4f8::1"] {
            assert!(is_public(a.parse().unwrap()), "{a} should be public");
        }
        assert_eq!(locate("10.21.48.230:18333"), None);
    }

    #[test]
    fn parses_zone_tab_coordinates() {
        let (lat, lon) = parse_iso6709("+404251-0740023").unwrap();
        assert!((lat - 40.714).abs() < 0.01 && (lon + 74.006).abs() < 0.01);
        let (lat, lon) = parse_iso6709("-3352+15113").unwrap();
        assert!((lat + 33.867).abs() < 0.01 && (lon - 151.217).abs() < 0.01);
    }

    #[test]
    fn finds_timezone_in_tab() {
        let tab = "# comment\nUS\t+404251-0740023\tAmerica/New_York\tEastern (most areas)\nAU\t-3352+15113\tAustralia/Sydney\n";
        let g = tz_from_tab(tab, "America/New_York").unwrap();
        assert_eq!(g.city.as_deref(), Some("New York"));
        assert_eq!(g.source, Some("timezone"));
        assert!(tz_from_tab(tab, "Europe/Paris").is_none());
    }

    #[test]
    fn reads_timezone_names_from_env_links_and_files() {
        assert_eq!(tz_name_from(":America/New_York").as_deref(), Some("America/New_York"));
        assert_eq!(
            tz_name_from("/var/db/timezone/zoneinfo/America/New_York").as_deref(),
            Some("America/New_York")
        );
        assert_eq!(tz_name_from("../usr/share/zoneinfo/Europe/Berlin").as_deref(), Some("Europe/Berlin"));
        assert_eq!(tz_name_from("Europe/Lisbon\n").as_deref(), Some("Europe/Lisbon"));
        assert_eq!(tz_name_from("UTC"), None);
    }

    #[test]
    fn built_in_zone_table_covers_common_zones() {
        for tz in ["America/New_York", "Europe/Berlin", "Asia/Tokyo", "America/Sao_Paulo"] {
            assert!(tz_from_tab(ZONE_TAB, tz).is_some(), "{tz} missing from built-in table");
        }
    }

    #[test]
    fn parses_self_modes() {
        assert_eq!(parse_self_mode(""), SelfMode::Auto);
        assert_eq!(parse_self_mode("TimeZone"), SelfMode::Timezone);
        assert_eq!(parse_self_mode("off"), SelfMode::Off);
        assert_eq!(parse_self_mode("ip"), SelfMode::Ip);
        match parse_self_mode("fixed:52.52,13.405,Berlin") {
            SelfMode::Fixed(g) => {
                assert_eq!((g.lat, g.lon), (52.52, 13.405));
                assert_eq!(g.city.as_deref(), Some("Berlin"));
                assert_eq!(g.source, Some("fixed"));
            }
            other => panic!("expected fixed, got {other:?}"),
        }
        assert_eq!(parse_self_mode("fixed:200,0"), SelfMode::Auto);
        assert_eq!(parse_self_mode("banana"), SelfMode::Auto);
    }

    #[test]
    fn self_geo_prefers_a_public_addrlocal() {
        if db_path().is_none() {
            return;
        }
        let peers = vec![serde_json::json!({ "addrlocal": "8.8.8.8:18333" })];
        let g = self_ip_geo(None, &peers).unwrap();
        assert_eq!(g.source, Some("ip"));
    }

    #[test]
    fn locates_a_public_ip_when_the_database_is_present() {
        if db_path().is_none() {
            return; // Database not downloaded on this machine.
        }
        let g = locate("8.8.8.8:53").expect("8.8.8.8 should resolve");
        assert!((-90.0..=90.0).contains(&g.lat) && (-180.0..=180.0).contains(&g.lon));
        assert!(g.country.is_some());
    }

    #[test]
    fn country_centers_land_inside_their_countries() {
        // (code, lat range, lon range): generous boxes around each country.
        for (cc, lat, lon) in [
            ("US", 25.0..50.0, -125.0..-80.0),
            ("IE", 51.0..56.0, -11.0..-5.0),
            ("DE", 47.0..55.0, 5.0..15.0),
            ("BR", -30.0..0.0, -65.0..-35.0),
            ("JP", 30.0..46.0, 128.0..146.0),
            ("AU", -40.0..-15.0, 115.0..155.0),
        ] {
            let (la, lo) = country_center(cc).unwrap_or_else(|| panic!("{cc} missing"));
            assert!(lat.contains(&la) && lon.contains(&lo), "{cc} at {la},{lo}");
        }
        // Across the 180° line: Fiji must stay near 178°E, not jump to 0°.
        let (_, fj) = country_center("FJ").unwrap();
        assert!(fj.abs() > 170.0, "FJ lon {fj}");
    }

    #[test]
    fn spread_is_small_and_stable() {
        let a = spread(40.0, -100.0, "1.2.3.4");
        assert_eq!(a, spread(40.0, -100.0, "1.2.3.4"));
        assert_ne!(a, spread(40.0, -100.0, "1.2.3.5"));
        assert!((a.0 - 40.0).abs() <= 1.6 && (a.1 + 100.0).abs() <= 1.6);
        let wrap = spread(0.0, 179.9, "9.9.9.9");
        assert!((-180.0..=180.0).contains(&wrap.1));
    }
}
