//! The address class of the configured hosts. `init` records whether each
//! host name resolves to a public, private, loopback or link-local address.
//! `run` and `check` resolve it once, refuse a class that changed, and
//! connect only to the addresses they checked.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

use anyhow::{Context, Result, bail};
use url::Host;

use crate::auth::resolve_token_url;
use crate::config::{AddressClass, AuthConfig, Source};

pub fn classify(ip: IpAddr) -> AddressClass {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            // 0.0.0.0/8 reaches the local machine on Linux.
            if v4.is_loopback() || a == 0 {
                AddressClass::Loopback
            } else if v4.is_link_local() {
                AddressClass::LinkLocal
            } else if v4.is_private() || (a == 100 && b & 0xc0 == 64) {
                AddressClass::Private
            } else {
                AddressClass::Public
            }
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return classify(IpAddr::V4(v4));
            }
            let seg = v6.segments();
            // NAT64 (64:ff9b::/96) reaches the IPv4 address in its last 32 bits.
            if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                let o = v6.octets();
                return classify(IpAddr::V4(std::net::Ipv4Addr::new(
                    o[12], o[13], o[14], o[15],
                )));
            }
            let first = seg[0];
            if v6.is_loopback() || v6.is_unspecified() {
                AddressClass::Loopback
            } else if first & 0xffc0 == 0xfe80 {
                AddressClass::LinkLocal
            } else if first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfec0 {
                AddressClass::Private
            } else {
                AddressClass::Public
            }
        }
    }
}

/// The host names `source` connects to: the `base_url` host and, for OAuth2,
/// the `token_url` host. IP literals are left out, because their class cannot
/// change, and so are hosts written with `${VAR}`, so that the value from the
/// environment is never written to `apisip.toml`.
fn host_names(source: &Source) -> Result<Vec<String>> {
    let mut urls = vec![(
        source.base_url.as_str(),
        super::parse_base(&source.base_url)?,
    )];
    if let Some(AuthConfig::Oauth2ClientCredentials { token_url, .. }) = &source.auth {
        // Absolute once filled, as `resolve_token_url` decides, so a
        // `${TOKEN_URL}` host counts as written with `${VAR}`.
        // A `token_url` that cannot be filled yet (an unset `${VAR}`) is
        // skipped, so the `base_url` host is still recorded and checked;
        // `ApiClient::build` then fails in `Authenticator::new`, before any
        // connection.
        if let (Ok(filled), Ok(url)) = (
            crate::config::interpolate(token_url),
            resolve_token_url(token_url, &source.base_url),
        ) {
            let raw = if filled.starts_with("http") {
                token_url.as_str()
            } else {
                source.base_url.as_str()
            };
            urls.push((raw, url));
        }
    }
    let mut hosts = Vec::new();
    for (raw, url) in urls {
        if let Some(Host::Domain(d)) = url.host()
            && !authority(raw).contains("${")
            && !hosts.iter().any(|h| h == d)
        {
            hosts.push(d.to_string());
        }
    }
    Ok(hosts)
}

/// The `host:port` part of a URL as written, before any `${VAR}` is filled.
fn authority(raw: &str) -> &str {
    let rest = raw.split_once("://").map_or(raw, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or_default()
}

#[cfg(test)]
thread_local! {
    /// Answers `lookup` gives on this thread instead of asking the system.
    pub static FAKE_DNS: std::cell::RefCell<BTreeMap<String, Vec<SocketAddr>>> =
        Default::default();
}

fn lookup(host: &str) -> Result<Vec<SocketAddr>> {
    #[cfg(test)]
    if let Some(addrs) = FAKE_DNS.with_borrow(|f| f.get(host).cloned()) {
        return Ok(addrs);
    }
    let addrs: Vec<SocketAddr> = (host, 0)
        .to_socket_addrs()
        .with_context(|| format!("resolving {host}"))?
        .collect();
    if addrs.is_empty() {
        bail!("resolving {host}: no address");
    }
    Ok(addrs)
}

/// The one class all of `addrs` share, or `None` when they differ.
fn class_of(addrs: &[SocketAddr]) -> Option<AddressClass> {
    let first = classify(addrs.first()?.ip());
    addrs
        .iter()
        .all(|a| classify(a.ip()) == first)
        .then_some(first)
}

/// What `init` writes to `address_class`. A host that does not resolve, or
/// resolves to addresses of several classes, is left out.
pub fn record(source: &Source) -> BTreeMap<String, AddressClass> {
    let mut out = BTreeMap::new();
    for host in host_names(source).unwrap_or_default() {
        if let Some(class) = lookup(&host).ok().and_then(|a| class_of(&a)) {
            out.insert(host, class);
        }
    }
    out
}

/// The class `address_class` records for `host`. URL hosts are lowercase,
/// so a key written in another case still matches.
fn recorded(source: &Source, host: &str) -> Option<AddressClass> {
    source
        .address_class
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(host))
        .map(|(_, &c)| c)
}

