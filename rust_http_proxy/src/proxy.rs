use std::{
    borrow::Cow,
    fmt::{Display, Formatter},
    io::{self, ErrorKind},
    net::SocketAddr,
    sync::LazyLock,
    time::Duration,
};

use crate::{
    address::host_addr,
    axum_handler::{self, AppProxyError, AXUM_PATHS},
    forward_proxy_client::ForwardProxyClient,
    ip_x::local_ip,
    raw_serve,
    reverse::DEFAULT_HOST,
    METRICS,
};
use {io_x::CounterIO, io_x::TimeoutIO, prom_label::LabelImpl};

use axum::extract::Request;
use axum_bootstrap::InterceptResult;
use http::{header::HOST, Uri};
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::{body::Bytes, header::HeaderValue, http, upgrade::Upgraded, Method, Response, Version};
use hyper_util::client::legacy::{self, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use hyper_util::rt::TokioIo;
use log::{debug, info, warn};
use percent_encoding::percent_decode_str;
use prometheus_client::encoding::EncodeLabelSet;
use rand::Rng;
use tokio::{net::TcpStream, pin};
static LOCAL_IP: LazyLock<String> = LazyLock::new(|| local_ip().unwrap_or("0.0.0.0".to_string()));
pub struct ProxyHandler {
    forwad_proxy_client: ForwardProxyClient<Incoming>,
    reverse_proxy_client: legacy::Client<hyper_rustls::HttpsConnector<HttpConnector>, Incoming>,
}

pub(crate) enum InterceptResultAdapter {
    Drop,
    Return(Response<BoxBody<Bytes, io::Error>>),
    Continue(Request<Incoming>),
}

impl From<InterceptResultAdapter> for InterceptResult<AppProxyError> {
    fn from(val: InterceptResultAdapter) -> Self {
        match val {
            InterceptResultAdapter::Return(resp) => {
                let (parts, body) = resp.into_parts();
                axum_bootstrap::InterceptResult::Return(Response::from_parts(parts, axum::body::Body::new(body)))
            }
            InterceptResultAdapter::Drop => InterceptResult::Drop,
            InterceptResultAdapter::Continue(req) => InterceptResult::Continue(req),
        }
    }
}

#[allow(unused)]
use hyper_rustls::HttpsConnectorBuilder;
impl ProxyHandler {
    #[allow(clippy::expect_used)]
    pub fn new() -> Result<Self, crate::DynError> {
        let reverse_client = build_hyper_legacy_client();
        let http1_client = ForwardProxyClient::<Incoming>::new();

        Ok(ProxyHandler {
            reverse_proxy_client: reverse_client,
            forwad_proxy_client: http1_client,
        })
    }
    pub async fn handle(
        &self, req: Request<hyper::body::Incoming>, client_socket_addr: SocketAddr,
    ) -> Result<InterceptResultAdapter, io::Error> {
        let config_basic_auth = &crate::CONFIG.basic_auth;
        let never_ask_for_auth = crate::CONFIG.never_ask_for_auth;

        // 对于非CONNECT请求，检查是否需要反向代理或服务
        if Method::CONNECT != req.method() {
            let (original_scheme_host_port, req_domain) = extract_scheme_host_port(
                &req,
                match crate::CONFIG.over_tls {
                    true => "https",
                    false => "http",
                },
            )?;

            // 尝试找到匹配的反向代理配置
            let location_config_of_host = crate::CONFIG
                .reverse_proxy_config
                .locations
                .get(&req_domain.0)
                .or(crate::CONFIG.reverse_proxy_config.locations.get(DEFAULT_HOST));

            if let Some(locations) = location_config_of_host {
                if let Some(location_config) = locations
                    .iter()
                    .find(|&ele| req.uri().path().starts_with(&ele.location))
                // 用请求的path和location做前缀匹配
                {
                    return location_config
                        .handle(req, client_socket_addr, &original_scheme_host_port, &self.reverse_proxy_client)
                        .await
                        .map(InterceptResultAdapter::Return);
                }
            }

            // 对于HTTP/2请求或URI中不包含host的请求，处理为普通服务请求
            if req.version() == Version::HTTP_2 || req.uri().host().is_none() {
                // 检查是否允许提供静态文件服务
                if crate::CONFIG.serving_control.prohibit_serving {
                    // 全局禁止静态文件托管
                    info!("Dropping request from {client_socket_addr} due to global prohibit_serving setting");
                    return Ok(InterceptResultAdapter::Drop);
                }

                // 检查是否有网段限制及客户端IP是否在允许的网段内
                let client_ip = client_socket_addr.ip().to_canonical();
                let allowed_networks = &crate::CONFIG.serving_control.allowed_networks;

                if !allowed_networks.is_empty() {
                    // 有网段限制，检查客户端IP是否在允许的网段内
                    let ip_allowed = allowed_networks.iter().any(|network| network.contains(client_ip));

                    if !ip_allowed {
                        info!("Dropping request from {client_ip} as it's not in allowed networks");
                        return Ok(InterceptResultAdapter::Drop);
                    }
                }

                // IP检查通过，提供静态文件服务
                match self.serve_request(&req, client_socket_addr).await {
                    Ok(res) => {
                        if res.status() == http::StatusCode::NOT_FOUND {
                            return Ok(InterceptResultAdapter::Continue(req));
                        } else {
                            return Ok(InterceptResultAdapter::Return(res));
                        }
                    }
                    Err(err) => {
                        return Err(err);
                    }
                }
            }
        }

        // 2. proxy stage
        match axum_handler::check_auth(req.headers(), http::header::PROXY_AUTHORIZATION, config_basic_auth) {
            Ok(username_option) => {
                let username = username_option.unwrap_or("unknown".to_owned());
                info!(
                    "{:>29} {:<5} {:^8} {:^7} {:?} {:?} ",
                    "https://ip.im/".to_owned() + &client_socket_addr.ip().to_canonical().to_string(),
                    client_socket_addr.port(),
                    username,
                    req.method().as_str(),
                    req.uri(),
                    req.version(),
                );
                if Method::CONNECT == req.method() {
                    self.tunnel_proxy(req, client_socket_addr, username)
                        .map(InterceptResultAdapter::Return)
                } else {
                    self.simple_proxy(req, client_socket_addr, username)
                        .await
                        .map(InterceptResultAdapter::Return)
                }
            }
            Err(e) => {
                warn!("auth check from {} error: {}", { client_socket_addr }, e);
                if never_ask_for_auth {
                    Err(io::Error::new(ErrorKind::PermissionDenied, "wrong basic auth, closing socket..."))
                } else {
                    Ok(InterceptResultAdapter::Return(build_authenticate_resp(true)))
                }
            }
        }
    }

    /// 代理普通请求
    /// HTTP/1.1 GET/POST/PUT/DELETE/HEAD
    async fn simple_proxy(
        &self, mut req: Request<Incoming>, client_socket_addr: SocketAddr, username: String,
    ) -> Result<Response<BoxBody<Bytes, io::Error>>, io::Error> {
        let access_label = build_access_label(&req, client_socket_addr, username)?;
        mod_http1_proxy_req(&mut req)?;
        match self
            .forwad_proxy_client
            .send_request(req, &access_label, |stream: TcpStream, access_label: AccessLabel| {
                CounterIO::new(stream, METRICS.proxy_traffic.clone(), LabelImpl::new(access_label))
            })
            .await
        {
            Ok(resp) => Ok(resp.map(|body| {
                body.map_err(|e| {
                    let e = e;
                    io::Error::new(ErrorKind::InvalidData, e)
                })
                .boxed()
            })),
            Err(e) => Err(e),
        }
    }

    /// 代理CONNECT请求
    /// HTTP/1.1 CONNECT    
    fn tunnel_proxy(
        &self, req: Request<Incoming>, client_socket_addr: SocketAddr, username: String,
    ) -> Result<Response<BoxBody<Bytes, io::Error>>, io::Error> {
        // Received an HTTP request like:
        // ```
        // CONNECT www.domain.com:443 HTTP/1.1
        // Host: www.domain.com:443
        // Proxy-Connection: Keep-Alive
        // ```
        //
        // When HTTP method is CONNECT we should return an empty body
        // then we can eventually upgrade the connection and talk a new protocol.
        //
        // Note: only after client received an empty body with STATUS_OK can the
        // connection be upgraded, so we can't return a response inside
        // `on_upgrade` future.
        if let Some(addr) = host_addr(req.uri()) {
            let proxy_traffic = METRICS.proxy_traffic.clone();
            tokio::task::spawn(async move {
                match hyper::upgrade::on(req).await {
                    Ok(src_upgraded) => {
                        let access_label = AccessLabel {
                            client: client_socket_addr.ip().to_canonical().to_string(),
                            target: addr.clone().to_string(),
                            username,
                        };
                        // Connect to remote server
                        match TcpStream::connect(addr.to_string()).await {
                            Ok(target_stream) => {
                                // if the DST server did not respond the FIN(shutdown) from the SRC client, then you will see a pair of FIN-WAIT-2 and CLOSE_WAIT in the proxy server
                                // which two socketAddrs are in the true path.
                                // use this command to check:
                                // netstat -ntp|grep -E "CLOSE_WAIT|FIN_WAIT"|sort
                                // The DST server should answer for this problem, becasue it ignores the FIN
                                // Dont worry, after the FIN_WAIT_2 timeout, the CLOSE_WAIT connection will close.
                                debug!(
                                    "[tunnel {}], [true path: {} -> {}]",
                                    access_label,
                                    client_socket_addr.ip().to_canonical().to_string()
                                        + ":"
                                        + &client_socket_addr.port().to_string(),
                                    target_stream
                                        .peer_addr()
                                        .map(|addr| addr.ip().to_canonical().to_string()
                                            + ":"
                                            + &addr.port().to_string())
                                        .unwrap_or("failed".to_owned())
                                );
                                let access_tag = access_label.to_string();
                                let dst_stream =
                                    CounterIO::new(target_stream, proxy_traffic, LabelImpl::new(access_label));
                                if let Err(e) = tunnel(src_upgraded, dst_stream).await {
                                    warn!("[tunnel io error] [{}]: [{}] {} ", access_tag, e.kind(), e);
                                };
                            }
                            Err(e) => {
                                warn!("[tunnel establish error] [{}]: [{}] {} ", access_label, e.kind(), e)
                            }
                        }
                    }
                    Err(e) => warn!("upgrade error: {e}"),
                }
            });
            let mut response = Response::new(empty_body());
            // 针对connect请求中，在响应中增加随机长度的padding，防止每次建连时tcp数据长度特征过于敏感
            let max_num = 2048 / LOCAL_IP.len();
            let count = rand::rng().random_range(1..max_num);
            for _ in 0..count {
                response
                    .headers_mut()
                    .append(http::header::SERVER, HeaderValue::from_static(&LOCAL_IP));
            }
            Ok(response)
        } else {
            warn!("CONNECT host is not socket addr: {:?}", req.uri());
            let mut resp = Response::new(full_body("CONNECT must be to a socket address"));
            *resp.status_mut() = http::StatusCode::BAD_REQUEST;

            Ok(resp)
        }
    }
    async fn serve_request(
        &self, req: &Request<Incoming>, client_socket_addr: SocketAddr,
    ) -> Result<Response<BoxBody<Bytes, io::Error>>, io::Error> {
        let raw_path = req.uri().path();
        let path = percent_decode_str(raw_path)
            .decode_utf8()
            .unwrap_or(Cow::from(raw_path));
        let path = path.as_ref();
        if AXUM_PATHS.contains(&path) {
            return raw_serve::not_found().map_err(|e| io::Error::new(ErrorKind::InvalidData, e));
        }
        raw_serve::serve_http_request(req, client_socket_addr, path)
            .await
            .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))
    }
}

