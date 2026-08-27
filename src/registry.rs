use std::time::SystemTime;

use itertools::Itertools;

use crate::{
    config::UpdateType,
    error,
    http::Client,
    structs::{
        image::{DigestInfo, Image, VersionInfo},
        version::Version,
    },
    utils::{
        link::parse_link,
        request::{
            get_protocol, get_response_body, parse_json, parse_www_authenticate, to_bearer_string,
        },
        time::{elapsed, now},
    },
    Context,
};

pub async fn check_auth(registry: &str, ctx: &Context, client: &Client) -> Option<String> {
    let protocol = get_protocol(registry, &ctx.config.registries);
    let url = format!("{}://{}/v2/", protocol, registry);
    let response = client.get(&url, &[], true).await;
    match response {
        Ok(response) => {
            let status = response.status();
            if status == 401 {
                match response.headers().get("www-authenticate") {
                        Some(challenge) => Some(parse_www_authenticate(challenge.to_str().unwrap())),
                        None => error!(
                            "Unauthorized to access registry {} and no way to authenticate was provided",
                            registry
                        ),
                    }
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

pub async fn get_latest_digest(
    image: &Image,
    token: Option<&str>,
    ctx: &Context,
    client: &Client,
) -> Image {
    ctx.logger
        .debug(format!("Checking for digest update to {}", image.reference));
    let start = SystemTime::now();
    let protocol = get_protocol(&image.parts.registry, &ctx.config.registries);
    let url = format!(
        "{}://{}/v2/{}/manifests/{}",
        protocol, &image.parts.registry, &image.parts.repository, &image.parts.tag
    );
    let authorization = to_bearer_string(&token);
    let headers = [("Accept", Some("application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json")), ("Authorization", authorization.as_deref())];

    let response = client.head(&url, &headers).await;
    let time = start.elapsed().unwrap().as_millis() as u32;
    ctx.logger.debug(format!(
        "Checked for digest update to {} in {}ms",
        image.reference, time
    ));
    // A response we can't read a digest out of costs this one image its freshness — it must
    // never cost the process. `error!` is `std::process::exit(1)`, so under
    // `restart: unless-stopped` bailing out here becomes a crash loop in which *nothing*
    // gets checked, which is exactly how one odd answer took the whole instance down on
    // 2026-08-27.
    let errored = |message: String| Image {
        error: Some(message),
        time_ms: image.time_ms + time,
        ..image.clone()
    };

    match response {
        Ok(res) => match res.headers().get("docker-content-digest") {
            Some(digest) => {
                let local_digests = match &image.digest_info {
                    Some(data) => data.local_digests.clone(),
                    None => return image.clone(),
                };
                let remote_digest = match digest.to_str() {
                    Ok(digest) => digest.to_string(),
                    Err(_) => {
                        let message = format!(
                            "{}: docker-content-digest is not readable text: {:?}",
                            url, digest
                        );
                        ctx.logger.warn(&message);
                        return errored(message);
                    }
                };
                Image {
                    digest_info: Some(DigestInfo {
                        remote_digest: Some(remote_digest),
                        local_digests,
                    }),
                    time_ms: image.time_ms + time,
                    ..image.clone()
                }
            }
            None => {
                let message = format!(
                    "{}: server returned no docker-content-digest\n{:#?}",
                    url, res
                );
                ctx.logger.warn(&message);
                errored(message)
            }
        },
        // `Client::request` has already logged this one.
        Err(error) => errored(error),
    }
}

/// The largest number of repositories a single token is allowed to cover.
///
/// A registry hands back a JWT that embeds one entry per granted scope, so a token grows
/// with the number of repositories it is asked for — measured against Docker Hub on
/// 2026-08-27, about 215 bytes per repository on top of a ~2.7 KiB base. The load balancer
/// in front of `registry-1.docker.io` rejects any request whose header line exceeds 16 KiB,
/// so once a host runs images from ~66 Docker Hub repositories the resulting
/// `Authorization: Bearer ...` header stops being deliverable and *every* manifest request
/// comes back as `400 Request Header Or Cookie Too Large` — an HTML error page with no
/// `docker-content-digest` in it. Asking for tokens in batches keeps each one far enough
/// below that ceiling that repository name length can't push it over.
pub const MAX_REPOSITORIES_PER_TOKEN: usize = 20;

/// Splits the images of a registry into the batches of repositories that [`get_token`]
/// should be called with: deduplicated (several tags of one image need one scope, and the
/// registry collapses repeats anyway) and no larger than [`MAX_REPOSITORIES_PER_TOKEN`].
pub fn batch_repositories<'a>(images: &[&'a Image]) -> Vec<Vec<&'a str>> {
    images
        .iter()
        .map(|image| image.parts.repository.as_str())
        .unique()
        .chunks(MAX_REPOSITORIES_PER_TOKEN)
        .into_iter()
        .map(|batch| batch.collect())
        .collect()
}

pub async fn get_token(
    repositories: &[&str],
    auth_url: &str,
    credentials: &Option<String>,
    client: &Client,
) -> Result<String, String> {
    let mut url = auth_url.to_owned();
    for repository in repositories {
        url = format!("{}&scope=repository:{}:pull", url, repository);
    }
    let authorization = credentials.as_ref().map(|creds| format!("Basic {}", creds));
    let headers = [("Authorization", authorization.as_deref())];

    // A failed token request (e.g. the registry returns 403/429 for the token
    // endpoint) must not be fatal. Exiting here takes down the whole server over a
    // single unreachable registry, and under `restart: unless-stopped` that becomes a
    // crash loop. Propagate the error so the caller can skip this registry's images.
    let response = client.get(&url, &headers, false).await?;
    let response_json = parse_json(&get_response_body(response).await);
    match response_json["token"].as_str() {
        Some(token) => Ok(token.to_string()),
        None => Err(format!(
            "GET {}: Registry did not include a token in its response.",
            url
        )),
    }
}

pub async fn get_latest_tag(
    image: &Image,
    base: &Version,
    token: Option<&str>,
    ctx: &Context,
    client: &Client,
    excluded_tags: Vec<String>,
) -> Image {
    ctx.logger
        .debug(format!("Checking for tag update to {}", image.reference));
    let start = now();
    let protocol = get_protocol(&image.parts.registry, &ctx.config.registries);
    let url = format!(
        "{}://{}/v2/{}/tags/list",
        protocol, &image.parts.registry, &image.parts.repository,
    );
    let authorization = to_bearer_string(&token);
    let headers = [
        ("Accept", Some("application/json")),
        ("Authorization", authorization.as_deref()),
    ];

    let mut tags: Vec<Version> = Vec::new();
    let mut next_url = Some(url);

    while next_url.is_some() {
        ctx.logger.debug(format!(
            "{} has extra tags! Current number of valid tags: {}",
            image.reference,
            tags.len()
        ));
        let (new_tags, next) = match get_extra_tags(
            &next_url.unwrap(),
            &headers,
            base,
            &image.version_info.as_ref().unwrap().format_str,
            ctx,
            client,
            &excluded_tags,
        )
        .await
        {
            Ok(t) => t,
            Err(message) => {
                return Image {
                    error: Some(message),
                    time_ms: image.time_ms + elapsed(start),
                    ..image.clone()
                }
            }
        };
        tags.extend_from_slice(&new_tags);
        next_url = next;
    }
    let tag = tags.iter().max();
    ctx.logger.debug(format!(
        "Checked for tag update to {} in {}ms",
        image.reference,
        elapsed(start)
    ));
    match tag {
        Some(t) => {
            if t == base && image.digest_info.is_some() {
                // Tags are equal so we'll compare digests
                ctx.logger.debug(format!(
                    "Tags for {} are equal, comparing digests.",
                    image.reference
                ));
                get_latest_digest(
                    &Image {
                        version_info: None, // Overwrite previous version info, since it isn't useful anymore (equal tags means up to date and an image is truly up to date when its digests are up to date, and we'll be checking those anyway)
                        time_ms: image.time_ms + elapsed(start),
                        ..image.clone()
                    },
                    token,
                    ctx,
                    client,
                )
                .await
            } else {
                Image {
                    version_info: Some(VersionInfo {
                        latest_remote_tag: Some(t.clone()),
                        ..image.version_info.as_ref().unwrap().clone()
                    }),
                    time_ms: image.time_ms + elapsed(start),
                    ..image.clone()
                }
            }
        }
        None => error!(
            "Image {} has no remote version tags! Local tag: {}",
            image.reference, image.parts.tag
        ),
    }
}

/// Checks if a tag matches any of the excluded tag prefixes.
fn is_excluded_tag(tag: &str, excluded_tags: &[String], ctx: &Context) -> bool {
    for excluded in excluded_tags {
        if tag.starts_with(excluded) {
            ctx.logger.debug(format!(
                "Ignoring tag \"{}\" as it matches excluded prefix \"{}\"",
                tag, excluded
            ));
            return true;
        }
    }
    false
}

pub async fn get_extra_tags(
    url: &str,
    headers: &[(&str, Option<&str>)],
    base: &Version,
    format_str: &str,
    ctx: &Context,
    client: &Client,
    excluded_tags: &[String],
) -> Result<(Vec<Version>, Option<String>), String> {
    let response = client.get(url, headers, false).await;

    match response {
        Ok(res) => {
            let next_url = res
                .headers()
                .get("Link")
                .map(|link| parse_link(link.to_str().unwrap(), url));
            let response_json = parse_json(&get_response_body(res).await);
            let result = response_json["tags"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|tag| !is_excluded_tag(tag.as_str().unwrap(), excluded_tags, ctx))
                .filter_map(|tag| Version::from_tag(tag.as_str().unwrap()))
                .filter(|(tag, format_string)| match (base.minor, tag.minor) {
                    (Some(_), Some(_)) | (None, None) => {
                        matches!((base.patch, tag.patch), (Some(_), Some(_)) | (None, None))
                            && format_str == *format_string
                    }
                    _ => false,
                })
                .filter_map(|(tag, _)| match ctx.config.ignore_update_type {
                    UpdateType::None => Some(tag),
                    UpdateType::Major => Some(tag).filter(|tag| base.major == tag.major),
                    UpdateType::Minor => {
                        Some(tag).filter(|tag| base.major == tag.major && base.minor == tag.minor)
                    }
                    UpdateType::Patch => Some(tag).filter(|tag| {
                        base.major == tag.major
                            && base.minor == tag.minor
                            && base.patch == tag.patch
                    }),
                })
                .dedup()
                .collect();
            Ok((result, next_url))
        }
        Err(message) => Err(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, RegistryConfig};
    use crate::logging::Logger;

    fn images(references: &[String]) -> Vec<Image> {
        references
            .iter()
            .map(|reference| Image::from_reference(reference))
            .collect()
    }

    /// One scope per *repository*, not per image: a host commonly runs several tags of the
    /// same repository, and asking for the same scope repeatedly only inflates the URL.
    #[test]
    fn batches_are_deduplicated_and_cover_every_repository_once() {
        let references: Vec<String> = (0..45)
            .flat_map(|i| {
                [
                    format!("user/app{}:1.0.0", i),
                    format!("user/app{}:2.0.0", i),
                ]
            })
            .collect();
        let images = images(&references);
        let images: Vec<&Image> = images.iter().collect();

        let batches = batch_repositories(&images);
        let requested: Vec<&str> = batches.concat();

        assert_eq!(
            requested.len(),
            45,
            "expected each of the 45 repositories once, got {:?}",
            requested
        );
        assert!(
            batches
                .iter()
                .all(|batch| batch.len() <= MAX_REPOSITORIES_PER_TOKEN),
            "a batch exceeded {} repositories: {:?}",
            MAX_REPOSITORIES_PER_TOKEN,
            batches.iter().map(|batch| batch.len()).collect::<Vec<_>>()
        );
    }

    /// End-to-end check against the live Docker Hub, ignored by default because it needs
    /// network access. Run with:
    ///
    /// ```sh
    /// cargo test --no-default-features --features cli -- --ignored --nocapture
    /// ```
    ///
    /// It walks the same path `check.rs` does — batch the repositories, take one token per
    /// batch, then use each repository's token — for more repositories than a single token
    /// could ever have covered, and asserts the registry actually answers.
    #[tokio::test]
    #[ignore = "hits the live Docker Hub"]
    async fn batched_tokens_are_accepted_by_the_live_registry() {
        use crate::config::Config;
        use crate::logging::Logger;

        #[rustfmt::skip]
        const REPOSITORIES: [&str; 70] = [
            "library/alpine", "library/nginx", "library/redis", "library/postgres",
            "library/mysql", "library/mariadb", "library/mongo", "library/node",
            "library/python", "library/golang", "library/rust", "library/ruby",
            "library/php", "library/httpd", "library/tomcat", "library/busybox",
            "library/ubuntu", "library/debian", "library/memcached", "library/rabbitmq",
            "library/influxdb", "library/telegraf", "library/wordpress", "library/drupal",
            "library/joomla", "library/ghost", "library/registry", "library/almalinux",
            "library/photon", "library/traefik", "library/haproxy", "library/caddy",
            "library/varnish", "library/composer", "library/groovy",
            "library/cassandra", "library/couchdb", "library/neo4j", "library/adminer",
            "library/phpmyadmin", "library/gcc", "library/perl", "library/swift",
            "library/eclipse-temurin", "library/amazoncorretto", "library/maven",
            "library/gradle", "library/sonarqube", "library/kapacitor", "library/nextcloud",
            "library/matomo", "library/mediawiki", "library/redmine", "library/jruby",
            "library/nats", "library/kong", "library/solr", "library/zookeeper",
            "library/erlang", "library/elixir", "library/haskell", "library/clojure",
            "library/julia", "library/r-base", "library/mono", "library/dart",
            "library/flink", "library/spark", "library/storm", "library/pypy",
        ];

        let ctx = Context {
            config: Config::new(),
            logger: Logger::new(false, false),
        };
        let client = Client::new(&ctx);
        let registry = "registry-1.docker.io";

        let auth_url = check_auth(registry, &ctx, &client)
            .await
            .expect("Docker Hub should ask us to authenticate");

        let images: Vec<Image> = REPOSITORIES
            .iter()
            .map(|repository| Image::from_reference(&format!("{}:1.0.0", repository)))
            .collect();
        let images: Vec<&Image> = images.iter().collect();

        let mut tokens: Vec<(&str, String)> = Vec::new();
        for batch in batch_repositories(&images) {
            let token = get_token(&batch, &auth_url, &None, &client)
                .await
                .expect("the token endpoint should answer");
            let header_len = "Authorization: Bearer ".len() + token.len();
            println!(
                "batch of {} repositories -> {} byte header",
                batch.len(),
                header_len
            );
            for repository in batch {
                tokens.push((repository, token.clone()));
            }
        }

        assert_eq!(tokens.len(), REPOSITORIES.len());

        for (repository, token) in &tokens {
            let url = format!("https://{}/v2/{}/manifests/latest", registry, repository);
            let authorization = to_bearer_string(&Some(token.as_str()));
            let headers = [("Authorization", authorization.as_deref())];
            let status = client
                .head(&url, &headers)
                .await
                .unwrap_or_else(|error| panic!("HEAD {} failed: {}", url, error))
                .status();
            assert_eq!(status, 200, "HEAD {} returned {}", url, status);
        }
    }

    /// Regression test for the crash loop of 2026-08-27.
    ///
    /// Cup used to request a single token covering every repository of a registry. Docker
    /// Hub embeds one entry per granted scope in the JWT, so on a host with enough images
    /// the `Authorization` header grew past the 16 KiB per-header limit of the load
    /// balancer in front of `registry-1.docker.io`, which answered every manifest request
    /// with `400 Request Header Or Cookie Too Large`. Measured against the live registry:
    /// 65 repositories produced a 16 126 byte header and a 200; 70 produced 16 967 and a 400.
    #[test]
    fn a_full_batch_cannot_approach_the_registry_header_limit() {
        /// Header line a request is rejected at, measured on registry-1.docker.io.
        const REGISTRY_HEADER_LIMIT: usize = 16_376;
        /// Size of a token granting no scopes at all.
        const BASE_TOKEN_BYTES: usize = 2_800;
        /// Growth per granted scope. Measured at ~215 bytes for typical repository names;
        /// padded here so unusually long names stay covered.
        const BYTES_PER_SCOPE: usize = 260;

        let worst_case = "Authorization: Bearer ".len()
            + BASE_TOKEN_BYTES
            + MAX_REPOSITORIES_PER_TOKEN * BYTES_PER_SCOPE;

        assert!(
            worst_case * 2 < REGISTRY_HEADER_LIMIT,
            "a token for {} repositories can reach {} bytes, which leaves no margin under \
             the registry's {} byte header limit",
            MAX_REPOSITORIES_PER_TOKEN,
            worst_case,
            REGISTRY_HEADER_LIMIT
        );
    }

    /// Spins up a listener that answers every request with `raw_response`, points `image`
    /// at it, and returns the result of checking that image.
    async fn check_against_registry_answering(image: &Image, raw_response: &'static [u8]) -> Image {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buffer = [0u8; 2048];
                    let _ = stream.read(&mut buffer).await;
                    let _ = stream.write_all(raw_response).await;
                    let _ = stream.flush().await;
                });
            }
        });

        let mut config = Config::new();
        config.registries.insert(
            addr.to_string(),
            RegistryConfig {
                insecure: true,
                ..Default::default()
            },
        );
        let ctx = Context {
            config,
            logger: Logger::new(false, false),
        };
        let client = Client::new(&ctx);
        let mut image = image.clone();
        let (registry, repository, tag) = crate::utils::reference::split(&format!(
            "{}/{}:{}",
            addr, image.parts.repository, image.parts.tag
        ));
        image.parts = crate::structs::parts::Parts {
            registry,
            repository,
            tag,
        };

        get_latest_digest(&image, None, &ctx, &client).await
    }

    /// An image the daemon knows the local digest of — the shape `get_latest_digest`
    /// actually compares against, and the only one that reads the digest header's value.
    fn image_with_local_digest() -> Image {
        let mut image = Image::from_reference("library/nginx:1.0.0");
        image.digest_info = Some(DigestInfo {
            local_digests: vec!["sha256:aaaa".to_string()],
            remote_digest: None,
        });
        image
    }

    /// Regression test for the crash loop of 2026-08-27.
    ///
    /// A response Cup considers successful but that carries no `docker-content-digest` —
    /// a proxy or gateway answering 200 with an error page, say — fell through to the
    /// `error!` macro, which is `std::process::exit(1)`. Under `restart: unless-stopped`
    /// that turns one odd response into an endless restart loop in which *no* image gets
    /// checked. A bad answer must cost that one image its freshness, not the process.
    ///
    /// (Statuses >= 400 no longer reach here at all — `Client::request` turns those into
    /// `Err` since `e62c538` — but a 2xx without the header still did.)
    #[tokio::test]
    async fn manifest_without_digest_header_errors_the_image_instead_of_exiting() {
        let image = check_against_registry_answering(
            &image_with_local_digest(),
            b"HTTP/1.1 200 OK\r\n\
              content-type: text/html\r\n\
              content-length: 0\r\n\r\n",
        )
        .await;

        assert!(
            image.error.is_some(),
            "a successful response with no digest header should mark the image errored"
        );
        assert!(
            image
                .digest_info
                .as_ref()
                .is_none_or(|info| info.remote_digest.is_none()),
            "no remote digest should be recorded from a failed check"
        );
    }

    /// The same crash loop through a different door: the digest header is present but its
    /// value isn't valid UTF-8, so `to_str()` fails. That used to be an `unwrap()`, and the
    /// release profile sets `panic = "abort"`, so it killed the process just as surely.
    #[tokio::test]
    async fn manifest_with_unreadable_digest_header_errors_the_image_instead_of_panicking() {
        let image = check_against_registry_answering(
            &image_with_local_digest(),
            b"HTTP/1.1 200 OK\r\n\
              docker-content-digest: \xff\xfe\r\n\
              content-length: 0\r\n\r\n",
        )
        .await;

        assert!(
            image.error.is_some(),
            "an unreadable digest header should mark the image errored"
        );
    }
}
