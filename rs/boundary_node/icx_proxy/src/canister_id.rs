use anyhow::Context;
use async_trait::async_trait;
use axum::extract::FromRequestParts;
use hyper::{header::HOST, http::request::Parts};
use ic_agent::export::Principal;
use itertools::iproduct;
use tracing::error;

use crate::{
    canister_alias::CanisterAlias, config::dns_canister_config::DnsCanisterConfig, proxy::AppState,
    InspectErr,
};

pub struct ResolverState {
    pub dns: DnsCanisterConfig,
    pub check_params: bool,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct QueryParam(pub Principal);

#[async_trait]
impl<V: Sync + Send, C: Sync + Send> FromRequestParts<AppState<V, C>> for QueryParam {
    type Rejection = &'static str;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState<V, C>,
    ) -> Result<Self, Self::Rejection> {
        FromRequestParts::from_request_parts(parts, state.resolver()).await
    }
}

#[async_trait]
impl FromRequestParts<ResolverState> for QueryParam {
    type Rejection = &'static str;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ResolverState,
    ) -> Result<Self, Self::Rejection> {
        const DISABLED: &str = "'canisterId' query parameter disabled by server";
        const NO_PARAM: &str = "'canisterId' query parameter not found";
        const BAD_PARAM: &str = "'canisterId' failed to parse: Invalid Principal";

        if !state.check_params {
            return Err(DISABLED);
        }

        let (_, canister_id) =
            form_urlencoded::parse(parts.uri.query().ok_or(NO_PARAM)?.as_bytes())
                .find(|(name, _)| name == "canisterId")
                .ok_or(NO_PARAM)?;

        Principal::from_text(canister_id.as_ref())
            .map(QueryParam)
            .map_err(|_| BAD_PARAM)
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct UriHost(pub Principal);

#[async_trait]
impl<V: Sync + Send, C: Sync + Send> FromRequestParts<AppState<V, C>> for UriHost {
    type Rejection = &'static str;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState<V, C>,
    ) -> Result<Self, Self::Rejection> {
        FromRequestParts::from_request_parts(parts, state.resolver()).await
    }
}

#[async_trait]
impl FromRequestParts<ResolverState> for UriHost {
    type Rejection = &'static str;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ResolverState,
    ) -> Result<Self, Self::Rejection> {
        const NO_HOST: &str = "No host in URI";
        const BAD_HOST: &str = "URI Host did not contain a canister id or alias";

        let host = parts.uri.host().ok_or(NO_HOST)?;
        state
            .dns
            .resolve_canister_id(host)
            .map(UriHost)
            .ok_or(BAD_HOST)
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct HostHeader(pub Principal);

#[async_trait]
impl<V: Sync + Send, C: Sync + Send> FromRequestParts<AppState<V, C>> for HostHeader {
    type Rejection = &'static str;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState<V, C>,
    ) -> Result<Self, Self::Rejection> {
        FromRequestParts::from_request_parts(parts, state.resolver()).await
    }
}

#[async_trait]
impl FromRequestParts<ResolverState> for HostHeader {
    type Rejection = &'static str;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ResolverState,
    ) -> Result<Self, Self::Rejection> {
        const NO_HOST: &str = "No host in headers";
        const BAD_HOST: &str = "Host header did not contain a canister id or alias";

        let host = parts.headers.get(HOST).ok_or(NO_HOST)?;
        let host = host.to_str().map_err(|_| BAD_HOST)?;
        // Remove the port
        let host = host
            .rsplit_once(':')
            .map(|(host, _port)| host)
            .unwrap_or(host);
        state
            .dns
            .resolve_canister_id(host)
            .map(HostHeader)
            .ok_or(BAD_HOST)
    }
}

/// The options for the canister resolver
pub struct CanisterIdOpts {
    /// A list of mappings from canister names to canister principals.
    pub canister_alias: Vec<CanisterAlias>,

    /// A list of domains that can be served. These are used for canister resolution.
    pub domain: Vec<String>,

    /// Whether or not to ignore `canisterId=` when locating the canister.
    pub ignore_url_canister_param: bool,
}

pub fn setup(opts: CanisterIdOpts) -> Result<ResolverState, anyhow::Error> {
    let CanisterIdOpts {
        canister_alias,
        domain,
        ignore_url_canister_param,
    } = opts;

    let dns_suffixes = domain
        .iter()
        .flat_map(|domain| [domain.clone(), format!("raw.{domain}")]);

    let dns_aliases = iproduct!(canister_alias.iter(), domain.iter()).flat_map(
        |(CanisterAlias { id, principal }, domain)| {
            [
                format!("{id}.{domain}:{principal}"),
                format!("{id}.raw.{domain}:{principal}"),
            ]
        },
    );

    let dns = DnsCanisterConfig::new(dns_aliases, dns_suffixes)
        .context("Failed to configure canister resolver DNS")
        .inspect_err(|e| error!("{e}"))?;
    Ok(ResolverState {
        dns,
        check_params: !ignore_url_canister_param,
    })
}

