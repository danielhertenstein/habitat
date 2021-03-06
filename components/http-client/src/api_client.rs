use std::{path::Path,
          time::Duration};

use habitat_core::{env,
                   package::PackageTarget,
                   util::sys};
use hyper::{client::{pool::{Config,
                            Pool},
                     Client as HyperClient,
                     IntoUrl,
                     RequestBuilder},
            header::UserAgent,
            http::h1::Http11Protocol,
            net::HttpsConnector};
use hyper_openssl::OpensslClient;
use openssl::ssl::{SslConnector,
                   SslConnectorBuilder,
                   SslMethod,
                   SslOption,
                   SSL_OP_NO_COMPRESSION,
                   SSL_OP_NO_SSLV2,
                   SSL_OP_NO_SSLV3,
                   SSL_VERIFY_NONE};
use url::Url;

use crate::{error::{Error,
                    Result},
            net::ProxyHttpsConnector,
            proxy::{proxy_unless_domain_exempted,
                    ProxyInfo},
            ssl};

// Read and write TCP socket timeout for Hyper/HTTP client calls.
const CLIENT_SOCKET_RW_TIMEOUT_SEC: u64 = 300;

header! { (ProxyAuthorization, "Proxy-Authorization") => [String] }

/// A generic wrapper around a Hyper HTTP client intended for API-like usage.
///
/// When an `ApiClient` is created, it has a constant URL base which is assumed to be some API
/// endpoint. This allows the underlying Hyper client to load and use any relevant HTTP proxy
/// support and to provide reasonable User-Agent HTTP headers, etc.
#[derive(Debug)]
pub struct ApiClient {
    /// The base URL for the client.
    endpoint: Url,
    /// An instance of a `hyper::Client` which is configured with an SSL context and optionally
    /// using an HTTP proxy.
    inner: HyperClient,
    /// Proxy information, if a proxy is being used.
    proxy: Option<ProxyInfo>,
    /// The URL scheme of the endpoint.
    target_scheme: String,
    /// The `User-Agent` header string to use for HTTP calls.
    user_agent_header: UserAgent,
}

impl ApiClient {
    /// Creates and returns a new `ApiClient` instance.
    ///
    /// # Errors
    ///
    /// * If the underlying Hyper client cannot be created
    /// * If a suitable SSL context cannot be established
    /// * If an HTTP proxy cannot be correctly setup
    /// * If a `User-Agent` HTTP header string cannot be constructed
    pub fn new<T>(endpoint: T,
                  product: &str,
                  version: &str,
                  fs_root_path: Option<&Path>)
                  -> Result<Self>
        where T: IntoUrl
    {
        let endpoint = endpoint.into_url().map_err(Error::UrlParseError)?;
        Ok(ApiClient { inner: new_hyper_client(&endpoint, fs_root_path)?,
                       proxy: proxy_unless_domain_exempted(Some(&endpoint))?,
                       target_scheme: endpoint.scheme().to_string(),
                       endpoint,
                       user_agent_header: user_agent(product, version)? })
    }

    /// Builds an HTTP GET request for a given path.
    pub fn get(&self, path: &str) -> RequestBuilder { self.get_with_custom_url(path, |_| {}) }

    /// Builds an HTTP GET request for a given path with the ability to customize the target URL.
    pub fn get_with_custom_url<F>(&self, path: &str, mut customize_url: F) -> RequestBuilder
        where F: FnMut(&mut Url)
    {
        let mut url = self.url_for(path);
        customize_url(&mut url);
        debug!("GET {} with {:?}", &url, &self);
        self.add_headers(self.inner.get(url))
    }

    /// Builds an HTTP HEAD request for a given path.
    pub fn head(&self, path: &str) -> RequestBuilder { self.head_with_custom_url(path, |_| {}) }

    /// Builds an HTTP HEAD request for a given path with the ability to customize the target URL.
    pub fn head_with_custom_url<F>(&self, path: &str, mut customize_url: F) -> RequestBuilder
        where F: FnMut(&mut Url)
    {
        let mut url = self.url_for(path);
        customize_url(&mut url);
        debug!("HEAD {} with {:?}", &url, &self);
        self.add_headers(self.inner.head(url))
    }

    /// Builds an HTTP PATCH request for a given path.
    pub fn patch(&self, path: &str) -> RequestBuilder { self.patch_with_custom_url(path, |_| {}) }