fn mod_http1_proxy_req(req: &mut Request<Incoming>) -> io::Result<()> {
    // 删除代理特有的请求头
    req.headers_mut().remove(http::header::PROXY_AUTHORIZATION.to_string());
    req.headers_mut().remove("Proxy-Connection");
    // set host header
    let uri = req.uri().clone();
    let hostname = uri
        .host()
        .ok_or(io::Error::new(ErrorKind::InvalidData, "host is absent in HTTP/1.1"))?;
    let host_header = if let Some(port) = match (uri.port().map(|p| p.as_u16()), is_schema_secure(&uri)) {
        (Some(443), true) => None,
        (Some(80), false) => None,
        _ => uri.port(),
    } {
        let s = format!("{hostname}:{port}");
        HeaderValue::from_str(&s)
    } else {
        HeaderValue::from_str(hostname)
    }
    .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    let origin = req.headers_mut().insert(HOST, host_header.clone());
    if Some(host_header.clone()) != origin {
        info!("change host header: {origin:?} -> {host_header:?}");
    }
    // change absoulte uri to relative uri
    origin_form(req.uri_mut())?;
    Ok(())
}

fn build_access_label(
    req: &Request<Incoming>, client_socket_addr: SocketAddr, username: String,
) -> Result<AccessLabel, io::Error> {
    let addr = host_addr(req.uri())
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, format!("URI missing host: {}", req.uri())))?;
    let access_label = AccessLabel {
        client: client_socket_addr.ip().to_canonical().to_string(),
        target: addr.to_string(),
        username,
    };
    Ok(access_label)
}

