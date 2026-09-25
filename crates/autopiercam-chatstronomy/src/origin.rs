use url::{Host, Url};

/// Canonical origin used for both endpoint construction and credential scoping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubOrigin(Url);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TransportPolicy {
    #[default]
    HttpsOnly,
    /// Explicit local development only; never permit arbitrary cleartext hosts.
    AllowLoopbackHttp,
}

#[derive(Debug, thiserror::Error)]
#[error("Hub must be an HTTPS origin without credentials, path, query, or fragment")]
pub struct InvalidOrigin;

impl HubOrigin {
    pub fn parse(input: &str, policy: TransportPolicy) -> Result<Self, InvalidOrigin> {
        if input.trim() != input || input.chars().any(char::is_control) || input.contains('\\') {
            return Err(InvalidOrigin);
        }
        // Check the raw path as well as the parsed path: URL normalization must
        // not silently turn /private/.. into an acceptable origin.
        let (_, authority) = input.split_once("://").ok_or(InvalidOrigin)?;
        if authority.contains('@')
            || authority
                .split_once('/')
                .is_some_and(|(_, path)| !path.is_empty())
        {
            return Err(InvalidOrigin);
        }
        let url = Url::parse(input).map_err(|_| InvalidOrigin)?;
        let loopback = match url.host() {
            Some(Host::Domain(name)) => name == "localhost",
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        };
        let permitted = url.scheme() == "https"
            || (url.scheme() == "http" && policy == TransportPolicy::AllowLoopbackHttp && loopback);
        if !permitted
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(InvalidOrigin);
        }
        Ok(Self(url))
    }

    pub fn canonical(&self) -> String {
        self.0.origin().ascii_serialization()
    }

    pub fn pairing_url(&self) -> Url {
        let mut url = self.0.clone();
        url.set_path("/v1/devices/pair");
        url
    }

    pub fn websocket_url(&self) -> Url {
        let mut url = self.0.clone();
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme).expect("validated HTTP(S) origin");
        url.set_path("/v1/devices");
        url
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_origin_and_endpoints() {
        let origin =
            HubOrigin::parse("https://HUB.example:443/", TransportPolicy::default()).unwrap();
        assert_eq!(origin.canonical(), "https://hub.example");
        assert_eq!(
            origin.pairing_url().as_str(),
            "https://hub.example/v1/devices/pair"
        );
        assert_eq!(
            origin.websocket_url().as_str(),
            "wss://hub.example/v1/devices"
        );
    }

    #[test]
    fn rejects_non_origins_and_secret_bearing_inputs_without_echoing_them() {
        for input in [
            "https://user:secret@hub.example",
            "https://@hub.example",
            "https://hub.example/private",
            "https://hub.example/private/..",
            "https://hub.example/?secret=token",
            "https://hub.example/#secret",
            "https://hub.example//",
            "https://hub.example\\private",
            " https://hub.example",
            "https://hub.\nexample",
            "file:///tmp",
            "wss://hub.example",
            "http://hub.example",
            "https:///hub.example",
        ] {
            let error = HubOrigin::parse(input, TransportPolicy::default()).unwrap_err();
            assert!(!error.to_string().contains("secret"));
        }
    }

    #[test]
    fn cleartext_is_explicit_and_loopback_only() {
        for input in [
            "http://localhost:8000",
            "http://127.0.0.1:8000",
            "http://[::1]:8000",
        ] {
            assert!(HubOrigin::parse(input, TransportPolicy::default()).is_err());
            let origin = HubOrigin::parse(input, TransportPolicy::AllowLoopbackHttp).unwrap();
            assert_eq!(origin.websocket_url().scheme(), "ws");
        }
        for input in [
            "http://localhost.evil.test",
            "http://192.168.1.2",
            "http://example.test",
        ] {
            assert!(HubOrigin::parse(input, TransportPolicy::AllowLoopbackHttp).is_err());
        }
    }
}
