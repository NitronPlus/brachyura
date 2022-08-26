use anyhow::{Error, Result};
use axum::{
    extract::Extension,
    http::{uri::Uri, Request, Response},
    routing::get,
    Router,
};
use axum_server::tls_rustls::RustlsConfig;
use env_logger::Env;
use hyper::client::HttpConnector;
use hyper::http::{header, header::HeaderName, HeaderValue};
use hyper::{Body, Method, StatusCode, Version};
use log::{debug, info};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::env;
use std::net::{SocketAddr, SocketAddrV4, ToSocketAddrs};
use std::sync::Arc;

type Client = hyper::client::Client<HttpConnector, Body>;

#[allow(clippy::declare_interior_mutable_const)]
const HOP_BY_HOP_HEADERS: [HeaderName; 8] = [
    HeaderName::from_static("keep-alive"),
    header::TRANSFER_ENCODING,
    header::TE,
    header::CONNECTION,
    header::TRAILER,
    header::UPGRADE,
    header::PROXY_AUTHORIZATION,
    header::PROXY_AUTHENTICATE,
];

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Config {
    listen: SocketAddrV4,
    tls: HashMap<String, String>,
    backends: Vec<Backend>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
struct Backend {
    name: Option<String>,
    location: Option<String>,
    #[serde(flatten)]
    extras: HashMap<String, String>,
}

struct ProxyState {
    config: Config,
    client: Client,
}
impl ProxyState {
    fn new(config: Config, client: Client) -> ProxyState {
        ProxyState { config, client }
    }
}

async fn read_proxy_config_yaml(yaml_path: String) -> Result<Config, serde_yaml::Error> {
    let deserialized: Config =
        serde_yaml::from_reader(std::fs::File::open(yaml_path).expect("Unable to read config"))?;
    Ok(deserialized)
}

async fn adjust_proxied_headers(req: &mut Request<Body>) -> Result<(), Error> {
    // Adjust headers for a request which is being proxied downstream

    // Remove hop by hop headers
    for h in HOP_BY_HOP_HEADERS {
        req.headers_mut().remove(h);
    }

    // Append a no-proxy header to avoid loops
    req.headers_mut()
        .insert("x-no-proxy", HeaderValue::from_static("true"));

    Ok(())
}

fn host_header_match_proxy_address(
    host_header: String,
    proxy_listener: SocketAddrV4,
) -> Result<bool, Error> {
    // This function checks whether the host header matches the proxy_listen address
    // Used for checking whether the host header was not set for HTTP1 requests
    // as it defaults to the request destination address

    let host_header_to_socket = host_header.to_socket_addrs();
    let host_header_ip_port = host_header_to_socket?
        .next()
        .expect("Unable to parse header as socket address");

    let listen_address = SocketAddr::from(proxy_listener);
    Ok(host_header_ip_port == listen_address)
}

async fn proxy_handler(
    Extension(state): Extension<Arc<ProxyState>>,
    mut req: Request<Body>,
) -> Result<Response<Body>, Infallible> {
    let mut response = Response::new(Body::empty());

    debug!(
        "Request version: {:?} method: {} uri: {} headers: {:?}",
        req.version(),
        req.method(),
        req.uri(),
        req.headers()
    );

    // Currently only testing HTTP1 support
    match req.version() {
        Version::HTTP_10 | Version::HTTP_11 => {}
        _ => {
            *response.body_mut() =
                Body::from(format!("Unsupported HTTP version: {:?}", req.version()));
            *response.status_mut() = StatusCode::BAD_REQUEST;
            return Ok(response);
        }
    }

    let headers = req.headers();
    let host_header = Some(headers.get("host")).unwrap_or(None);
    let no_proxy = headers.contains_key("x-no-proxy");
    let host_header_str = host_header
        .unwrap()
        .to_str()
        .expect("Unable to parse host header");

    let host_match_proxy_address =
        host_header_match_proxy_address(String::from(host_header_str), state.config.listen);

    match (
        req.method(),
        req.uri().path(),
        no_proxy,
        host_match_proxy_address.unwrap_or_default(),
    ) {
        // Proxy internal status endpoint
        (&Method::GET, "/status", true, true) => {
            *response.body_mut() = Body::from("The proxy is running");
        }

        // A non internal request, but the host header has not been defined
        (_, _, false, true) => {
            info!("Host header not defined");
            *response.body_mut() = Body::from("Host header not defined");
            *response.status_mut() = StatusCode::NOT_FOUND;
        }

        // Proxy the request
        _ => {
            info!("Standard request proxy");

            let mut backend_location = None;

            for backend in state.config.backends.iter().cloned() {
                if backend.name.is_some() & backend.location.is_some()
                    && *host_header_str == backend.name.unwrap()
                {
                    backend_location = backend.location;
                    break;
                }
            }
            if backend_location.is_none() {
                *response.status_mut() = StatusCode::NOT_FOUND;
            } else {
                // Proxy to backend

                // Scheme currently hardcoded to http (given this is a TLS terminating proxy)
                let scheme = "http";

                let uri = Uri::builder()
                    .scheme(scheme)
                    .authority(backend_location.unwrap())
                    .path_and_query(req.uri().path())
                    .build()
                    .expect("Unable to extract URI");

                // Simply take the existing request and mutate the uri and headers
                *req.uri_mut() = uri.clone();
                adjust_proxied_headers(&mut req)
                    .await
                    .expect("Unable to adjust headers");

                // If the backend scheme is http, adjust the original request HTTP version to 1
                // (It seems that the HTTP2 implementation requires TLS)
                if scheme == "http" {
                    *req.version_mut() = Version::HTTP_11;
                }
                response = state
                    .client
                    .request(req)
                    .await
                    .expect("Error making downstream request");
                info!(
                    "Proxied response from: {} | Status: {}",
                    uri,
                    response.status()
                );
            }
        }
    };
    debug!("Response headers: {:?}", response.headers());
    Ok(response)
}

pub async fn run_server(config_path: String) {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let config = read_proxy_config_yaml(config_path)
        .await
        .expect("Error loading yaml proxy config");

    let listen_address = SocketAddr::from(config.listen);

    let client = Client::new();

    let state = Arc::new(ProxyState::new(config, client));

    let current_dir = env::current_dir().unwrap();
    let tls_config = RustlsConfig::from_pem_file(
        current_dir.join(
            state
                .config
                .tls
                .get("cert_path")
                .expect("Unable to read cert_path"),
        ),
        current_dir.join(
            state
                .config
                .tls
                .get("key_path")
                .expect("Unable to read key_path"),
        ),
    )
    .await
    .expect("TLS config error");

    let app = Router::new()
        .route("/*path", get(proxy_handler))
        .layer(Extension(state));

    info!("Reverse proxy listening on {}", listen_address);

    axum_server::bind_rustls(listen_address, tls_config)
        .serve(app.into_make_service())
        .await
        .expect("Error starting axum server");
}

#[cfg(test)]
mod tests {
    use hyper::{
        header::{HOST, PROXY_AUTHENTICATE},
        Body, Request,
    };
    use std::net::{Ipv4Addr, SocketAddrV4};

    use crate::adjust_proxied_headers;
    use crate::host_header_match_proxy_address;
    use crate::read_proxy_config_yaml;

    #[tokio::test]
    async fn test_read_config_yaml() {
        let data = read_proxy_config_yaml("config.yaml".to_string())
            .await
            .unwrap();
        assert_eq!(
            data.backends[0].name.as_ref().unwrap(),
            &String::from("test.home")
        );
    }

    #[tokio::test]
    async fn test_adjust_proxied_headers() {
        let mut req = Request::new(Body::from("test"));
        req.headers_mut().insert(HOST, "test_host".parse().unwrap());
        req.headers_mut()
            .insert(PROXY_AUTHENTICATE, "true".parse().unwrap());
        adjust_proxied_headers(&mut req).await.unwrap();
        assert!(req.headers().iter().count() == 2);
        assert!(req.headers().contains_key(HOST));
        assert!(req.headers().contains_key("x-no-proxy"));
    }

    #[tokio::test]
    async fn test_host_header_match_proxy_address() {
        // Should match
        let listen = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 4000);
        let host_header_string = String::from("localhost:4000");
        let test_match = host_header_match_proxy_address(host_header_string, listen);
        assert_eq!(test_match.unwrap(), true);

        // Shouldn't match
        let host_header_string = String::from("localhost:4000");
        let listen = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 2), 4000);
        let test_match = host_header_match_proxy_address(host_header_string, listen);
        assert_eq!(test_match.unwrap(), false);

        // Failure case
        let host_header_string = String::from("test.home");
        let listen = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 2), 4000);
        let test_match = host_header_match_proxy_address(host_header_string, listen);
        assert_eq!(test_match.unwrap_or_default(), false);
    }
}
