//! Redirect handling for the credential-bearing HTTP client.
//!
//! The async query API attaches the API key as an `X-API-Key` header. When a redirect leaves
//! the origin, `reqwest` strips only the standard credential headers — `Authorization`,
//! `Cookie`, `cookie2`, `Proxy-Authorization` and `WWW-Authenticate` — so a custom header such
//! as `X-API-Key` rides along to whatever the `Location` names. A runtime, proxy or ingress
//! answering with an off-origin `Location` would therefore be handed the key.
//!
//! See <https://github.com/spiceai/spiceai/issues/12502>.

/// How many same-origin redirects to follow before giving up.
///
/// `reqwest::redirect::Policy::custom` does not apply the default hop limit — its own docs
/// note the closure has to handle loops itself — so the bound is enforced here. It is
/// compared the way `Policy::limited` compares it: `previous()` includes the originating URL,
/// so the bound is exclusive and this matches `reqwest`'s default depth.
const MAX_REDIRECTS: usize = 10;

/// Whether two URLs share an origin: scheme, host and effective port.
///
/// `port_or_known_default` is what makes `http://host` and `http://host:80` one origin; `Url`
/// has already lowercased the host by the time it gets here.
fn is_same_origin(previous: &reqwest::Url, next: &reqwest::Url) -> bool {
    previous.scheme() == next.scheme()
        && previous.host_str() == next.host_str()
        && previous.port_or_known_default() == next.port_or_known_default()
}

/// A redirect policy that follows same-origin redirects and refuses to leave the origin.
///
/// Stopping rather than erroring hands the 3xx back as a response, which the callers in
/// [`crate::query`] already report with its status code, so a refused redirect stays a
/// diagnosable condition instead of an opaque transport failure.
fn same_origin_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        // Decided up front so every borrow of `attempt` ends before it is consumed below.
        // No previous hop means nothing proves this stays on origin, hence `is_none_or`.
        let leaves_origin = attempt
            .previous()
            .last()
            .is_none_or(|previous| !is_same_origin(previous, attempt.url()));
        let too_many_hops = attempt.previous().len() > MAX_REDIRECTS;

        if leaves_origin || too_many_hops {
            return attempt.stop();
        }

        attempt.follow()
    })
}

/// A `reqwest` client builder that will not carry a credential off its origin.
///
/// Every HTTP client in this crate is built from here, so the policy cannot be set on one
/// construction path and missed on another.
#[must_use]
pub(crate) fn credentialed_client_builder() -> reqwest::ClientBuilder {
    let policy = same_origin_redirect_policy();
    reqwest::Client::builder().redirect(policy)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(value: &str) -> reqwest::Url {
        reqwest::Url::parse(value).expect("test URL should parse")
    }

    #[test]
    fn test_is_same_origin_matches_scheme_host_and_effective_port() {
        let base = url("http://host:8090/v1/queries");
        let sibling = url("http://host:8090/v1/queries/1");
        assert!(is_same_origin(&base, &sibling));

        // An implicit default port is the same origin as the explicit one.
        let implicit_http = url("http://host/a");
        let explicit_http = url("http://host:80/b");
        assert!(is_same_origin(&implicit_http, &explicit_http));

        let implicit_https = url("https://host/a");
        let explicit_https = url("https://host:443/b");
        assert!(is_same_origin(&implicit_https, &explicit_https));
    }

    #[test]
    fn test_is_same_origin_rejects_a_different_origin() {
        let base = url("https://runtime.example.com/v1/queries");

        // A different host is a different origin.
        let other_host = url("https://attacker.example.com/v1/queries");
        assert!(!is_same_origin(&base, &other_host));

        // So is a different port on the same host.
        let other_port = url("https://runtime.example.com:8443/v1/queries");
        assert!(!is_same_origin(&base, &other_port));
    }

    /// The case `reqwest`'s own stripping would miss: it compares host and effective port but
    /// never scheme, so a downgrade that keeps the port is not "cross-host" to it — and a
    /// plaintext hop is exactly where a credential must not go.
    #[test]
    fn test_is_same_origin_rejects_a_scheme_downgrade_on_the_same_port() {
        let secure = url("https://runtime.example.com:8443/v1/queries");
        let plaintext = url("http://runtime.example.com:8443/v1/queries");
        assert!(!is_same_origin(&secure, &plaintext));
    }

    #[test]
    fn test_is_same_origin_ignores_path_query_and_fragment() {
        let with_query = url("http://host:8090/v1/queries?a=1#x");
        let other_path = url("http://host:8090/other?b=2#y");
        assert!(is_same_origin(&with_query, &other_path));
    }
}