#[cfg(test)]
mod tests {
    use axum::extract::FromRequestParts;
    use hyper::{header::HOST, http::request::Parts, Request};
    use ic_agent::export::Principal;
    use tokio::runtime::Runtime;

    use super::{HostHeader, QueryParam, ResolverState, UriHost};
    use crate::config::dns_canister_config::DnsCanisterConfig;

    #[test]
    fn simple_resolve() {
        let rt = Runtime::new().unwrap();
        let dns = parse_config(
            vec!["happy.little.domain.name:r7inp-6aaaa-aaaaa-aaabq-cai"],
            vec!["little.domain.name"],
        );

        let resolver = ResolverState {
            dns,
            check_params: false,
        };

        let mut req = build_req(
            Some("happy.little.domain.name"),
            "https://happy.little.domain.name/rrkah-fqaaa-aaaaa-aaaaq-cai",
        );
        assert_eq!(
            rt.block_on(HostHeader::from_request_parts(&mut req, &resolver)),
            Ok(HostHeader(principal("r7inp-6aaaa-aaaaa-aaabq-cai")))
        );
        assert_eq!(
            rt.block_on(UriHost::from_request_parts(&mut req, &resolver)),
            Ok(UriHost(principal("r7inp-6aaaa-aaaaa-aaabq-cai")))
        );
        assert!(rt
            .block_on(QueryParam::from_request_parts(&mut req, &resolver))
            .is_err());

        let mut req = build_req(
            Some("rrkah-fqaaa-aaaaa-aaaaq-cai.little.domain.name"),
            "/r7inp-6aaaa-aaaaa-aaabq-cai",
        );
        assert_eq!(
            rt.block_on(HostHeader::from_request_parts(&mut req, &resolver)),
            Ok(HostHeader(principal("rrkah-fqaaa-aaaaa-aaaaq-cai")))
        );
        assert!(rt
            .block_on(UriHost::from_request_parts(&mut req, &resolver))
            .is_err());
        assert!(rt
            .block_on(QueryParam::from_request_parts(&mut req, &resolver))
            .is_err());
    }

    #[test]
    fn prod() {
        let rt = Runtime::new().unwrap();
        let dns = parse_config(
            vec![
                "personhood.ic0.app:g3wsl-eqaaa-aaaan-aaaaa-cai",
                "personhood.raw.ic0.app:g3wsl-eqaaa-aaaan-aaaaa-cai",
                "identity.ic0.app:rdmx6-jaaaa-aaaaa-aaadq-cai",
                "identity.raw.ic0.app:rdmx6-jaaaa-aaaaa-aaadq-cai",
                "nns.ic0.app:qoctq-giaaa-aaaaa-aaaea-cai",
                "nns.raw.ic0.app:qoctq-giaaa-aaaaa-aaaea-cai",
                "dscvr.ic0.app:h5aet-waaaa-aaaab-qaamq-cai",
                "dscvr.raw.ic0.app:h5aet-waaaa-aaaab-qaamq-cai",
            ],
            vec!["raw.ic0.app", "ic0.app"],
        );

        let resolver = ResolverState {
            dns,
            check_params: false,
        };

        let mut req = build_req(Some("nns.ic0.app"), "/about");
        assert_eq!(
            rt.block_on(HostHeader::from_request_parts(&mut req, &resolver)),
            Ok(HostHeader(principal("qoctq-giaaa-aaaaa-aaaea-cai")))
        );
        assert!(rt
            .block_on(UriHost::from_request_parts(&mut req, &resolver))
            .is_err());
        assert!(rt
            .block_on(QueryParam::from_request_parts(&mut req, &resolver))
            .is_err());

        let mut req = build_req(Some("nns.ic0.app"), "https://nns.ic0.app/about");
        assert_eq!(
            rt.block_on(HostHeader::from_request_parts(&mut req, &resolver)),
            Ok(HostHeader(principal("qoctq-giaaa-aaaaa-aaaea-cai")))
        );
        assert_eq!(
            rt.block_on(UriHost::from_request_parts(&mut req, &resolver)),
            Ok(UriHost(principal("qoctq-giaaa-aaaaa-aaaea-cai")))
        );
        assert!(rt
            .block_on(QueryParam::from_request_parts(&mut req, &resolver))
            .is_err());

        let mut req = build_req(None, "https://nns.ic0.app/about");
        assert!(rt
            .block_on(HostHeader::from_request_parts(&mut req, &resolver))
            .is_err());
        assert_eq!(
            rt.block_on(UriHost::from_request_parts(&mut req, &resolver)),
            Ok(UriHost(principal("qoctq-giaaa-aaaaa-aaaea-cai")))
        );
        assert!(rt
            .block_on(QueryParam::from_request_parts(&mut req, &resolver))
            .is_err());

        let mut req = build_req(None, "https://rrkah-fqaaa-aaaaa-aaaaq-cai.ic0.app/about");
        assert!(rt
            .block_on(HostHeader::from_request_parts(&mut req, &resolver))
            .is_err());
        assert_eq!(
            rt.block_on(UriHost::from_request_parts(&mut req, &resolver)),
            Ok(UriHost(principal("rrkah-fqaaa-aaaaa-aaaaq-cai")))
        );
        assert!(rt
            .block_on(QueryParam::from_request_parts(&mut req, &resolver))
            .is_err());

        let mut req = build_req(
            Some("rrkah-fqaaa-aaaaa-aaaaq-cai.ic0.app"),
            "https://rrkah-fqaaa-aaaaa-aaaaq-cai.ic0.app/about",
        );
        assert_eq!(
            rt.block_on(HostHeader::from_request_parts(&mut req, &resolver)),
            Ok(HostHeader(principal("rrkah-fqaaa-aaaaa-aaaaq-cai")))
        );
        assert_eq!(
            rt.block_on(UriHost::from_request_parts(&mut req, &resolver)),
            Ok(UriHost(principal("rrkah-fqaaa-aaaaa-aaaaq-cai")))
        );
        assert!(rt
            .block_on(QueryParam::from_request_parts(&mut req, &resolver))
            .is_err());

        let mut req = build_req(Some("rrkah-fqaaa-aaaaa-aaaaq-cai.ic0.app"), "/about");
        assert_eq!(
            rt.block_on(HostHeader::from_request_parts(&mut req, &resolver)),
            Ok(HostHeader(principal("rrkah-fqaaa-aaaaa-aaaaq-cai")))
        );
        assert!(rt
            .block_on(UriHost::from_request_parts(&mut req, &resolver))
            .is_err());
        assert!(rt
            .block_on(QueryParam::from_request_parts(&mut req, &resolver))
            .is_err());

        let mut req = build_req(Some("rrkah-fqaaa-aaaaa-aaaaq-cai.raw.ic0.app"), "/about");
        assert_eq!(
            rt.block_on(HostHeader::from_request_parts(&mut req, &resolver)),
            Ok(HostHeader(principal("rrkah-fqaaa-aaaaa-aaaaq-cai")))
        );
        assert!(rt
            .block_on(UriHost::from_request_parts(&mut req, &resolver))
            .is_err());
        assert!(rt
            .block_on(QueryParam::from_request_parts(&mut req, &resolver))
            .is_err());

        let mut req = build_req(
            Some("rrkah-fqaaa-aaaaa-aaaaq-cai.foo.raw.ic0.app"),
            "/about",
        );
        assert!(rt
            .block_on(HostHeader::from_request_parts(&mut req, &resolver))
            .is_err());
        assert!(rt
            .block_on(UriHost::from_request_parts(&mut req, &resolver))
            .is_err());
        assert!(rt
            .block_on(QueryParam::from_request_parts(&mut req, &resolver))
            .is_err());
    }

