use serde::{Deserialize, Serialize};

/// Information about a running container that belongs to a Docker Compose project.
///
/// This is what lets us map an available image update back to the compose project
/// (and folder) that can actually apply it via `docker compose pull && up -d`.
/// Populated from the `com.docker.compose.*` labels Docker writes onto every
/// compose-managed container. Containers without those labels are not represented
/// here, because there's no compose project to update.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub struct ComposeContainer {
    /// Full container ID (used to refuse updating Cup's own container).
    pub id: String,
    /// Container name, e.g. `test-web` (Docker's leading `/` stripped).
    pub name: String,
    /// Compose service name (`com.docker.compose.service`).
    pub service: String,
    /// Compose project name (`com.docker.compose.project`).
    pub project: String,
    /// Absolute path of the compose project directory
    /// (`com.docker.compose.project.working_dir`) — the folder we run compose in.
    pub working_dir: String,
    /// Compose config file path(s) (`com.docker.compose.project.config_files`).
    pub config_files: String,
}
