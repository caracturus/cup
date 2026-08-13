use std::time::Duration;

use futures::future::join_all;
use itertools::Itertools;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    docker::{get_compose_containers, get_images_from_docker_daemon, get_in_use_images},
    http::Client,
    registry::{check_auth, get_token},
    structs::{image::Image, update::Update},
    utils::{
        reference::split,
        request::{get_response_body, parse_json},
    },
    Context,
};

/// Upper bound on how long we'll wait for a single remote Cup instance.
///
/// Deliberately generous, because `/api/v3/refresh` makes the remote run its own full
/// registry check before it answers. But it must exist: `serve()` does not bind its port
/// until the first check finishes, so a single wedged peer would otherwise stop Cup from
/// ever starting.
const REMOTE_SERVER_TIMEOUT: Duration = Duration::from_secs(90);

/// Fetches image data from a single remote Cup instance.
async fn get_server_updates(
    ctx: &Context,
    client: &Client,
    name: &str,
    url: &str,
    refresh: bool,
) -> Vec<Update> {
    let base_url = if url.starts_with("http://") || url.starts_with("https://") {
        format!("{}/api/v3/", url.trim_end_matches('/'))
    } else {
        format!("https://{}/api/v3/", url.trim_end_matches('/'))
    };
    let json_url = base_url.clone() + "json";
    if refresh {
        let refresh_url = base_url + "refresh";
        match client.get(&refresh_url, &[], false).await {
            Ok(response) => {
                if response.status() != 200 {
                    ctx.logger.warn(format!("GET {}: Failed to refresh server. Server returned invalid response code: {}", refresh_url, response.status()));
                    return Vec::new();
                }
            }
            Err(e) => {
                ctx.logger.warn(format!(
                    "GET {}: Failed to refresh server. {}",
                    refresh_url, e
                ));
                return Vec::new();
            }
        }
    }
    match client.get(&json_url, &[], false).await {
        Ok(response) => {
            if response.status() != 200 {
                ctx.logger.warn(format!("GET {}: Failed to fetch updates from server. Server returned invalid response code: {}", json_url, response.status()));
                return Vec::new();
            }
            let json = parse_json(&get_response_body(response).await);
            ctx.logger
                .debug(format!("JSON response for {}: {}", name, json));
            if let Some(updates) = json["images"].as_array() {
                let mut server_updates: Vec<Update> = updates
                    .iter()
                    .filter_map(|img| serde_json::from_value(img.clone()).ok())
                    .collect();
                // Add server origin to each image
                for update in &mut server_updates {
                    update.server = Some(name.to_string());
                    update.status = update.get_status();
                }
                ctx.logger
                    .debug(format!("Updates for {}: {:#?}", name, server_updates));
                return server_updates;
            }

            Vec::new()
        }
        Err(e) => {
            ctx.logger.warn(format!(
                "GET {}: Failed to fetch updates from server. {}",
                json_url, e
            ));
            Vec::new()
        }
    }
}

/// Fetches image data from other Cup instances
async fn get_remote_updates(ctx: &Context, client: &Client, refresh: bool) -> Vec<Update> {
    get_remote_updates_bounded(ctx, client, refresh, REMOTE_SERVER_TIMEOUT).await
}

/// The body of [`get_remote_updates`], with the per-server timeout injected so tests don't
/// have to wait out the production value.
async fn get_remote_updates_bounded(
    ctx: &Context,
    client: &Client,
    refresh: bool,
    per_server_timeout: Duration,
) -> Vec<Update> {
    let mut remote_images = Vec::new();

    let handles: Vec<_> = ctx
        .config
        .servers
        .iter()
        .map(|(name, url)| async move {
            match tokio::time::timeout(
                per_server_timeout,
                get_server_updates(ctx, client, name, url, refresh),
            )
            .await
            {
                Ok(updates) => updates,
                Err(_) => {
                    ctx.logger.warn(format!(
                        "Timed out after {}s waiting for server {} ({}). Skipping it.",
                        per_server_timeout.as_secs(),
                        name,
                        url
                    ));
                    Vec::new()
                }
            }
        })
        .collect();

    for mut images in join_all(handles).await {
        remote_images.append(&mut images);
    }

    remote_images
}

/// Returns a list of excluded tag prefixes for the given image.
fn get_excluded_tags(image: &Image, ctx: &Context) -> Vec<String> {
    let image_name = image.reference.split(':').next().unwrap();
    ctx.config
        .images
        .exclude
        .iter()
        .filter(|item| item.starts_with(image_name))
        .filter_map(|excluded| {
            let tag = split(excluded).2;
            (tag != "latest").then_some(tag)
        })
        .collect()
}

