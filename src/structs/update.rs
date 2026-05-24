use serde::{ser::SerializeStruct, Deserialize, Serialize};

use super::{container::ComposeContainer, parts::Parts, status::Status};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[cfg_attr(test, derive(PartialEq, Default))]
pub struct Update {
    pub reference: String,
    pub parts: Parts,
    pub url: Option<String>,
    pub result: UpdateResult,
    pub time: u32,
    pub server: Option<String>,
    pub in_use: bool,
    /// Whether this image belongs to a compose project (drives the UI checkbox).
    /// This is the ONLY compose info exposed via the API.
    /// `default` so updates fetched from a remote server that doesn't send this field
    /// (e.g. an upstream Cup agent) still deserialize instead of being silently dropped.
    #[serde(default)]
    pub compose_managed: bool,
    /// Compose-managed containers using this image. Kept in memory for the update handler
    /// but NOT serialized, so host paths / container IDs aren't leaked on the (commonly
    /// unauthenticated) /api/ endpoints.
    #[serde(skip_serializing, default)]
    pub compose: Vec<ComposeContainer>,
    #[serde(skip_serializing, skip_deserializing)]
    pub status: Status,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[cfg_attr(test, derive(PartialEq, Default))]
pub struct UpdateResult {
    pub has_update: Option<bool>,
    pub info: UpdateInfo,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[cfg_attr(test, derive(PartialEq, Default))]
#[serde(untagged)]
pub enum UpdateInfo {
    #[cfg_attr(test, default)]
    None,
    Version(VersionUpdateInfo),
    Digest(DigestUpdateInfo),
}

#[derive(Deserialize, Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct VersionUpdateInfo {
    pub version_update_type: String,
    pub new_tag: String,
    pub current_version: String,
    pub new_version: String,
}

#[derive(Deserialize, Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct DigestUpdateInfo {
    pub local_digests: Vec<String>,
    pub remote_digest: Option<String>,
}

impl Serialize for VersionUpdateInfo {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("VersionUpdateInfo", 5)?;
        let _ = state.serialize_field("type", "version");
        let _ = state.serialize_field("version_update_type", &self.version_update_type);
        let _ = state.serialize_field("new_tag", &self.new_tag);
        let _ = state.serialize_field("current_version", &self.current_version);
        let _ = state.serialize_field("new_version", &self.new_version);
        state.end()
    }
}

impl Serialize for DigestUpdateInfo {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("DigestUpdateInfo", 3)?;
        let _ = state.serialize_field("type", "digest");
        let _ = state.serialize_field("local_digests", &self.local_digests);
        let _ = state.serialize_field("remote_digest", &self.remote_digest);
        state.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Updates fetched from a remote server (e.g. an upstream Cup agent) must still
    /// deserialize even though their JSON has no `compose_managed` field — that field
    /// only exists in this fork. Otherwise `check::get_remote_updates`'s
    /// `serde_json::from_value(..).ok()` silently drops every remote image, and the
    /// containers on other servers disappear from the UI.
    #[test]
    fn deserializes_remote_update_without_compose_managed_field() {
        // Mirrors one image object from an upstream Cup agent's /api/v3/json response.
        let upstream_json = r#"{
            "reference": "ghcr.io/sergi0g/cup:latest",
            "parts": {"registry": "ghcr.io", "repository": "sergi0g/cup", "tag": "latest"},
            "url": null,
            "result": {
                "has_update": true,
                "info": {"type": "digest", "local_digests": ["sha256:aaa"], "remote_digest": "sha256:bbb"},
                "error": null
            },
            "time": 42,
            "server": null,
            "in_use": true
        }"#;

        let update: Update = serde_json::from_str(upstream_json)
            .expect("remote update JSON without compose_managed should deserialize");

        assert_eq!(update.reference, "ghcr.io/sergi0g/cup:latest");
        assert!(
            !update.compose_managed,
            "a missing compose_managed field must default to false"
        );
    }
}

impl Update {
    pub fn get_status(&self) -> Status {
        match &self.status {
            Status::Unknown(s) => {
                if s.is_empty() {
                    match self.result.has_update {
                        Some(true) => match &self.result.info {
                            UpdateInfo::Version(info) => match info.version_update_type.as_str() {
                                "major" => Status::UpdateMajor,
                                "minor" => Status::UpdateMinor,
                                "patch" => Status::UpdatePatch,
                                _ => unreachable!(),
                            },
                            UpdateInfo::Digest(_) => Status::UpdateAvailable,
                            _ => unreachable!(),
                        },
                        Some(false) => Status::UpToDate,
                        None => Status::Unknown(self.result.error.clone().unwrap()),
                    }
                } else {
                    self.status.clone()
                }
            }
            status => status.clone(),
        }
    }
}
