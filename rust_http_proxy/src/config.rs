use base64::engine::general_purpose;
use base64::Engine;
use clap::Parser;
use ipnetwork::IpNetwork;
use log::{info, warn};
use log_x::init_log;
use std::collections::HashMap;
use std::str::FromStr;

use crate::reverse::{parse_reverse_proxy_config, ReverseProxyConfig};
use crate::{DynError, IDLE_TIMEOUT};

/// A HTTP proxy server based on Hyper and Rustls, which features TLS proxy and static file serving.
#[derive(Parser)]
#[command(author, version=None, about, long_about = None)]
pub struct Param {
    #[arg(long, value_name = "LOG_DIR", default_value = "/tmp")]
    log_dir: String,
    #[arg(long, value_name = "LOG_FILE", default_value = "proxy.log")]
    log_file: String,
    #[arg(
        short,
        long,
        value_name = "PORT",
        default_value = "3128",
        help = "可以多次指定来实现多端口\n"
    )]
    port: Vec<u16>,
    #[arg(short, long, value_name = "CERT", default_value = "cert.pem")]
    cert: String,
    #[arg(short, long, value_name = "KEY", default_value = "privkey.pem")]
    key: String,
    #[arg(
        short,
        long,
        value_name = "USER",
        help = "默认为空，表示不鉴权。\n\
    格式为 'username:password'\n\
    可以多次指定来实现多用户"
    )]
    users: Vec<String>,
    #[arg(
        short,
        long,
        value_name = "WEB_CONTENT_PATH",
        default_value = "/usr/share/nginx/html"
    )]
    web_content_path: String,
    #[arg(
        short,
        long,
        value_name = "REFERER",
        help = "Http Referer请求头处理 \n\
        1. 图片资源的防盗链：针对png/jpeg/jpg等文件的请求，要求Request的Referer header要么为空，要么包含配置的值\n\
        2. 外链访问监控：如果Referer不包含配置的值，并且访问html资源时，Prometheus counter req_from_out++，用于外链访问监控\n\
        可以多次指定，也可以不指定"
    )]
    referer_keywords_to_self: Vec<String>,
    #[arg(
        long,
        help = "if enable, never send '407 Proxy Authentication Required' to client。\n\
        当作为正向代理使用时建议开启，否则有被嗅探的风险。"
    )]
    never_ask_for_auth: bool,
    #[arg(long, help = "禁止所有静态文件托管，为了避免被嗅探")]
    prohibit_serving: bool,
    #[arg(
        long,
        value_name = "CIDR",
        help = "允许访问静态文件托管的网段，格式为CIDR，例如: 192.168.1.0/24, 10.0.0.0/8\n\
        可以多次指定来允许多个网段\n\
        如设置了prohibit_serving，则此参数无效\n\
        如未设置任何网段，且未设置prohibit_serving，则允许所有IP访问静态文件"
    )]
    allow_serving_network: Vec<String>,
    #[arg(short, long, help = "if enable, proxy server will listen on https")]
    over_tls: bool,
    #[arg(long, value_name = "FILE_PATH", help = r#"反向代理配置文件"#)]
    reverse_proxy_config_file: Option<String>,
    #[arg(long, help = r#"是否开启github proxy"#)]
    enable_github_proxy: bool,
    #[arg(
        long,
        value_name = "https://example.com",
        help = "便捷反向代理配置\n\
        例如：--append-upstream-url=https://cdnjs.cloudflare.com\n\
        则访问 https://your_domain/https://cdnjs.cloudflare.com 会被代理到 https://cdnjs.cloudflare.com"
    )]
    append_upstream_url: Vec<String>,
}

pub(crate) struct Config {
    pub(crate) cert: String,
    pub(crate) key: String,
    pub(crate) basic_auth: HashMap<String, String>,
    pub(crate) web_content_path: String,
    pub(crate) referer_keywords_to_self: Vec<String>,
    pub(crate) never_ask_for_auth: bool,
    pub(crate) serving_control: ServingControl,
    pub(crate) over_tls: bool,
    pub(crate) port: Vec<u16>,
    pub(crate) reverse_proxy_config: ReverseProxyConfig,
}