    /// Builds an HTTP PATCH request for a given path with the ability to customize the target URL.
    pub fn patch_with_custom_url<F>(&self, path: &str, mut customize_url: F) -> RequestBuilder
        where F: FnMut(&mut Url)
    {
        let mut url = self.url_for(path);
        customize_url(&mut url);
        debug!("PATH {} with {:?}", &url, &self);
        self.add_headers(self.inner.patch(url))
    }

    /// Builds an HTTP POST request for a given path.
    pub fn post(&self, path: &str) -> RequestBuilder { self.post_with_custom_url(path, |_| {}) }

    /// Builds an HTTP POST request for a given path with the ability to customize the target URL.
    pub fn post_with_custom_url<F>(&self, path: &str, mut customize_url: F) -> RequestBuilder
        where F: FnMut(&mut Url)
    {
        let mut url = self.url_for(path);
        customize_url(&mut url);
        debug!("POST {} with {:?}", &url, &self);
        self.add_headers(self.inner.post(url))
    }

    /// Builds an HTTP PUT request for a given path.
    pub fn put(&self, path: &str) -> RequestBuilder { self.put_with_custom_url(path, |_| {}) }

    /// Builds an HTTP PUT request for a given path with the ability to customize the target URL.
    pub fn put_with_custom_url<F>(&self, path: &str, mut customize_url: F) -> RequestBuilder
        where F: FnMut(&mut Url)
    {
        let mut url = self.url_for(path);
        customize_url(&mut url);
        debug!("PUT {} with {:?}", &url, &self);
        self.add_headers(self.inner.put(url))
    }

    /// Builds an HTTP DELETE request for a given path.
    pub fn delete(&self, path: &str) -> RequestBuilder { self.delete_with_custom_url(path, |_| {}) }

    /// Builds an HTTP DELETE request for a given path with the ability to customize the target URL.
    pub fn delete_with_custom_url<F>(&self, path: &str, mut customize_url: F) -> RequestBuilder
        where F: FnMut(&mut Url)
    {
        let mut url = self.url_for(path);
        customize_url(&mut url);
        debug!("DELETE {} with {:?}", &url, &self);
        self.add_headers(self.inner.delete(url))
    }

    fn add_headers<'a>(&'a self, rb: RequestBuilder<'a>) -> RequestBuilder {
        let mut rb = rb.header(self.user_agent_header.clone());
        // If the target URL is an `"http"` scheme and we're using a proxy server, then add the
        // proxy authorization header if appropriate. Note that for `"https"` targets, the proxy
        // server will be operating in TCP tunneling mode and will be authenticated on connection to
        // the proxy server which is why we should not add an additional header in this latter
        // case.
        if self.target_scheme == "http" {
            if let Some(ref info) = self.proxy {
                if let Some(header_value) = info.authorization_header_value() {
                    rb = rb.header(ProxyAuthorization(header_value));
                }
            }
        }
        rb
    }

    fn url_for(&self, path: &str) -> Url {
        let mut url = self.endpoint.clone();

        if path.is_empty() {
            return url;
        }

        if url.path().ends_with('/') || path.starts_with('/') {
            url.set_path(&format!("{}{}", self.endpoint.path(), path));
        } else {
            url.set_path(&format!("{}/{}", self.endpoint.path(), path));
        }

        url
    }
}

