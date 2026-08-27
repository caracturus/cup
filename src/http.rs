use std::fmt::Display;
use std::time::Duration;

use reqwest::Response;
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};

use crate::{error, Context};

pub enum RequestMethod {
    GET,
    HEAD,
}

impl Display for RequestMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RequestMethod::GET => "GET",
            RequestMethod::HEAD => "HEAD",
        })
    }
}

/// A struct for handling HTTP requests. Takes care of the repetitive work of checking for errors, etc and exposes a simple interface
pub struct Client {
    inner: ClientWithMiddleware,
    ctx: Context,
}

/// How long to wait for a TCP connection to be established.
///
/// Without this, a host that is down or silently dropping packets sits in the kernel's SYN
/// retry loop for over two minutes before `reqwest` sees a failure — multiplied by the
/// retry middleware below.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on a single request, from start to response body.
///
/// `reqwest` applies **no** timeout by default, so a peer that completes the handshake and
/// then never answers blocks the caller forever. Generous enough for a remote Cup instance
/// running its own registry check on `/api/v3/refresh`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

impl Client {
    pub fn new(ctx: &Context) -> Self {
        let inner = match reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
        {
            Ok(client) => client,
            Err(e) => error!("Failed to build the HTTP client: {}", e),
        };
        Self {
            inner: ClientBuilder::new(inner)
                .with(RetryTransientMiddleware::new_with_policy(
                    ExponentialBackoff::builder().build_with_max_retries(3),
                ))
                .build(),
            ctx: ctx.clone(),
        }
    }

    async fn request(
        &self,
        url: &str,
        method: RequestMethod,
        headers: &[(&str, Option<&str>)],
        ignore_401: bool,
    ) -> Result<Response, String> {
        let mut request = match method {
            RequestMethod::GET => self.inner.get(url),
            RequestMethod::HEAD => self.inner.head(url),
        };
        for (name, value) in headers {
            if let Some(v) = value {
                request = request.header(*name, *v)
            }
        }
        match request.send().await {
            Ok(response) => {
                let status = response.status();
                if status == 404 {
                    let message = format!("{} {}: Not found!", method, url);
                    self.ctx.logger.warn(&message);
                    Err(message)
                } else if status == 401 {
                    if ignore_401 {
                        Ok(response)
                    } else {
                        let message = format!("{} {}: Unauthorized! Please configure authentication for this registry or if you have already done so, please make sure it is correct.", method, url);
                        self.ctx.logger.warn(&message);
                        Err(message)
                    }
                } else if status == 403 {
                    let message = format!("{} {}: Forbidden! If you've configured authentication for this registry, make sure it is correct. Otherwise there is a chance that the registry is down and a proxy is returning an error.", method, url);
                    self.ctx.logger.warn(&message);
                    Err(message)
                } else if status == 502 || status == 503 {
                    let message = format!(
                        "{} {}: The registry is currently unavailabile (returned status code {}).",
                        method, url, status
                    );
                    self.ctx.logger.warn(&message);
                    Err(message)
                } else if status == 429 {
                    let message = format!(
                        "{} {}: Rate limited by the registry (returned status code 429). Skipping this check.",
                        method, url
                    );
                    self.ctx.logger.warn(&message);
                    Err(message)
                } else if status.as_u16() < 400 {
                    Ok(response)
                } else {
                    // Anything else is still a failed request: the body is an error page,
                    // not a manifest or a tag list. Handing it back as `Ok` made callers
                    // parse an error page and then kill the process over the "invalid"
                    // response it obviously was. One bad answer from one registry must cost
                    // us one image, not the whole run — under `restart: unless-stopped`
                    // exiting here turns into a crash loop.
                    let message = match method {
                        RequestMethod::GET => format!(
                            "{} {}: Unexpected error: {}",
                            method,
                            url,
                            response.text().await.unwrap_or_default()
                        ),
                        RequestMethod::HEAD => format!(
                            "{} {}: Unexpected error: Received status code {}",
                            method, url, status
                        ),
                    };
                    self.ctx.logger.warn(&message);
                    Err(message)
                }
            }
            Err(error) => {
                if error.is_connect() {
                    let message = format!("{} {}: Connection failed!", method, url);
                    self.ctx.logger.warn(&message);
                    Err(message)
                } else if error.is_timeout() {
                    let message = format!("{} {}: Connection timed out!", method, url);
                    self.ctx.logger.warn(&message);
                    Err(message)
                } else if error.is_middleware() {
                    let message = format!("{} {}: Connection failed after 3 retries!", method, url);
                    self.ctx.logger.warn(&message);
                    Err(message)
                } else {
                    error!(
                        "{} {}: Unexpected error: {}",
                        method,
                        url,
                        error.to_string()
                    )
                }
            }
        }
    }

    pub async fn get(
        &self,
        url: &str,
        headers: &[(&str, Option<&str>)],
        ignore_401: bool,
    ) -> Result<Response, String> {
        self.request(url, RequestMethod::GET, headers, ignore_401)
            .await
    }

    pub async fn head(
        &self,
        url: &str,
        headers: &[(&str, Option<&str>)],
    ) -> Result<Response, String> {
        self.request(url, RequestMethod::HEAD, headers, false).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, logging::Logger};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Serves `response` verbatim to every connection and returns the address to hit.
    async fn serve_canned_response(response: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    let _ = stream.read(&mut buffer).await;
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        format!("http://{}/v2/library/alpine/manifests/latest", addr)
    }

    fn client() -> Client {
        let ctx = Context {
            config: Config::new(),
            logger: Logger::new(false, false),
        };
        Client::new(&ctx)
    }

    /// Regression test for the crash loop of 2026-08-27.
    ///
    /// The load balancer in front of `registry-1.docker.io` answers a request whose header
    /// line exceeds 16 KiB with this exact response. Cup treated any status up to *and
    /// including* 400 as a success, so the HTML error page was handed to the caller as if
    /// it were a manifest; the caller then found no `docker-content-digest` on it and
    /// killed the process. A 400 is a failed request and must be reported as one.
    #[tokio::test]
    async fn a_400_is_an_error_not_a_successful_response() {
        let url = serve_canned_response(
            "HTTP/1.1 400 Bad Request\r\nServer: awselb/2.0\r\nContent-Type: text/html\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;

        let result = client().head(&url, &[]).await;

        assert!(
            result.is_err(),
            "a 400 should be surfaced as an error, got status {:?}",
            result.map(|response| response.status())
        );
    }

    /// Any other unhandled error status used to reach `error!`, which calls
    /// `std::process::exit(1)` — one unexpected answer from one registry took down a server
    /// that was still perfectly able to check every other image. This test would abort the
    /// whole test binary if that ever came back.
    #[tokio::test]
    async fn an_unhandled_error_status_does_not_kill_the_process() {
        let url = serve_canned_response(
            "HTTP/1.1 418 I'm a teapot\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;

        let result = client().head(&url, &[]).await;

        assert!(
            result.is_err(),
            "an unhandled error status should be surfaced as an error, got status {:?}",
            result.map(|response| response.status())
        );
    }
}
