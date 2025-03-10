use std::borrow::Cow;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::str;
use std::task::{Context, Poll};

use futures_core::Stream;
use futures_util::future::BoxFuture;
use futures_util::{future, ready, stream};
use http::{Response, Uri};
use hyper::{
    body::{self, Body, Buf},
    client::{Builder, Client},
};
use pin_project_lite::pin_project;
use thiserror::Error;

#[cfg(feature = "tokio-http-resolver")]
use hyper::client::connect::{HttpConnector, HttpInfo};

#[cfg(feature = "tokio-http-resolver")]
type GaiResolver = hyper_system_resolver::system::Resolver;

use crate::{Resolutions, Version};

///////////////////////////////////////////////////////////////////////////////
// Hardcoded resolvers

/// All builtin HTTP resolvers.
pub const ALL: &dyn crate::Resolver<'static> = &&[
    #[cfg(feature = "ipify-org")]
    HTTP_IPIFY_ORG,
    #[cfg(feature = "https-ipify-org")]
    HTTPS_IPIFY_ORG,
    #[cfg(feature = "whatismyipaddress-com")]
    HTTP_WHATISMYIPADDRESS_COM,
];

/// `http://api.ipify.org` HTTP resolver options
#[cfg(feature = "ipify-org")]
#[cfg_attr(docsrs, doc(cfg(feature = "ipify-org")))]
pub const HTTP_IPIFY_ORG: &dyn crate::Resolver<'static> =
    &Resolver::new_static("http://api.ipify.org", ExtractMethod::PlainText);

/// `https://api.ipify.org` HTTPS resolver options
#[cfg(feature = "https-ipify-org")]
#[cfg_attr(docsrs, doc(cfg(feature = "https-ipify-org")))]
pub const HTTPS_IPIFY_ORG: &dyn crate::Resolver<'static> =
    &Resolver::new_static("https://api.ipify.org", ExtractMethod::PlainText);

/// `http://bot.whatismyipaddress.com` HTTP resolver options
#[cfg(feature = "whatismyipaddress-com")]
#[cfg_attr(docsrs, doc(cfg(feature = "whatismyipaddress-com")))]
pub const HTTP_WHATISMYIPADDRESS_COM: &dyn crate::Resolver<'static> =
    &Resolver::new_static("http://bot.whatismyipaddress.com", ExtractMethod::PlainText);

///////////////////////////////////////////////////////////////////////////////
// Error

/// HTTP resolver error
#[derive(Debug, Error)]
pub enum Error {
    /// Client error.
    #[error("{0}")]
    Client(hyper::Error),
    /// URI parsing error.
    #[error("{0}")]
    Uri(http::uri::InvalidUri),
}

///////////////////////////////////////////////////////////////////////////////
// Details & options

/// A resolution produced from a HTTP resolver
#[derive(Debug, Clone)]
pub struct Details {
    uri: Uri,
    server: SocketAddr,
    method: ExtractMethod,
}

impl Details {
    /// URI used in the resolution of the associated IP address
    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    /// HTTP server used in the resolution of our IP address.
    pub fn server(&self) -> SocketAddr {
        self.server
    }

    /// The extract method used in the resolution of the associated IP address
    pub fn extract_method(&self) -> ExtractMethod {
        self.method
    }
}

/// Method used to extract an IP address from a http response
#[derive(Debug, Clone, Copy)]
pub enum ExtractMethod {
    /// Parses the body with whitespace trimmed as the IP address.
    PlainText,
    /// Parses the body with double quotes and whitespace trimmed as the IP address.
    StripDoubleQuotes,
    /// Parses the value of the JSON property `"ip"` within the body as the IP address.
    ///
    /// Note this method does not validate the JSON.
    ExtractJsonIpField,
}

///////////////////////////////////////////////////////////////////////////////
// Resolver

/// Options to build a HTTP resolver
#[derive(Debug, Clone)]
pub struct Resolver<'r> {
    uri: Cow<'r, str>,
    method: ExtractMethod,
}

impl<'r> Resolver<'r> {
    /// Create new HTTP resolver options
    pub fn new<U>(uri: U, method: ExtractMethod) -> Self
    where
        U: Into<Cow<'r, str>>,
    {
        Self {
            uri: uri.into(),
            method,
        }
    }
}

impl Resolver<'static> {
    /// Create new HTTP resolver options from static
    #[must_use]
    pub const fn new_static(uri: &'static str, method: ExtractMethod) -> Self {
        Self {
            uri: Cow::Borrowed(uri),
            method,
        }
    }
}