/// Builds a new hyper HTTP client with appropriate SSL configuration and HTTP/HTTPS proxy support.
///
/// ## Linux Platforms
///
/// We need a set of root certificates when connected to SSL/TLS web endpoints and this usually
/// boiled down to using an all-in-one certificate file (such as a `cacert.pem` file) or a directory
/// of files which are certificates. The strategy to location or use a reasonable set of
/// certificates is as follows:
///
/// 1. If the `SSL_CERT_FILE` environment variable is set, then use its value for the certificates.
///    Internally this is triggering default OpenSSL behavior for this environment variable.
/// 2. If the `SSL_CERT_DIR` environment variable is set, then use its value for the directory
///    containing certificates. Like the `SSL_CERT_FILE` case above, this triggers default OpenSSL
///    behavior for this environment variable.
/// 3. If the `core/cacerts` Habitat package is installed locally, then use the latest release's
///    `cacert.pem` file.
/// 4. If none of the conditions above are met, then a `cacert.pem` will be written in an SSL cache
///    directory (by default `/hab/cache/ssl` for a root user and `$HOME/.hab/cache/ssl` for a
///    non-root user) and this will be used. The contents of this file will be inlined in this
///    crate at build time as a fallback insurance policy, meaning that if the a program using this
///    code is operating in a minimal environment which may not contain system certificates, it can
///    still operate. Once a `core/cacerts` Habitat package is present, the logic would fall back
///    to preferring the package version to the cached/inline file version.
///
/// ## Mac Platforms
///
/// The Mac platform uses a Security Framework to store and find root certificates and the hyper
/// library will default to using this on the Mac. Therefore the behavior on the Mac remains
/// unchanged and will use the system's certificates.
fn new_hyper_client(url: &Url, fs_root_path: Option<&Path>) -> Result<HyperClient> {
    let connector = ssl_connector(fs_root_path)?;
    let ssl_client = OpensslClient::from(connector);

    let timeout_in_secs = match env::var("HAB_CLIENT_SOCKET_TIMEOUT") {
        Ok(t) => {
            match t.parse::<u64>() {
                Ok(n) => n,
                Err(_) => CLIENT_SOCKET_RW_TIMEOUT_SEC,
            }
        }
        Err(_) => CLIENT_SOCKET_RW_TIMEOUT_SEC,
    };
    debug!("Client socket timeout: {} secs", timeout_in_secs);

    let timeout = Some(Duration::from_secs(timeout_in_secs));

    match proxy_unless_domain_exempted(Some(url))? {
        Some(proxy) => {
            debug!("Using proxy {}:{}...", proxy.host(), proxy.port());
            let connector = ProxyHttpsConnector::new(proxy, ssl_client)?;
            let pool = Pool::with_connector(Config::default(), connector);
            let mut client = HyperClient::with_protocol(Http11Protocol::with_connector(pool));
            client.set_read_timeout(timeout);
            client.set_write_timeout(timeout);
            Ok(client)
        }
        None => {
            let connector = HttpsConnector::new(ssl_client);
            let pool = Pool::with_connector(Config::default(), connector);
            let mut client = HyperClient::with_protocol(Http11Protocol::with_connector(pool));
            client.set_read_timeout(timeout);
            client.set_write_timeout(timeout);
            Ok(client)
        }
    }
}

/// Returns an HTTP User-Agent string type for use by Hyper when making HTTP requests.
///
/// The general form for Habitat-related clients are of the following form:
///
/// ```text
/// <PRODUCT>/<VERSION> (<TARGET>; <KERNEL_RELEASE>)
/// ```
///
/// where:
///
/// * `<PRODUCT>`: is the provided product name
/// * `<VERSION>`: is the provided version string which may also include a release number
/// * `<TARGET>`: is the machine architecture and the kernel separated by a dash in lower case
/// * `<KERNEL_RELEASE>`: is the kernel release string from `uname`
///
/// For example:
///
/// ```text
/// hab/0.6.0/20160606153031 (x86_64-darwin; 14.5.0)
/// ```
///
/// # Errors
///
/// * If system information cannot be obtained via `uname`
fn user_agent(product: &str, version: &str) -> Result<UserAgent> {
    let uname = sys::uname()?;
    let ua = format!("{}/{} ({}; {})",
                     product.trim(),
                     version.trim(),
                     PackageTarget::active_target(),
                     uname.release.trim().to_lowercase());
    debug!("User-Agent: {}", &ua);
    Ok(UserAgent(ua))
}

fn ssl_connector(fs_root_path: Option<&Path>) -> Result<SslConnector> {
    let mut conn = SslConnectorBuilder::new(SslMethod::tls())?;
    let mut options = SslOption::empty();
    options.toggle(SSL_OP_NO_SSLV2);
    options.toggle(SSL_OP_NO_SSLV3);
    options.toggle(SSL_OP_NO_COMPRESSION);
    ssl::set_ca(&mut conn, fs_root_path)?;
    conn.set_options(options);
    conn.set_cipher_list("ALL!EXPORT!EXPORT40!EXPORT56!aNULL!LOW!RC4@STRENGTH")?;

    if env::var("HAB_SSL_CERT_VERIFY_NONE").is_ok() {
        conn.set_verify(SSL_VERIFY_NONE);
    }

    Ok(conn.build())
}
