use std::net::IpAddr;

use axum::http::{HeaderMap, StatusCode, header};
use ipnet::IpNet;

use crate::{
    error::{AppError, AppResult},
    state::AppState,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestScheme {
    Http,
    Https,
}

impl RequestScheme {
    fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }

    pub(crate) fn is_secure(self) -> bool {
        self == Self::Https
    }
}

pub(crate) fn request_scheme(
    state: &AppState,
    headers: &HeaderMap,
    peer_ip: IpAddr,
) -> AppResult<RequestScheme> {
    resolve_request_scheme(
        state.secure_cookies,
        state.trust_proxy_headers,
        &state.trusted_proxy_cidrs,
        headers,
        peer_ip,
    )
}

fn resolve_request_scheme(
    secure_by_default: bool,
    trust_proxy_headers: bool,
    trusted_proxy_cidrs: &[IpNet],
    headers: &HeaderMap,
    peer_ip: IpAddr,
) -> AppResult<RequestScheme> {
    let fallback = if secure_by_default {
        RequestScheme::Https
    } else {
        RequestScheme::Http
    };
    if !trust_proxy_headers
        || !trusted_proxy_cidrs
            .iter()
            .any(|network| network.contains(&peer_ip))
    {
        return Ok(fallback);
    }

    let mut values = headers.get_all("x-forwarded-proto").iter();
    let Some(value) = values.next() else {
        return Ok(fallback);
    };
    if values.next().is_some() {
        return Err(AppError::bad_request("X-Forwarded-Proto 无效"));
    }
    let value = value
        .to_str()
        .map_err(|_| AppError::bad_request("X-Forwarded-Proto 无效"))?
        .trim();
    if value.eq_ignore_ascii_case("http") {
        Ok(RequestScheme::Http)
    } else if value.eq_ignore_ascii_case("https") {
        Ok(RequestScheme::Https)
    } else {
        Err(AppError::bad_request("X-Forwarded-Proto 无效"))
    }
}

pub(crate) fn ensure_same_origin(
    headers: &HeaderMap,
    request_scheme: RequestScheme,
) -> AppResult<()> {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::new(StatusCode::FORBIDDEN, "缺少 Origin 请求头"))?;
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::new(StatusCode::FORBIDDEN, "缺少 Host 请求头"))?;
    let uri = origin
        .parse::<axum::http::Uri>()
        .map_err(|_| AppError::new(StatusCode::FORBIDDEN, "Origin 无效"))?;
    let valid_scheme = uri.scheme_str() == Some(request_scheme.as_str());
    let origin_host = uri.authority().map(|authority| authority.as_str());
    if !valid_scheme || !origin_host.is_some_and(|value| value.eq_ignore_ascii_case(host)) {
        return Err(AppError::new(StatusCode::FORBIDDEN, "拒绝跨站请求"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin_headers(host: &str, origin: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, host.parse().expect("valid Host header"));
        headers.insert(header::ORIGIN, origin.parse().expect("valid Origin header"));
        headers
    }

    #[test]
    fn trusted_proxy_protocol_selects_http_or_https_per_request() {
        let peer_ip = "172.30.135.3".parse().unwrap();
        let trusted = ["172.30.135.3/32".parse().unwrap()];
        let mut headers = HeaderMap::new();

        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        assert_eq!(
            resolve_request_scheme(false, true, &trusted, &headers, peer_ip).unwrap(),
            RequestScheme::Https
        );

        headers.insert("x-forwarded-proto", "http".parse().unwrap());
        assert_eq!(
            resolve_request_scheme(true, true, &trusted, &headers, peer_ip).unwrap(),
            RequestScheme::Http
        );
    }

    #[test]
    fn untrusted_forwarded_protocol_cannot_override_legacy_default() {
        let trusted = ["172.30.135.3/32".parse().unwrap()];
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", "https".parse().unwrap());

        assert_eq!(
            resolve_request_scheme(
                false,
                true,
                &trusted,
                &headers,
                "192.0.2.8".parse().unwrap(),
            )
            .unwrap(),
            RequestScheme::Http
        );
    }

    #[test]
    fn trusted_proxy_rejects_ambiguous_forwarded_protocol() {
        let trusted = ["172.30.135.3/32".parse().unwrap()];
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", "https,http".parse().unwrap());

        assert!(
            resolve_request_scheme(
                false,
                true,
                &trusted,
                &headers,
                "172.30.135.3".parse().unwrap(),
            )
            .is_err()
        );

        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        headers.append("x-forwarded-proto", "http".parse().unwrap());
        assert!(
            resolve_request_scheme(
                false,
                true,
                &trusted,
                &headers,
                "172.30.135.3".parse().unwrap(),
            )
            .is_err()
        );
    }

    #[test]
    fn same_origin_requires_request_scheme_and_host() {
        let secure = origin_headers(
            "console.example.com:8443",
            "https://console.example.com:8443",
        );
        assert!(ensure_same_origin(&secure, RequestScheme::Https).is_ok());
        assert!(ensure_same_origin(&secure, RequestScheme::Http).is_err());

        let insecure = origin_headers(
            "console.example.com:8080",
            "http://console.example.com:8080",
        );
        assert!(ensure_same_origin(&insecure, RequestScheme::Http).is_ok());

        let cross_site = origin_headers("console.example.com", "https://evil.example");
        assert!(ensure_same_origin(&cross_site, RequestScheme::Https).is_err());
    }
}
