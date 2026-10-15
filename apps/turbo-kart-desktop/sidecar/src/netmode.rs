//! E6.INTERNET: the sidecar's network mode, chosen once at startup.
//!
//! `Lan` (no `--relay-url`) is exactly the v0.1 behaviour: direct UDP only,
//! clock readiness ≤ 15 ms. `Internet` is opt-in: the game mesh may also use
//! one TLS iroh relay (hole punching, address discovery, relay fallback) and
//! race readiness accepts a measured clock uncertainty up to a hard 60 ms.
//! The relay only forwards end-to-end encrypted QUIC; peers still
//! authenticate each other by EndpointId and RaceAdmission is unchanged.

use holochain_moq_turbo_kart::MAX_RELAY_URL_LEN;
use iroh::RelayUrl;

/// How much clock uncertainty a race start may accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimingTier {
    pub name: &'static str,
    /// Inclusive ceiling on a clock estimate's error bound.
    pub max_error_bound_us: u64,
}

/// WP4's ±20 ms start-agreement budget ("Fair"), unchanged since E6.4.
pub const LAN_TIER: TimingTier = TimingTier { name: "lan", max_error_bound_us: 15_000 };
/// Experimental, opt-in: GO may differ between screens by up to the measured
/// bound (≤ 60 ms). Not a fairness or finish-order guarantee.
pub const INTERNET_TIER: TimingTier = TimingTier { name: "internet", max_error_bound_us: 60_000 };

impl TimingTier {
    pub fn accepts(&self, error_bound_us: u64) -> bool {
        error_bound_us <= self.max_error_bound_us
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetMode {
    Lan,
    Internet { relay: RelayUrl },
}

impl NetMode {
    pub fn tier(&self) -> TimingTier {
        match self {
            Self::Lan => LAN_TIER,
            Self::Internet { .. } => INTERNET_TIER,
        }
    }
    pub fn relay(&self) -> Option<&RelayUrl> {
        match self {
            Self::Lan => None,
            Self::Internet { relay } => Some(relay),
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Self::Lan => "lan",
            Self::Internet { .. } => "internet",
        }
    }
}

/// An `https://host[:port][/path]` relay URL: no plaintext, credentials,
/// query or fragment; within the lobby/presence URL length limit.
pub fn parse_relay_url(s: &str) -> Result<RelayUrl, String> {
    if s.is_empty() || s.len() > MAX_RELAY_URL_LEN {
        return Err(format!("relay URL must be 1..={MAX_RELAY_URL_LEN} bytes"));
    }
    if !s.is_ascii() || s.chars().any(|c| c.is_ascii_whitespace() || c.is_ascii_control()) {
        return Err("relay URL must be printable ASCII without spaces".into());
    }
    // Checked on the raw text: URL parsing would quietly turn `https:///x` into host `x`.
    if !s.strip_prefix("https://").is_some_and(|r| !r.is_empty() && !r.starts_with('/')) {
        return Err("relay URL must be https://<host>[:port][/path] (no plaintext relay)".into());
    }
    let url: RelayUrl = s.parse().map_err(|e| format!("invalid relay URL: {e}"))?;
    if url.scheme() != "https" {
        return Err("relay URL must use https (no plaintext relay)".into());
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("relay URL needs a host".into());
    }
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err("relay URL must not contain credentials, a query or a fragment".into());
    }
    Ok(url)
}

/// `--endpoint <path> [--relay-url <https url>]`, in either order, each once.
pub fn parse_args(args: &[String]) -> Result<(String, NetMode), String> {
    let (mut endpoint, mut relay) = (None, None);
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        let slot = match flag.as_str() {
            "--endpoint" => &mut endpoint,
            "--relay-url" => &mut relay,
            other => return Err(format!("unknown argument {other:?}")),
        };
        if slot.replace(value.clone()).is_some() {
            return Err(format!("{flag} given twice"));
        }
    }
    let endpoint = endpoint.filter(|p| !p.is_empty()).ok_or("--endpoint <path> is required")?;
    let mode = match relay {
        None => NetMode::Lan,
        Some(r) => NetMode::Internet { relay: parse_relay_url(&r)? },
    };
    Ok((endpoint, mode))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn endpoint_only_is_lan_mode() {
        let (e, m) = parse_args(&args(&["--endpoint", "/run/x.sock"])).unwrap();
        assert_eq!((e.as_str(), &m), ("/run/x.sock", &NetMode::Lan));
        assert_eq!(m.tier(), LAN_TIER);
        assert!(m.relay().is_none());
    }

    #[test]
    fn relay_url_selects_internet_mode_in_either_order() {
        for a in [
            args(&["--relay-url", "https://relay.example/", "--endpoint", "/s"]),
            args(&["--endpoint", "/s", "--relay-url", "https://relay.example/"]),
        ] {
            let (e, m) = parse_args(&a).unwrap();
            assert_eq!(e, "/s");
            assert_eq!(m.tier(), INTERNET_TIER);
            assert_eq!(m.relay().unwrap().as_str(), "https://relay.example/");
        }
    }

    #[test]
    fn malformed_arguments_are_refused() {
        for a in [
            args(&[]),
            args(&["--endpoint"]),
            args(&["--endpoint", ""]),
            args(&["--endpoint", "/s", "--endpoint", "/t"]),
            args(&["--endpoint", "/s", "--relay-url", "https://a/", "--relay-url", "https://b/"]),
            args(&["--endpoint", "/s", "--verbose", "1"]),
            args(&["--relay-url", "https://relay.example/"]),
        ] {
            assert!(parse_args(&a).is_err(), "{a:?}");
        }
    }

    #[test]
    fn only_clean_https_relay_urls_are_accepted() {
        assert!(parse_relay_url("https://dev-test-bootstrap2.holochain.org/").is_ok());
        assert!(parse_relay_url("https://relay.example:8443/relay").is_ok());
        for bad in [
            "http://relay.example/",
            "ws://relay.example/",
            "ftp://relay.example/",
            "relay.example",
            "https://",
            "https:///path",
            "https://user:pw@relay.example/",
            "https://relay.example/?token=1",
            "https://relay.example/#x",
            "https://relay .example/",
            "https://relay.example/\n",
            "",
        ] {
            assert!(parse_relay_url(bad).is_err(), "{bad:?}");
        }
        let long = format!("https://{}.example/", "r".repeat(MAX_RELAY_URL_LEN));
        assert!(parse_relay_url(&long).is_err());
    }

    #[test]
    fn tiers_accept_exactly_up_to_their_ceiling() {
        assert!(LAN_TIER.accepts(0));
        assert!(LAN_TIER.accepts(15_000));
        assert!(!LAN_TIER.accepts(15_001));
        assert!(INTERNET_TIER.accepts(15_001));
        assert!(INTERNET_TIER.accepts(60_000));
        assert!(!INTERNET_TIER.accepts(60_001));
        assert!(!INTERNET_TIER.accepts(u64::MAX));
    }

    #[test]
    fn lan_tier_equals_the_existing_fair_quality_gate() {
        use holochain_moq_clock::{ClockEstimate, ClockQuality};
        for bound in [0, 5_000, 5_001, 14_999, 15_000, 15_001, 30_000, 60_000, 60_001] {
            let est = ClockEstimate { error_bound_us: bound, ..ClockEstimate::host() };
            assert_eq!(LAN_TIER.accepts(bound), est.quality() <= ClockQuality::Fair, "{bound}");
        }
    }
}