    #[test]
    fn dfx() {
        let rt = Runtime::new().unwrap();
        let dns = parse_config(vec![], vec!["localhost"]);

        let resolver = ResolverState {
            dns,
            check_params: true,
        };

        let mut req = build_req(Some("rrkah-fqaaa-aaaaa-aaaaq-cai.localhost"), "/about");
        assert_eq!(
            rt.block_on(HostHeader::from_request_parts(&mut req, &resolver)),
            Ok(HostHeader(principal("rrkah-fqaaa-aaaaa-aaaaq-cai")))
        );
        assert!(rt
            .block_on(UriHost::from_request_parts(&mut req, &resolver))
            .is_err());
        assert!(rt
            .block_on(QueryParam::from_request_parts(&mut req, &resolver))
            .is_err());

        let mut req = build_req(
            Some("localhost"),
            "/about?canisterId=rrkah-fqaaa-aaaaa-aaaaq-cai",
        );
        assert!(rt
            .block_on(HostHeader::from_request_parts(&mut req, &resolver))
            .is_err());
        assert!(rt
            .block_on(UriHost::from_request_parts(&mut req, &resolver))
            .is_err());
        assert_eq!(
            rt.block_on(QueryParam::from_request_parts(&mut req, &resolver)),
            Ok(QueryParam(principal("rrkah-fqaaa-aaaaa-aaaaq-cai")))
        );
    }

    fn parse_config(aliases: Vec<&str>, suffixes: Vec<&str>) -> DnsCanisterConfig {
        let aliases: Vec<String> = aliases.iter().map(|&s| String::from(s)).collect();
        let suffixes: Vec<String> = suffixes.iter().map(|&s| String::from(s)).collect();
        DnsCanisterConfig::new(aliases, suffixes).unwrap()
    }

    fn build_req(host: Option<&str>, uri: &str) -> Parts {
        let req = Request::builder().uri(uri);
        if let Some(host) = host {
            req.header(HOST, host)
        } else {
            req
        }
        .body(())
        .unwrap()
        .into_parts()
        .0
    }

    fn principal(v: &str) -> Principal {
        Principal::from_text(v).unwrap()
    }
}