/// Returns a list of updates for all images passed in.
pub async fn get_updates(
    references: &Option<Vec<String>>, // If a user requested _specific_ references to be checked, this will have a value
    refresh: bool,
    ctx: &Context,
) -> Vec<Update> {
    let client = Client::new(ctx);

    // Merge references argument with references from config
    let all_references = match &references {
        Some(refs) => {
            if !ctx.config.images.extra.is_empty() {
                refs.clone().extend_from_slice(&ctx.config.images.extra);
            }
            refs
        }
        None => &ctx.config.images.extra,
    };

    // Get local images
    ctx.logger.debug("Retrieving images to be checked");
    let mut images = get_images_from_docker_daemon(ctx, references).await;
    let in_use_images = get_in_use_images(ctx).await;
    ctx.logger
        .debug(format!("Found {} images in use", in_use_images.len()));

    // Complete in_use field
    images.iter_mut().for_each(|image| {
        if in_use_images.contains(&image.reference) {
            image.in_use = true
        }
    });

    // Add extra images from references
    if !all_references.is_empty() {
        let image_refs: FxHashSet<&String> = images.iter().map(|image| &image.reference).collect();
        let extra = all_references
            .iter()
            .filter(|&reference| !image_refs.contains(reference))
            .map(|reference| Image::from_reference(reference))
            .collect::<Vec<Image>>();
        images.extend(extra);
    }

    // Get remote images from other servers
    let remote_updates = if !ctx.config.servers.is_empty() {
        ctx.logger.debug("Fetching updates from remote servers");
        get_remote_updates(ctx, &client, refresh).await
    } else {
        Vec::new()
    };

    ctx.logger.debug(format!(
        "Checking {:?}",
        images.iter().map(|image| &image.reference).collect_vec()
    ));

    // Get a list of unique registries our images belong to. We are unwrapping the registry because it's guaranteed to be there.
    let registries: Vec<&String> = images
        .iter()
        .map(|image| &image.parts.registry)
        .unique()
        .filter(|&registry| match ctx.config.registries.get(registry) {
            Some(config) => !config.ignore,
            None => true,
        })
        .collect::<Vec<&String>>();

    // Create request client. All network requests share the same client for better performance.
    // This client is also configured to retry a failed request up to 3 times with exponential backoff in between.
    let client = Client::new(ctx);

    // Create a map of images indexed by registry. This solution seems quite inefficient, since each iteration causes a key to be looked up. I can't find anything better at the moment.
    let mut image_map: FxHashMap<&String, Vec<&Image>> = FxHashMap::default();

    for image in &images {
        image_map
            .entry(&image.parts.registry)
            .or_default()
            .push(image);
    }

    // Retrieve an authentication token (if required) for each registry.
    let mut tokens: FxHashMap<&str, Option<String>> = FxHashMap::default();
    // Registries whose token could not be fetched (e.g. the token endpoint returned
    // 403/429). Their images are surfaced as errored rather than crashing the run.
    let mut token_errors: FxHashMap<&str, String> = FxHashMap::default();
    for registry in registries.clone() {
        let credentials = if let Some(registry_config) = ctx.config.registries.get(registry) {
            &registry_config.authentication
        } else {
            &None
        };
        match check_auth(registry, ctx, &client).await {
            Some(auth_url) => {
                match get_token(
                    image_map.get(registry).unwrap(),
                    &auth_url,
                    credentials,
                    &client,
                )
                .await
                {
                    Ok(token) => {
                        tokens.insert(registry, Some(token));
                    }
                    Err(error) => {
                        token_errors.insert(registry, error);
                    }
                }
            }
            None => {
                tokens.insert(registry, None);
            }
        }
    }

    ctx.logger.debug(format!("Tokens: {:?}", tokens));

    let mut handles = Vec::with_capacity(images.len());
    // Images belonging to a registry whose token fetch failed. They're surfaced with
    // the token error instead of being checked (which would only fail again).
    let mut errored_images: Vec<Image> = Vec::new();

    // Loop through images check for updates
    for image in &images {
        let is_ignored = !registries.contains(&&image.parts.registry)
            || ctx
                .config
                .images
                .exclude
                .iter()
                .any(|item| image.reference.starts_with(item));
        if !is_ignored {
            if let Some(error) = token_errors.get(image.parts.registry.as_str()) {
                errored_images.push(Image {
                    error: Some(error.clone()),
                    ..image.clone()
                });
                continue;
            }
            let excluded_tags = get_excluded_tags(image, ctx);
            let token = tokens.get(image.parts.registry.as_str()).unwrap();
            let future = image.check(token.as_deref(), ctx, &client, excluded_tags);
            handles.push(future);
        }
    }
    // Await all the futures
    let mut images = join_all(handles).await;
    images.extend(errored_images);
    let mut updates: Vec<Update> = images.iter().map(|image| image.to_update()).collect();

    // Attach compose-project info (folder, service, project) to each local update, so the
    // API/UI can map an available update back to the folder that can apply it.
    let compose_map = get_compose_containers(ctx).await;
    for update in &mut updates {
        if let Some(containers) = compose_map.get(&update.reference) {
            update.compose_managed = !containers.is_empty();
            update.compose = containers.clone();
        }
    }

    updates.extend_from_slice(&remote_updates);
    updates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::logging::Logger;
    use std::time::Instant;

    /// Regression test for the startup hang: a remote server that completes the TCP
    /// handshake and then never sends a byte used to block `get_remote_updates` forever,
    /// because the HTTP client had no timeout. `serve()` runs this check *before* binding
    /// its port, so one such peer kept Cup from ever starting.
    #[tokio::test]
    async fn unresponsive_remote_server_is_skipped_not_awaited_forever() {
        // A listener that accepts connections and then holds them open in silence.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut accepted = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                accepted.push(stream);
            }
        });

        let mut config = Config::new();
        config
            .servers
            .insert("stalled".to_string(), format!("http://{}", addr));
        let ctx = Context {
            config,
            logger: Logger::new(false, false),
        };
        let client = Client::new(&ctx);

        let per_server_timeout = Duration::from_secs(1);
        let start = Instant::now();
        let updates = get_remote_updates_bounded(&ctx, &client, true, per_server_timeout).await;
        let elapsed = start.elapsed();

        assert!(
            updates.is_empty(),
            "a silent server should contribute no updates"
        );
        assert!(
            elapsed < per_server_timeout * 5,
            "expected the stalled server to be abandoned near {:?}, but waited {:?}",
            per_server_timeout,
            elapsed
        );
    }
}
