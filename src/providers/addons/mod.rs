pub mod adapter;
pub mod aggregator;
pub mod client;
pub mod models;

pub use adapter::{
    meta_detail_to_media_details, meta_to_catalog_item, release_to_playback_source,
    stream_item_to_release,
};
pub use aggregator::aggregate_streams;
pub use client::AddonClient;
pub use models::{
    AddonManifest, BUILTIN_ADDON_MANIFESTS, InstalledAddon, MetaDetail, MetaItem, StreamItem,
    is_slow_host, request_timeout,
};

use crate::providers::models::{CatalogItem, MediaDetails, ProviderError, ProviderKind, Release};
use crate::providers::{Provider, ProviderCapabilities, ReleaseProvider};

/// Saved addons (`addons_config.json`) plus the built-in ones that are not saved yet.
/// Built-in manifests are fetched (cached) to learn what each addon provides.
pub async fn load_effective_addons(client: &AddonClient) -> Vec<InstalledAddon> {
    let mut list = tokio::task::spawn_blocking(crate::config::load_addons)
        .await
        .unwrap_or_default();

    let missing: Vec<(&str, &str)> = BUILTIN_ADDON_MANIFESTS
        .iter()
        .copied()
        .filter(|(_, url)| !list.iter().any(|a| a.manifest_url == *url))
        .collect();

    let futs = missing.into_iter().map(|(name, url)| async move {
        match tokio::time::timeout(request_timeout(url, 12), client.fetch_manifest(url)).await {
            Ok(Ok(manifest)) => InstalledAddon::from_manifest(url.to_string(), &manifest),
            _ => InstalledAddon::builtin_fallback(name, url),
        }
    });
    list.extend(futures::future::join_all(futs).await);
    list
}

/// Saved addons plus the built-in ones, without any network request. Asking for
/// streams must not wait for every addon's manifest (a sleeping host would eat
/// the whole time limit and the answers of the fast addons would be lost).
pub async fn load_stream_addons() -> Vec<InstalledAddon> {
    let mut list = tokio::task::spawn_blocking(crate::config::load_addons)
        .await
        .unwrap_or_default();
    for (name, url) in BUILTIN_ADDON_MANIFESTS {
        if !list.iter().any(|a| a.manifest_url == *url) {
            list.push(InstalledAddon::builtin_fallback(name, url));
        }
    }
    list
}

