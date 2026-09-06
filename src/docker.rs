use bollard::{container::ListContainersOptions, models::ImageInspect, ClientVersion, Docker};

use futures::future::join_all;
use rustc_hash::FxHashMap;

use crate::{
    error,
    structs::{container::ComposeContainer, image::Image},
    Context,
};

fn create_docker_client(socket: Option<&str>) -> Docker {
    let client: Result<Docker, bollard::errors::Error> = match socket {
        Some(sock) => {
            if sock.starts_with("unix://") {
                Docker::connect_with_unix(
                    sock,
                    120,
                    &ClientVersion {
                        major_version: 1,
                        minor_version: 44,
                    },
                )
            } else {
                Docker::connect_with_http(
                    sock,
                    120,
                    &ClientVersion {
                        major_version: 1,
                        minor_version: 44,
                    },
                )
            }
        }
        None => Docker::connect_with_unix_defaults(),
    };

    match client {
        Ok(d) => d,
        Err(e) => error!("Failed to connect to docker daemon!\n{}", e),
    }
}

/// Retrieves images from Docker daemon. If `references` is Some, return only the images whose references match the ones specified.
pub async fn get_images_from_docker_daemon(
    ctx: &Context,
    references: &Option<Vec<String>>,
) -> Vec<Image> {
    if ctx.config.socket.as_deref() == Some("none") {
        return vec![];
    }
    let client: Docker = create_docker_client(ctx.config.socket.as_deref());
    let mut swarm_images = match client.list_services::<String>(None).await {
        Ok(services) => services
            .iter()
            .filter_map(|service| match &service.spec {
                Some(service_spec) => match &service_spec.task_template {
                    Some(task_spec) => match &task_spec.container_spec {
                        Some(container_spec) => match &container_spec.image {
                            Some(image) => Image::from_inspect_data(ctx, image),
                            None => None,
                        },
                        None => None,
                    },
                    None => None,
                },
                None => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    let mut local_images = match references {
        Some(refs) => {
            let mut inspect_handles = Vec::with_capacity(refs.len());
            for reference in refs {
                inspect_handles.push(client.inspect_image(reference));
            }
            let inspects: Vec<ImageInspect> = join_all(inspect_handles)
                .await
                .iter()
                .filter(|inspect| inspect.is_ok())
                .map(|inspect| inspect.as_ref().unwrap().clone())
                .collect();
            inspects
                .iter()
                .filter_map(|inspect| Image::from_inspect_data(ctx, inspect.clone()))
                .collect()
        }
        None => {
            let images = match client.list_images::<String>(None).await {
                Ok(images) => images,
                Err(e) => {
                    error!("Failed to retrieve list of images available!\n{}", e)
                }
            };
            images
                .iter()
                .filter_map(|image| Image::from_inspect_data(ctx, image.clone()))
                .collect::<Vec<Image>>()
        }
    };
    local_images.append(&mut swarm_images);
    local_images
}

pub async fn get_in_use_images(ctx: &Context) -> Vec<String> {
    if ctx.config.socket.as_deref() == Some("none") {
        return vec![];
    }

    let client: Docker = create_docker_client(ctx.config.socket.as_deref());

    let containers = match client
        .list_containers::<String>(Some(ListContainersOptions {
            all: true,
            ..Default::default()
        }))
        .await
    {
        Ok(containers) => containers,
        Err(e) => {
            error!("Failed to retrieve list of containers available!\n{}", e)
        }
    };

    containers
        .iter()
        .filter_map(|container| match &container.image {
            Some(image) => Some({
                if image.contains(":") {
                    image.clone()
                } else {
                    format!("{image}:latest")
                }
            }),
            None => None,
        })
        .collect()
}

/// Maps each in-use image reference to the running, compose-managed containers using it.
///
/// Only containers that belong to a Docker Compose project (i.e. carry the
/// `com.docker.compose.project` label) are included — there's nothing to
/// `docker compose up` for a hand-run container. One-off containers
/// (`docker compose run`) are skipped. The image reference is normalized the same
/// way as [`get_in_use_images`] so the keys line up with `Update::reference`.
///
/// Returns an empty map (after logging a warning) if the daemon can't be queried —
/// compose info is enrichment, so failing to get it must not crash update checking.
pub async fn get_compose_containers(ctx: &Context) -> FxHashMap<String, Vec<ComposeContainer>> {
    let mut map: FxHashMap<String, Vec<ComposeContainer>> = FxHashMap::default();
    if ctx.config.socket.as_deref() == Some("none") {
        return map;
    }

    let client: Docker = create_docker_client(ctx.config.socket.as_deref());

    let containers = match client
        .list_containers::<String>(Some(ListContainersOptions {
            all: true,
            ..Default::default()
        }))
        .await
    {
        Ok(containers) => containers,
        Err(e) => {
            ctx.logger.warn(format!(
                "Failed to retrieve containers for compose mapping: {}",
                e
            ));
            return map;
        }
    };

    for container in &containers {
        let labels = match &container.labels {
            Some(labels) => labels,
            None => continue,
        };
        // Only compose-managed containers carry this label.
        let project = match labels.get("com.docker.compose.project") {
            Some(project) => project.clone(),
            None => continue,
        };
        // Skip one-off containers created by `docker compose run`.
        if labels
            .get("com.docker.compose.oneoff")
            .map(|v| v == "True")
            .unwrap_or(false)
        {
            continue;
        }
        let reference = match &container.image {
            Some(image) if image.contains(':') => image.clone(),
            Some(image) => format!("{image}:latest"),
            None => continue,
        };
        let compose_container = ComposeContainer {
            id: container.id.clone().unwrap_or_default(),
            name: container
                .names
                .as_ref()
                .and_then(|names| names.first())
                .map(|name| name.trim_start_matches('/').to_string())
                .unwrap_or_default(),
            service: labels
                .get("com.docker.compose.service")
                .cloned()
                .unwrap_or_default(),
            project,
            working_dir: labels
                .get("com.docker.compose.project.working_dir")
                .cloned()
                .unwrap_or_default(),
            config_files: labels
                .get("com.docker.compose.project.config_files")
                .cloned()
                .unwrap_or_default(),
        };
        map.entry(reference).or_default().push(compose_container);
    }

    map
}