pub(crate) struct SchemeHostPort {
    pub(crate) scheme: String,
    pub(crate) host: String,
    pub(crate) port: Option<u16>,
}

impl Display for SchemeHostPort {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self.port {
            Some(port) => write!(f, "{}://{}:{}", self.scheme, self.host, port),
            None => write!(f, "{}://{}", self.scheme, self.host),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct RequestDomain(String);

fn extract_scheme_host_port(
    req: &Request<Incoming>, default_scheme: &str,
) -> io::Result<(SchemeHostPort, RequestDomain)> {
    let uri = req.uri();
    let scheme = uri.scheme_str().unwrap_or(default_scheme);
    if req.version() == Version::HTTP_2 {
        //H2，信息全在uri中
        let host_in_url = uri
            .host()
            .ok_or(io::Error::new(ErrorKind::InvalidData, "authority is absent in HTTP/2"))?
            .to_string();
        let host_in_header = req
            .headers()
            .get(http::header::HOST)
            .and_then(|host| host.to_str().ok())
            .and_then(|host_str| host_str.split(':').next())
            .map(str::to_string);
        Ok((
            SchemeHostPort {
                scheme: scheme.to_owned(),
                host: host_in_url.clone(),
                port: uri.port_u16(),
            },
            RequestDomain(if let Some(host_in_header) = host_in_header {
                host_in_header
            } else {
                host_in_url
            }),
        ))
    } else {
        let mut split = req
            .headers()
            .get(http::header::HOST)
            .ok_or(io::Error::new(ErrorKind::InvalidData, "Host Header is absent in HTTP/1.1"))?
            .to_str()
            .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?
            .split(':');
        let host = split
            .next()
            .ok_or(io::Error::new(ErrorKind::InvalidData, "host not in header"))?
            .to_string();
        let port = match split.next() {
            Some(port) => Some(
                port.parse::<u16>()
                    .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?,
            ),
            None => None,
        };
        Ok((
            SchemeHostPort {
                scheme: scheme.to_owned(),
                host: host.clone(),
                port,
            },
            RequestDomain(host),
        ))
    }
}

fn is_schema_secure(uri: &Uri) -> bool {
    uri.scheme_str()
        .map(|scheme_str| matches!(scheme_str, "wss" | "https"))
        .unwrap_or_default()
}

fn build_hyper_legacy_client() -> legacy::Client<hyper_rustls::HttpsConnector<HttpConnector>, Incoming> {
    let pool_idle_timeout = Duration::from_secs(90);
    // 创建一个 HttpConnector
    let mut http_connector = HttpConnector::new();
    http_connector.enforce_http(false);
    http_connector.set_keepalive(Some(pool_idle_timeout));

    let https_connector = HttpsConnectorBuilder::new()
        .with_platform_verifier()
        .https_or_http()
        .enable_all_versions()
        .wrap_connector(http_connector);
    // 创建一个 HttpsConnector，使用 rustls 作为后端
    let client: legacy::Client<hyper_rustls::HttpsConnector<HttpConnector>, Incoming> =
        legacy::Client::builder(TokioExecutor::new())
            .pool_idle_timeout(pool_idle_timeout)
            .pool_max_idle_per_host(5)
            .pool_timer(hyper_util::rt::TokioTimer::new())
            .build(https_connector);
    client
}

fn origin_form(uri: &mut Uri) -> io::Result<()> {
    let path = match uri.path_and_query() {
        Some(path) if path.as_str() != "/" => {
            let mut parts = ::http::uri::Parts::default();
            parts.path_and_query = Some(path.clone());
            Uri::from_parts(parts).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?
        }
        _none_or_just_slash => {
            debug_assert!(Uri::default() == "/");
            Uri::default()
        }
    };
    *uri = path;
    Ok(())
}

// Create a TCP connection to host:port, build a tunnel between the connection and
// the upgraded connection
async fn tunnel(upgraded: Upgraded, target_io: CounterIO<TcpStream, LabelImpl<AccessLabel>>) -> io::Result<()> {
    let mut upgraded = TokioIo::new(upgraded);
    let timed_target_io = TimeoutIO::new(target_io, crate::IDLE_TIMEOUT);
    pin!(timed_target_io);
    // https://github.com/sfackler/tokio-io-timeout/issues/12
    // timed_target_io.as_mut() // 一定要as_mut()，否则会move所有权
    // ._set_timeout_pinned(Duration::from_secs(crate::IDLE_SECONDS));
    let (_from_client, _from_server) = tokio::io::copy_bidirectional(&mut upgraded, &mut timed_target_io).await?;
    Ok(())
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ReqLabels {
    // Use your own enum types to represent label values.
    pub referer: String,
    // Or just a plain string.
    pub path: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet, PartialOrd, Ord)]
pub struct AccessLabel {
    pub client: String,
    pub target: String,
    pub username: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet, PartialOrd, Ord)]
pub struct ReverseProxyReqLabel {
    pub client: String,
    pub origin: String,
    pub upstream: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct NetDirectionLabel {
    pub direction: &'static str,
}

impl Display for AccessLabel {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} -> {}", self.client, self.target)
    }
}

pub(crate) fn build_authenticate_resp(for_proxy: bool) -> Response<BoxBody<Bytes, io::Error>> {
    let mut resp = Response::new(full_body("auth need"));
    resp.headers_mut().append(
        if for_proxy {
            http::header::PROXY_AUTHENTICATE
        } else {
            http::header::WWW_AUTHENTICATE
        },
        HeaderValue::from_static("Basic realm=\"are you kidding me\""),
    );
    if for_proxy {
        *resp.status_mut() = http::StatusCode::PROXY_AUTHENTICATION_REQUIRED;
    } else {
        *resp.status_mut() = http::StatusCode::UNAUTHORIZED;
    }
    resp
}

pub fn empty_body() -> BoxBody<Bytes, io::Error> {
    Empty::<Bytes>::new().map_err(|never| match never {}).boxed()
}

pub fn full_body<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, io::Error> {
    Full::new(chunk.into()).map_err(|never| match never {}).boxed()
}

#[cfg(test)]
mod test {
    #[test]
    fn test_aa() {
        let host = "www.arloor.com";
        assert_eq!(host.split(':').next().unwrap_or("").to_string(), host);
    }
}