///////////////////////////////////////////////////////////////////////////////
// Resolutions

pin_project! {
    #[project = HttpResolutionsProj]
    enum HttpResolutions<'r> {
        HttpRequest {
            #[pin]
            response: BoxFuture<'r, Result<(IpAddr, crate::Details), crate::Error>>,
        },
        Done,
    }
}

impl<'r> Stream for HttpResolutions<'r> {
    type Item = Result<(IpAddr, crate::Details), crate::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.as_mut().project() {
            HttpResolutionsProj::HttpRequest { response } => {
                let response = ready!(response.poll(cx));
                *self = HttpResolutions::Done;
                Poll::Ready(Some(response))
            }
            HttpResolutionsProj::Done => Poll::Ready(None),
        }
    }
}

async fn resolve(
    version: Version,
    uri: Uri,
    method: ExtractMethod,
) -> Result<(IpAddr, crate::Details), crate::Error> {
    let response = http_client(version)
        .get(uri.clone())
        .await
        .map_err(Error::Client)?;
    // TODO
    let server = remote_addr(&response);
    let mut body = body::aggregate(response.into_body())
        .await
        .map_err(Error::Client)?;
    let body = body.copy_to_bytes(body.remaining());
    let body_str = str::from_utf8(body.as_ref())?;
    let address_str = match method {
        ExtractMethod::PlainText => body_str.trim(),
        ExtractMethod::ExtractJsonIpField => extract_json_ip_field(body_str)?,
        ExtractMethod::StripDoubleQuotes => body_str.trim().trim_matches('"'),
    };
    let address = address_str.parse()?;
    let details = Box::new(Details {
        uri,
        method,
        server,
    });
    Ok((address, crate::Details::from(details)))
}

impl<'r> crate::Resolver<'r> for Resolver<'r> {
    fn resolve(&self, version: Version) -> Resolutions<'r> {
        let method = self.method;
        let uri: Uri = match self.uri.as_ref().parse() {
            Ok(name) => name,
            Err(err) => return Box::pin(stream::once(future::ready(Err(crate::Error::new(err))))),
        };
        Box::pin(HttpResolutions::HttpRequest {
            response: Box::pin(resolve(version, uri, method)),
        })
    }
}

fn extract_json_ip_field(s: &str) -> Result<&str, crate::Error> {
    s.splitn(2, r#""ip":"#)
        .nth(1)
        .and_then(|after_prop| after_prop.split('"').nth(1))
        .ok_or(crate::Error::Addr)
}

///////////////////////////////////////////////////////////////////////////////
// Client

#[cfg(feature = "tokio-http-resolver")]
fn http_client(version: Version) -> Client<HttpConnector<GaiResolver>, Body> {
    use dns_lookup::{AddrFamily, AddrInfoHints, SockType};
    use hyper_system_resolver::system::System;
    let hints = match version {
        Version::V4 => AddrInfoHints {
            address: AddrFamily::Inet.into(),
            ..AddrInfoHints::default()
        },
        Version::V6 => AddrInfoHints {
            address: AddrFamily::Inet6.into(),
            ..AddrInfoHints::default()
        },
        Version::Any => AddrInfoHints {
            socktype: SockType::Stream.into(),
            ..AddrInfoHints::default()
        },
    };
    let system = System {
        addr_info_hints: Some(hints),
        service: None,
    };
    let connector = HttpConnector::new_with_resolver(system.resolver());
    Builder::default().build(connector)
}

#[cfg(feature = "tokio-http-resolver")]
fn remote_addr(response: &Response<Body>) -> SocketAddr {
    response
        .extensions()
        .get::<HttpInfo>()
        .unwrap()
        .remote_addr()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_json_ip_field() {
        const VALID: &str = r#"{
            "ip": "123.123.123.123",
        }"#;

        const INVALID: &str = r#"{
            "ipp": "123.123.123.123",
        }"#;

        const VALID_INVALID: &str = r#"{
            "ip": "123.123.123.123",
            "ip": "321.321.321.321",
        }"#;

        assert_eq!(extract_json_ip_field(VALID).unwrap(), "123.123.123.123");
        assert_eq!(
            extract_json_ip_field(VALID_INVALID).unwrap(),
            "123.123.123.123"
        );
        assert!(matches!(
            extract_json_ip_field(INVALID).unwrap_err(),
            crate::Error::Addr
        ));
    }
}