/// The key of each URL whose host cannot be read, with the reason, which
/// names no value from the environment.
type Unreadable = Vec<(&'static str, String)>;

/// The host names `source` connects to, as `host_names` finds them, but read
/// as written, so an unset `${VAR}` outside the host does not hide a host.
fn hosts_as_written(source: &Source) -> (Vec<String>, Unreadable) {
    let mut urls = vec![("base_url", source.base_url.as_str())];
    if let Some(AuthConfig::Oauth2ClientCredentials { token_url, .. }) = &source.auth {
        urls.push(("token_url", token_url.as_str()));
    }
    let (mut hosts, mut unreadable) = (Vec::<String>::new(), Vec::new());
    for (key, raw) in urls {
        let filled = crate::config::interpolate(raw);
        // A relative `token_url` names the `base_url` host, already counted.
        let relative = key == "token_url"
            && match &filled {
                Ok(f) => !f.starts_with("http"),
                Err(_) => !raw.contains("://"),
            };
        if relative || authority(raw).contains("${") {
            continue;
        }
        let url = url::Url::parse(raw)
            .or_else(|_| -> Result<url::Url> { Ok(url::Url::parse(&filled?)?) });
        match url {
            Ok(url) => {
                if let Some(Host::Domain(d)) = url.host()
                    && !hosts.iter().any(|h| h == d)
                {
                    hosts.push(d.to_string());
                }
            }
            Err(e) => unreadable.push((key, format!("{e:#}"))),
        }
    }
    (hosts, unreadable)
}

/// Each new host with its class, `None` when it could not be classified.
pub type NewHosts = Vec<(String, Option<AddressClass>)>;

/// The hosts `source` connects to that `before` did not, with no entry in
/// `address_class` yet, each with its class as `record` finds it: `None` when
/// the host does not resolve or resolves to addresses of several classes.
/// Hosts are read as written, and a URL of `source` whose host cannot be read
/// gets a warning that names it.
pub fn new_hosts(source: &Source, before: &Source) -> NewHosts {
    let old = hosts_as_written(before).0;
    let (hosts, unreadable) = hosts_as_written(source);
    for (key, why) in unreadable {
        eprintln!(
            "warning: cannot read the host of {key} in apisip.toml ({why}), so no \
             address class is recorded for it: fix {key}, then run `apisip update` again"
        );
    }
    hosts
        .into_iter()
        .filter(|h| !old.contains(h) && recorded(source, h).is_none())
        .map(|h| {
            let class = lookup(&h).ok().and_then(|a| class_of(&a));
            (h, class)
        })
        .collect()
}

/// The checked addresses of each recorded host, for the client to connect to.
/// Fails when a host now resolves to an address of another class.
pub fn pinned(source: &Source) -> Result<Vec<(String, Vec<SocketAddr>)>> {
    let mut out = Vec::new();
    for host in host_names(source)? {
        let Some(want) = recorded(source, &host) else {
            eprintln!(
                "warning: apisip.toml records no address class for {host}, so a DNS change \
                 to an internal address goes unnoticed: add it to address_class under \
                 [source], or re-run `apisip init`"
            );
            continue;
        };
        let addrs = lookup(&host)?;
        check(&host, want, &addrs)?;
        out.push((host, addrs));
    }
    Ok(out)
}

fn check(host: &str, want: AddressClass, addrs: &[SocketAddr]) -> Result<()> {
    if let Some(a) = addrs.iter().find(|a| classify(a.ip()) != want) {
        let got = classify(a.ip());
        bail!(
            "{host} now resolves to {}, a {got} address, but apisip.toml records it as \
             {want}, so apisip does not connect: if the change is intended, change \
             the \"{host}\" entry of address_class under [source] to \"{got}\", \
             keeping the other entries, or re-run `apisip init`",
            a.ip()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_addresses() {
        for (ip, want) in [
            ("8.8.8.8", AddressClass::Public),
            ("100.63.255.255", AddressClass::Public),
            ("100.64.0.1", AddressClass::Private),
            ("100.127.255.255", AddressClass::Private),
            ("100.128.0.0", AddressClass::Public),
            ("10.1.2.3", AddressClass::Private),
            ("172.16.0.1", AddressClass::Private),
            ("192.168.1.1", AddressClass::Private),
            ("127.0.0.1", AddressClass::Loopback),
            ("0.0.0.0", AddressClass::Loopback),
            ("169.254.169.254", AddressClass::LinkLocal),
            ("2606:4700::1111", AddressClass::Public),
            ("::1", AddressClass::Loopback),
            ("::", AddressClass::Loopback),
            ("fe80::1", AddressClass::LinkLocal),
            ("fd00:ec2::254", AddressClass::Private),
            ("fc00::1", AddressClass::Private),
            ("::ffff:169.254.169.254", AddressClass::LinkLocal),
            ("::ffff:8.8.8.8", AddressClass::Public),
            ("64:ff9b::a00:5", AddressClass::Private),
            ("64:ff9b::808:808", AddressClass::Public),
            ("fec0::1", AddressClass::Private),
        ] {
            assert_eq!(classify(ip.parse().unwrap()), want, "{ip}");
        }
    }

    fn source(base_url: &str, token_url: Option<&str>) -> Source {
        let mut c = crate::config::Config::parse(&format!(
            "[source]\nspec='a'\nbase_url='{base_url}'\n[destination]\n"
        ))
        .unwrap();
        c.source.auth = token_url.map(|t| AuthConfig::Oauth2ClientCredentials {
            scheme: None,
            token_url: t.into(),
            client_id: "id".into(),
            client_secret: "secret".into(),
            scopes: vec![],
        });
        c.source
    }

    #[test]
    fn host_names_skip_ip_literals_variables_and_duplicates() {
        unsafe { std::env::set_var("APISIP_ADDR_TEST_HOST", "secret-tenant") };
        let names = |b, t| host_names(&source(b, t)).unwrap();
        assert_eq!(
            names("https://api.example.test/v1", None),
            ["api.example.test"]
        );
        assert_eq!(
            names(
                "https://api.example.test",
                Some("https://auth.example.test/token")
            ),
            ["api.example.test", "auth.example.test"]
        );
        assert_eq!(
            names(
                "https://api.example.test",
                Some("/oauth/token?k=${APISIP_ADDR_TEST_HOST}")
            ),
            ["api.example.test"]
        );
        unsafe { std::env::set_var("APISIP_ADDR_TEST_URL", "https://tenant.example.test/token") };
        assert_eq!(
            names("https://api.example.test", Some("${APISIP_ADDR_TEST_URL}")),
            ["api.example.test"]
        );
        assert!(names("http://127.0.0.1:8080", None).is_empty());
        assert!(names("http://[::1]:8080", Some("http://10.0.0.1/token")).is_empty());
        assert!(names("https://${APISIP_ADDR_TEST_HOST}.example.test", None).is_empty());
        assert_eq!(
            names(
                "https://api.example.test",
                Some("https://${APISIP_ADDR_TEST_HOST}.example.test/token")
            ),
            ["api.example.test"]
        );
    }

    #[test]
    fn refuses_an_address_of_another_class() {
        let addrs: Vec<SocketAddr> = vec![
            "93.184.216.34:0".parse().unwrap(),
            "169.254.169.254:0".parse().unwrap(),
        ];
        check("api.example.test", AddressClass::Public, &addrs[..1]).unwrap();
        let err = check("api.example.test", AddressClass::Public, &addrs)
            .unwrap_err()
            .to_string();
        for part in [
            "api.example.test",
            "169.254.169.254",
            "a link-local address",
            "records it as public",
            "the \"api.example.test\" entry of address_class under [source] to \"link-local\"",
        ] {
            assert!(err.contains(part), "{part}: {err}");
        }
        assert_eq!(class_of(&addrs), None);
        assert_eq!(class_of(&addrs[1..]), Some(AddressClass::LinkLocal));
    }

    #[test]
    fn records_and_pins_localhost() {
        let mut s = source("http://localhost:1/v1", None);
        assert_eq!(
            record(&s),
            BTreeMap::from([("localhost".to_string(), AddressClass::Loopback)])
        );
        s.address_class = record(&s);
        let pins = pinned(&s).unwrap();
        assert_eq!(pins[0].0, "localhost");
        assert!(pins[0].1.iter().all(|a| a.ip().is_loopback()));
        s.address_class
            .insert("localhost".into(), AddressClass::Public);
        assert!(pinned(&s).is_err());
        s.address_class = BTreeMap::from([("LocalHost".to_string(), AddressClass::Public)]);
        assert!(
            pinned(&s).is_err(),
            "a key in another case is still checked"
        );
    }

    #[test]
    fn an_unset_token_url_variable_still_records_the_base_url_host() {
        let s = source(
            "http://localhost:1/v1",
            Some("${APISIP_ADDR_TEST_NEVER_SET}"),
        );
        assert_eq!(
            record(&s),
            BTreeMap::from([("localhost".to_string(), AddressClass::Loopback)])
        );
        assert_eq!(host_names(&s).unwrap(), ["localhost"]);
    }

    #[test]
    fn hosts_are_read_as_written_and_an_unreadable_url_is_named() {
        let unset = "APISIP_ADDR_TEST_NEVER_SET";
        let read = |b: &str, t: Option<&str>| {
            let b = b.replace("UNSET", unset);
            let t = t.map(|t| t.replace("UNSET", unset));
            hosts_as_written(&source(&b, t.as_deref()))
        };
        assert_eq!(
            read(
                "https://api.example.test/${UNSET}",
                Some("https://auth.example.test/token?k=${UNSET}")
            ),
            (
                vec!["api.example.test".to_string(), "auth.example.test".into()],
                vec![]
            )
        );
        // Relative, or written with a variable: nothing to read.
        assert_eq!(
            read("https://api.example.test", Some("/oauth/${UNSET}")),
            (vec!["api.example.test".to_string()], vec![])
        );
        assert!(read("https://${UNSET}/v1", Some("${UNSET}")).0.is_empty());
        let (hosts, unreadable) = read(
            "${UNSET}://api.example.test/v1",
            Some("https://auth.example.test/token"),
        );
        assert_eq!(hosts, ["auth.example.test"]);
        assert_eq!(unreadable.len(), 1);
        assert_eq!(unreadable[0].0, "base_url");
        assert!(unreadable[0].1.contains(unset), "{unreadable:?}");
        let (hosts, unreadable) = read(
            "https://api.example.test/${UNSET}",
            Some("${UNSET}://auth.example.test/token"),
        );
        assert_eq!(
            (hosts, unreadable[0].0),
            (vec!["api.example.test".to_string()], "token_url")
        );
        // One entry for a host both URLs name.
        assert_eq!(
            read(
                "https://api.example.test/${UNSET}",
                Some("https://API.example.test/token")
            )
            .0,
            ["api.example.test"]
        );
    }
}
