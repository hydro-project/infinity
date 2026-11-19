//! Names and addresses.
//!
//! Every mailbox (one per daemon) has a host name, and every inbox on it has
//! an inbox name. An address is `host/inbox`, or a bare `inbox` for an inbox
//! on the local host. Host names are relative to the viewer: a mailbox calls
//! its SSH peers by their `remotes.json` names and its inbound peers by the
//! name they announce, the same convention the daemon uses for remote
//! session IDs.

/// Maximum length of a host or inbox name.
const MAX_NAME_LEN: usize = 64;

/// Check that `name` is a valid host or inbox name: 1–64 ASCII letters,
/// digits, `-`, `_`, or `.`, not starting with `.`.
pub fn validate_name(kind: &str, name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err(format!(
            "invalid {kind} name {name:?}: must be 1-{MAX_NAME_LEN} characters"
        ));
    }
    if name.starts_with('.') {
        return Err(format!(
            "invalid {kind} name {name:?}: must not start with '.'"
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(format!(
            "invalid {kind} name {name:?}: only ASCII letters, digits, '-', '_' and '.' are allowed"
        ));
    }
    Ok(())
}

/// A parsed `host/inbox` (or bare `inbox`) address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    /// `None` for a bare inbox name (the local host).
    pub host: Option<String>,
    pub inbox: String,
}

impl Address {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (host, inbox) = match s.split_once('/') {
            Some((host, inbox)) => {
                validate_name("host", host)?;
                (Some(host.to_owned()), inbox)
            }
            None => (None, s),
        };
        validate_name("inbox", inbox)?;
        Ok(Self {
            host,
            inbox: inbox.to_owned(),
        })
    }
}

/// Derive a host name from the machine's hostname: the first DNS label,
/// lowercased, with invalid characters replaced by `-`.
pub fn default_host_name() -> String {
    let raw = nix::unistd::gethostname()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_default();
    let label = raw.split('.').next().unwrap_or_default();
    let mut name: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(MAX_NAME_LEN)
        .collect();
    if name.is_empty() {
        name = "localhost".to_owned();
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_qualified_and_bare_addresses() {
        assert_eq!(
            Address::parse("cloud-desktop/reviewer").expect("valid address"),
            Address {
                host: Some("cloud-desktop".to_owned()),
                inbox: "reviewer".to_owned()
            }
        );
        assert_eq!(
            Address::parse(" reviewer ").expect("valid address"),
            Address {
                host: None,
                inbox: "reviewer".to_owned()
            }
        );
    }

    #[test]
    fn rejects_malformed_addresses() {
        for bad in [
            "",
            "/x",
            "x/",
            "a/b/c",
            "a b",
            "../x",
            "x/.hidden",
            "host/in\nbox",
        ] {
            assert!(Address::parse(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(Address::parse(&"a".repeat(65)).is_err());
    }

    #[test]
    fn default_host_name_is_valid() {
        validate_name("host", &default_host_name()).expect("derived host name must be valid");
    }
}