pub(crate) struct ServingControl {
    pub(crate) prohibit_serving: bool,
    pub(crate) allowed_networks: Vec<IpNetwork>,
}

impl TryFrom<Param> for Config {
    type Error = DynError;
    fn try_from(mut param: Param) -> Result<Self, Self::Error> {
        let mut basic_auth = HashMap::new();
        for raw_user in param.users {
            let mut user = raw_user.split(':');
            let username = user.next().unwrap_or("").to_string();
            let password = user.next().unwrap_or("").to_string();
            if !username.is_empty() && !password.is_empty() {
                let base64 = general_purpose::STANDARD.encode(raw_user);
                basic_auth.insert(format!("Basic {base64}"), username);
            }
        }
        let reverse_proxy_config = parse_reverse_proxy_config(
            &param.reverse_proxy_config_file,
            &mut param.append_upstream_url,
            param.enable_github_proxy,
        )?;

        // 处理静态文件托管控制
        // 1. 如果设置了prohibit_serving，则禁止所有静态文件托管
        // 2. 如果会主动询问用户鉴权，且没有设置never_ask_for_auth，也禁止所有静态文件托管
        // 3. 否则根据allow_serving_network参数确定允许的网段
        let prohibit_serving = param.prohibit_serving;
        let mut allowed_networks = Vec::new();

        // 只有在不全局禁止的情况下才解析允许的网段
        if !prohibit_serving && !param.allow_serving_network.is_empty() {
            for network_str in &param.allow_serving_network {
                match IpNetwork::from_str(network_str) {
                    Ok(network) => {
                        allowed_networks.push(network);
                    }
                    Err(e) => {
                        warn!("Invalid network CIDR format: {network_str} - {e}");
                    }
                }
            }
        }

        Ok(Config {
            cert: param.cert,
            key: param.key,
            basic_auth,
            web_content_path: param.web_content_path,
            referer_keywords_to_self: param.referer_keywords_to_self,
            never_ask_for_auth: param.never_ask_for_auth,
            serving_control: ServingControl {
                prohibit_serving,
                allowed_networks,
            },
            over_tls: param.over_tls,
            port: param.port,
            reverse_proxy_config,
        })
    }
}

pub(crate) fn load_config() -> Result<Config, DynError> {
    let param = Param::parse();
    if let Err(log_init_error) = init_log(&param.log_dir, &param.log_file, "info") {
        return Err(format!("init log error:{log_init_error}").into());
    }
    info!("build time: {}", crate::BUILD_TIME);
    #[cfg(all(feature = "ring", not(feature = "aws_lc_rs")))]
    {
        info!("use ring as default crypto provider");
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    }
    #[cfg(all(feature = "aws_lc_rs", not(feature = "ring")))]
    {
        info!("use aws_lc_rs as default crypto provider");
        let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
    let config = Config::try_from(param)?;
    log_config(&config);
    info!("auto close connection after idle for {IDLE_TIMEOUT:?}");
    Ok(config)
}

fn log_config(config: &Config) {
    if config.serving_control.prohibit_serving {
        warn!("do not serve web content to avoid being detected!");
    } else {
        info!("serve web content of \"{}\"", config.web_content_path);
        if !config.serving_control.allowed_networks.is_empty() {
            info!("Only allowing static content access from networks: {:?}", config.serving_control.allowed_networks);
        } else {
            info!("Allowing static content access from all networks");
        }
        if !config.referer_keywords_to_self.is_empty() {
            info!("Referer header to images must contain {:?}", config.referer_keywords_to_self);
        }
    }
    info!("basic auth is {:?}", config.basic_auth);
    if !config.reverse_proxy_config.locations.is_empty() {
        info!("reverse proxy config: ");
    }
    config
        .reverse_proxy_config
        .locations
        .iter()
        .for_each(|reverse_proxy_config| {
            for ele in reverse_proxy_config.1 {
                info!(
                    "    {:<70} -> {}**",
                    format!("http(s)://{}:port{}**", reverse_proxy_config.0, ele.location),
                    ele.upstream.url_base,
                );
            }
        });
}