impl AddonClient {
    /// (type, catalog id) pairs of an addon that can be searched.
    async fn search_targets(&self, addon: &InstalledAddon) -> Vec<(String, String)> {
        if addon.is_core() {
            return vec![
                ("movie".to_string(), "top".to_string()),
                ("series".to_string(), "top".to_string()),
            ];
        }
        let manifest = match tokio::time::timeout(
            request_timeout(&addon.manifest_url, 12),
            self.fetch_manifest(&addon.manifest_url),
        )
        .await
        {
            Ok(Ok(m)) => m,
            _ => return Vec::new(),
        };
        manifest
            .catalogs
            .iter()
            .filter(|c| {
                (c.r#type == "movie" || c.r#type == "series")
                    && c.extra
                        .iter()
                        .any(|e| e.name.eq_ignore_ascii_case("search"))
            })
            .map(|c| (c.r#type.clone(), c.id.clone()))
            .collect()
    }
}

impl ReleaseProvider for AddonClient {
    async fn episode_streams(
        &self,
        id: &str,
        season: usize,
        episode: usize,
    ) -> Result<Vec<Release>, ProviderError> {
        let addons = load_stream_addons().await;
        let is_series = id.starts_with("series:") || season > 0;
        let (releases, blocked) =
            aggregate_streams(self, &addons, id, season, episode, is_series).await;
        if releases.is_empty() {
            if !blocked.is_empty() {
                return Err(ProviderError::Unavailable(format!(
                    "Only torrent/magnet streams were found ({}); direct links need a debrid-enabled addon.",
                    blocked.join(", ")
                )));
            }
            return Err(ProviderError::NotFound);
        }
        Ok(releases)
    }
}

impl Provider for AddonClient {
    fn id(&self) -> ProviderKind {
        ProviderKind::Addons
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_search: true,
            supports_pagination: false,
            supports_series: true,
            supports_subtitles: true,
            supports_homepage: false,
        }
    }

    async fn search(&self, query: &str, _page: usize) -> Result<Vec<CatalogItem>, ProviderError> {
        let addons = load_effective_addons(self).await;
        let catalog_addons: Vec<InstalledAddon> = addons
            .iter()
            .filter(|a| a.enabled && (a.provides_meta || a.provides_catalog))
            .cloned()
            .collect();

        if catalog_addons.is_empty() {
            return Err(ProviderError::Unavailable(
                "No catalog/metadata addon enabled. Open /config to configure one.".to_string(),
            ));
        }

        // Search every catalog-capable addon at the same time (Cinemeta stays first).
        let tasks = catalog_addons.into_iter().map(|addon| async move {
            let base_url = Self::base_addon_url(&addon.manifest_url);
            let targets = self.search_targets(&addon).await;
            let wait = request_timeout(&addon.manifest_url, 10);
            let mut found: Vec<MetaItem> = Vec::new();
            let searches = targets.iter().map(|(media_type, catalog_id)| {
                let base_url = base_url.clone();
                async move {
                    tokio::time::timeout(
                        wait,
                        self.fetch_catalog_search(&base_url, media_type, catalog_id, query),
                    )
                    .await
                }
            });
            for res in futures::future::join_all(searches).await {
                if let Ok(Ok(metas)) = res {
                    found.extend(metas);
                }
            }
            // Keep IMDb ids (every stream addon understands them) and the addon's
            // own ids (e.g. BanglaPlex "bpx-...").
            if !addon.is_core() {
                found.retain(|m| {
                    m.id.starts_with("tt")
                        || addon
                            .id_prefixes
                            .iter()
                            .any(|p| !p.is_empty() && m.id.starts_with(p.as_str()))
                });
            }
            found
        });
        let combined: Vec<MetaItem> = futures::future::join_all(tasks)
            .await
            .into_iter()
            .flatten()
            .collect();

        let mut seen = std::collections::HashSet::new();
        Ok(combined
            .into_iter()
            .filter(|m| seen.insert(m.id.clone()))
            .map(|m| adapter::meta_to_catalog_item(&m))
            .collect())
    }

    async fn details(&self, id: &str) -> Result<MediaDetails, ProviderError> {
        let (type_hint, clean_id) = match id.split_once(':') {
            Some((prefix, rest))
                if ["movie", "series", "tv", "anime", "other"]
                    .contains(&prefix.to_ascii_lowercase().as_str()) =>
            {
                (Some(prefix.to_ascii_lowercase()), rest)
            }
            _ => (None, id),
        };

        let addons = load_effective_addons(self).await;
        let mut target_addons: Vec<_> = addons
            .iter()
            .filter(|a| a.enabled && a.provides_meta)
            .collect();

        for addon in &addons {
            if addon.enabled
                && !target_addons
                    .iter()
                    .any(|m| m.manifest_url == addon.manifest_url)
            {
                target_addons.push(addon);
            }
        }

        let types_to_try: Vec<&str> = if let Some(hint) = &type_hint {
            let mut types = vec![hint.as_str()];
            for t in ["series", "movie", "tv", "anime", "other"] {
                if !types.contains(&t) {
                    types.push(t);
                }
            }
            types
        } else {
            vec!["series", "movie", "tv", "anime", "other"]
        };

        let mut best_detail: Option<models::MetaDetail> = None;

        for addon in &target_addons {
            let base_url = Self::base_addon_url(&addon.manifest_url);
            for t in &types_to_try {
                if let Ok(d) = self.fetch_meta(&base_url, t, clean_id).await {
                    let has_valid_id = d.id == clean_id;
                    let has_title = !d.name.trim().is_empty()
                        || d.title.as_deref().is_some_and(|t| !t.trim().is_empty());
                    if has_valid_id && has_title {
                        if !d.videos.is_empty()
                            || d.r#type.eq_ignore_ascii_case("series")
                            || d.r#type.eq_ignore_ascii_case("tv")
                        {
                            let mut details = adapter::meta_detail_to_media_details(&d);
                            if let Some(hint) = &type_hint {
                                details.id.value = format!("{hint}:{clean_id}");
                            }
                            return Ok(details);
                        }
                        if type_hint.as_deref() == Some("movie")
                            && d.r#type.eq_ignore_ascii_case("movie")
                        {
                            let mut details = adapter::meta_detail_to_media_details(&d);
                            details.id.value = format!("movie:{clean_id}");
                            return Ok(details);
                        }
                        if best_detail.is_none() {
                            best_detail = Some(d);
                        }
                    }
                }
            }
        }

        if let Some(d) = best_detail {
            let mut details = adapter::meta_detail_to_media_details(&d);
            if let Some(hint) = &type_hint {
                details.id.value = format!("{hint}:{clean_id}");
            }
            return Ok(details);
        }

        Err(ProviderError::NotFound)
    }
}
