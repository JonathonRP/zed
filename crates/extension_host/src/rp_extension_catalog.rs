use anyhow::{Context as _, Result, bail, ensure};
use cloud_api_types::{ExtensionApiManifest, ExtensionMetadata, ExtensionProvides};
use collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use fs::Fs;
use futures::{AsyncReadExt as _, StreamExt as _};
use http_client::{
    AsyncBody, HttpClient, HttpClientWithUrl, HttpRequestExt as _, RedirectPolicy, Request,
    StatusCode, Url,
};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

const CATALOG_URL: &str = "https://jonathonrp.github.io/extensions/rp-catalog/v1/catalog.json";
const CATALOG_DIGEST_URL: &str =
    "https://jonathonrp.github.io/extensions/rp-catalog/v1/catalog.json.sha256";
const CATALOG_SCHEMA_URL: &str =
    "https://jonathonrp.github.io/extensions/rp-catalog/v1/schema.json";
const RP_CATALOG_HOST: &str = "jonathonrp.github.io";
const UPSTREAM_API_HOST: &str = "api.zed.dev";
const UPSTREAM_ARCHIVE_HOST: &str = "zed-extensions.nyc3.digitaloceanspaces.com";
const MAX_REDIRECTS: usize = 5;
const MAX_CATALOG_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 20_000;
const MAX_ARCHIVE_COMPRESSED_BYTES: u64 = 128 * 1024 * 1024;
const MAX_ARCHIVE_DEPTH: usize = 32;
const MAX_ARCHIVE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ARCHIVE_UNCOMPRESSED_BYTES: u64 = 512 * 1024 * 1024;

pub(crate) fn is_enabled() -> bool {
    release_channel::rp_release_metadata().is_some()
}

#[derive(Clone)]
pub(crate) struct RpExtensionCatalogClient {
    fs: Arc<dyn Fs>,
    http_client: Arc<HttpClientWithUrl>,
    state_path: PathBuf,
    provenance_path: PathBuf,
    catalog: Arc<Mutex<Option<Arc<ValidatedCatalog>>>>,
    refresh_lock: Arc<futures::lock::Mutex<()>>,
    provenance_lock: Arc<futures::lock::Mutex<()>>,
}

impl RpExtensionCatalogClient {
    pub(crate) fn new(
        extensions_dir: &Path,
        fs: Arc<dyn Fs>,
        http_client: Arc<HttpClientWithUrl>,
    ) -> Self {
        Self {
            fs,
            http_client,
            state_path: extensions_dir.join("rp-catalog-state.json"),
            provenance_path: extensions_dir.join("rp-installed-provenance.json"),
            catalog: Arc::new(Mutex::new(None)),
            refresh_lock: Arc::new(futures::lock::Mutex::new(())),
            provenance_lock: Arc::new(futures::lock::Mutex::new(())),
        }
    }

    async fn accepted_catalog(&self) -> Result<Arc<ValidatedCatalog>> {
        if let Some(catalog) = self
            .catalog
            .lock()
            .map_err(|_| anyhow::anyhow!("RP catalog lock poisoned"))?
            .clone()
        {
            return Ok(catalog);
        }
        self.refresh().await
    }

    async fn refresh(&self) -> Result<Arc<ValidatedCatalog>> {
        let _refresh = self.refresh_lock.lock().await;
        let digest_bytes = get_bounded(
            &self.http_client,
            CATALOG_DIGEST_URL,
            RequestKind::Catalog,
            256,
        )
        .await?;
        let expected_digest = parse_digest_file(&digest_bytes)?;
        let catalog_bytes = get_bounded(
            &self.http_client,
            CATALOG_URL,
            RequestKind::Catalog,
            MAX_CATALOG_BYTES,
        )
        .await?;
        let actual_digest = sha256_hex(&catalog_bytes);
        ensure!(
            actual_digest == expected_digest,
            "RP catalog digest does not match exact catalog bytes"
        );

        let catalog = Arc::new(validate_catalog(&catalog_bytes, actual_digest.clone())?);
        self.accept_revision(&catalog).await?;
        replace_accepted_catalog(&self.catalog, catalog.clone())?;
        Ok(catalog)
    }

    async fn accept_revision(&self, catalog: &ValidatedCatalog) -> Result<()> {
        let previous = match self.fs.load(&self.state_path).await {
            Ok(value) => Some(
                serde_json::from_str::<PersistedCatalogState>(&value)
                    .context("invalid persisted RP catalog state")?,
            ),
            Err(_error) if self.fs.metadata(&self.state_path).await?.is_none() => None,
            Err(error) => return Err(error).context("reading persisted RP catalog state"),
        };
        validate_revision(previous.as_ref(), catalog.revision, &catalog.digest)?;
        let state = PersistedCatalogState {
            revision: catalog.revision,
            digest: catalog.digest.clone(),
        };
        self.fs
            .atomic_write(self.state_path.clone(), serde_json::to_string(&state)?)
            .await
            .context("persisting accepted RP catalog revision")
    }

    pub(crate) async fn list(
        &self,
        search: Option<&str>,
        provides: Option<&BTreeSet<ExtensionProvides>>,
    ) -> Result<Vec<ExtensionMetadata>> {
        let catalog = if search.is_none_or(str::is_empty) {
            self.refresh().await?
        } else {
            self.accepted_catalog().await?
        };
        let search = search.map(str::to_ascii_lowercase);
        Ok(catalog
            .data
            .iter()
            .filter(|extension| {
                provides.is_none_or(|required| {
                    required
                        .iter()
                        .all(|value| extension.manifest.provides.contains(value))
                })
            })
            .filter(|extension| {
                search.as_ref().is_none_or(|search| {
                    extension.id.to_ascii_lowercase().contains(search)
                        || extension
                            .manifest
                            .name
                            .to_ascii_lowercase()
                            .contains(search)
                        || extension
                            .manifest
                            .description
                            .as_ref()
                            .is_some_and(|value| value.to_ascii_lowercase().contains(search))
                        || extension
                            .manifest
                            .authors
                            .iter()
                            .any(|value| value.to_ascii_lowercase().contains(search))
                })
            })
            .cloned()
            .collect())
    }

    pub(crate) async fn versions(&self, id: &str) -> Result<Vec<ExtensionMetadata>> {
        let catalog = self.accepted_catalog().await?;
        installable_versions(&catalog, id)
            .with_context(|| format!("extension {id} is absent from the RP catalog"))
    }

    pub(crate) async fn latest(&self, id: &str) -> Result<ExtensionMetadata> {
        self.refresh()
            .await?
            .data
            .iter()
            .find(|extension| extension.id.as_ref() == id)
            .cloned()
            .with_context(|| format!("extension {id} is absent from RP Extensions"))
    }

    pub(crate) async fn updates(
        &self,
        installed: &[(Arc<str>, Arc<str>)],
    ) -> Result<Vec<ExtensionMetadata>> {
        let catalog = self.refresh().await?;
        let provenance = self.load_provenance().await?;
        let mut updates = Vec::new();
        for (id, installed_version) in installed {
            let Some(installed_provenance) = provenance.installed.get(id.as_ref()) else {
                continue;
            };
            let Some(latest) = catalog.data.iter().find(|entry| entry.id == *id) else {
                continue;
            };
            let Some(package) = catalog
                .packages
                .get(&format!("{}@{}", latest.id, latest.manifest.version))
            else {
                continue;
            };
            if !same_authority(installed_provenance.authority, package.authority) {
                continue;
            }
            if is_strictly_newer(installed_version, &latest.manifest.version)
                .with_context(|| format!("comparing installed extension {id} version"))?
            {
                updates.push(latest.clone());
            }
        }

        Ok(updates)
    }

    pub(crate) async fn package(
        &self,
        id: &str,
        version: &str,
    ) -> Result<(RpPackage, CatalogIdentity)> {
        let catalog = self.accepted_catalog().await?;
        let package = catalog
            .packages
            .get(&format!("{id}@{version}"))
            .with_context(|| format!("{id}@{version} is not installable from the RP catalog"))?
            .clone();
        Ok((
            package,
            CatalogIdentity {
                revision: catalog.revision,
                digest: catalog.digest.clone(),
            },
        ))
    }

    pub(crate) async fn authorize(
        &self,
        package: &RpPackage,
        replacing_existing: bool,
    ) -> Result<()> {
        let provenance = self.load_provenance().await?;
        match provenance.installed.get(&package.id) {
            Some(installed) if replacing_existing => ensure!(
                installed.authority == package.authority,
                "refusing silent cross-authority extension update"
            ),
            Some(_) => {}
            None if replacing_existing => bail!(
                "existing extension has no RP authority provenance; uninstall it before reinstalling from RP Extensions"
            ),
            None => {}
        }
        Ok(())
    }

    pub(crate) async fn record_install(
        &self,
        package: &RpPackage,
        catalog: &CatalogIdentity,
    ) -> Result<()> {
        let _provenance = self.provenance_lock.lock().await;
        let mut provenance = self.load_provenance().await?;
        provenance.installed.insert(
            package.id.clone(),
            InstalledProvenance {
                authority: package.authority,
                version: package.version.clone(),
                source_repository: package.source_repository.clone(),
                source_revision: package.source_revision.clone(),
                catalog_revision: catalog.revision,
                catalog_digest: catalog.digest.clone(),
            },
        );
        self.fs
            .atomic_write(
                self.provenance_path.clone(),
                serde_json::to_string(&provenance)?,
            )
            .await
            .context("persisting RP extension provenance")
    }

    pub(crate) async fn has_recorded_install(&self, id: &str, version: &str) -> Result<bool> {
        Ok(self
            .load_provenance()
            .await?
            .installed
            .get(id)
            .is_some_and(|installed| installed.version == version))
    }

    pub(crate) async fn forget_install(&self, id: &str) -> Result<()> {
        let _provenance = self.provenance_lock.lock().await;
        let mut provenance = self.load_provenance().await?;
        if provenance.installed.remove(id).is_some() {
            self.fs
                .atomic_write(
                    self.provenance_path.clone(),
                    serde_json::to_string(&provenance)?,
                )
                .await
                .context("persisting RP extension provenance")?;
        }
        Ok(())
    }

    async fn load_provenance(&self) -> Result<InstalledProvenanceFile> {
        match self.fs.load(&self.provenance_path).await {
            Ok(value) => serde_json::from_str(&value).context("invalid RP extension provenance"),
            Err(_error) if self.fs.metadata(&self.provenance_path).await?.is_none() => {
                Ok(InstalledProvenanceFile::default())
            }
            Err(error) => Err(error).context("reading RP extension provenance"),
        }
    }

    pub(crate) async fn download(&self, package: &RpPackage) -> Result<Vec<u8>> {
        let bytes = get_bounded(
            &self.http_client,
            &package.archive_url,
            RequestKind::Archive(package.authority),
            package.archive_size,
        )
        .await?;
        ensure!(
            bytes.len() as u64 == package.archive_size,
            "RP extension archive size mismatch"
        );
        ensure!(
            sha256_hex(&bytes) == package.archive_sha256,
            "RP extension archive SHA-256 mismatch"
        );
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PackageAuthority {
    Upstream,
    Rp,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RpPackage {
    pub id: String,
    pub version: String,
    pub authority: PackageAuthority,
    pub schema_version: i32,
    pub wasm_api_version: Option<String>,
    pub registry_revision: String,
    pub source_repository: String,
    pub source_revision: String,
    pub archive_url: String,
    pub archive_size: u64,
    pub archive_sha256: String,
}

#[derive(Clone)]
pub(crate) struct CatalogIdentity {
    revision: u64,
    digest: String,
}

struct ValidatedCatalog {
    revision: u64,
    digest: String,
    data: Vec<ExtensionMetadata>,
    versions: BTreeMap<String, Vec<ExtensionMetadata>>,
    packages: BTreeMap<String, RpPackage>,
}

fn installable_versions(catalog: &ValidatedCatalog, id: &str) -> Option<Vec<ExtensionMetadata>> {
    catalog.versions.get(id).map(|versions| {
        versions
            .iter()
            .filter(|version| {
                catalog
                    .packages
                    .contains_key(&format!("{}@{}", version.id, version.manifest.version))
            })
            .cloned()
            .collect()
    })
}

fn same_authority(installed: PackageAuthority, candidate: PackageAuthority) -> bool {
    installed == candidate
}

fn is_strictly_newer(installed: &str, candidate: &str) -> Result<bool> {
    let installed = Version::parse(installed).context("installed version is invalid")?;
    let candidate = Version::parse(candidate).context("candidate version is invalid")?;
    Ok(candidate > installed)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogDocument {
    schema_version: u32,
    channel: String,
    label: String,
    generated_at: String,
    snapshot_revision: String,
    snapshot_taken_at: String,
    source: CatalogSource,
    integrity: CatalogIntegrity,
    entry_count: usize,
    installable_entry_count: usize,
    upstream_entry_count: usize,
    published_upstream_entry_count: usize,
    entries_sha256: String,
    additions: Vec<Addition>,
    source_entries: Vec<SourceEntry>,
    unavailable_source_entries: Vec<UnavailableSourceEntry>,
    revocations: Vec<serde_json::Value>,
    yanks: Vec<serde_json::Value>,
    data: Vec<CatalogMetadata>,
    versions: BTreeMap<String, Vec<CatalogMetadata>>,
    packages: Vec<RpPackage>,
    package_index: BTreeMap<String, RpPackage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogSource {
    fork_repository: String,
    fork_revision: String,
    upstream_repository: String,
    upstream_revision: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogIntegrity {
    catalog_digest_algorithm: String,
    catalog_digest_url: String,
    authorities: Authorities,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Authorities {
    upstream: AuthorityDescription,
    rp: AuthorityDescription,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorityDescription {
    initial_hosts: Vec<String>,
    final_hosts: Vec<String>,
    owner: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceEntry {
    id: String,
    version: String,
    source_repository: String,
    source_revision: String,
    available: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Addition {
    id: String,
    version: String,
    source_repository: String,
    source_revision: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnavailableSourceEntry {
    id: String,
    version: String,
    reason: String,
    published_versions: Vec<String>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogMetadata {
    id: Arc<str>,
    name: String,
    version: Arc<str>,
    description: Option<String>,
    authors: Vec<String>,
    repository: String,
    schema_version: Option<i32>,
    wasm_api_version: Option<String>,
    #[serde(default)]
    provides: BTreeSet<ExtensionProvides>,
    published_at: chrono::DateTime<chrono::Utc>,
    download_count: u64,
}

impl From<CatalogMetadata> for ExtensionMetadata {
    fn from(value: CatalogMetadata) -> Self {
        Self {
            id: value.id,
            manifest: ExtensionApiManifest {
                name: value.name,
                version: value.version,
                description: value.description,
                authors: value.authors,
                repository: value.repository,
                schema_version: value.schema_version,
                wasm_api_version: value.wasm_api_version,
                provides: value.provides,
            },
            published_at: value.published_at,
            download_count: value.download_count,
        }
    }
}

fn validate_catalog(bytes: &[u8], digest: String) -> Result<ValidatedCatalog> {
    let document: CatalogDocument =
        serde_json::from_slice(bytes).context("invalid RP catalog JSON")?;
    ensure!(
        document.schema_version == 1,
        "unsupported RP catalog schema"
    );
    ensure!(
        document.channel == "rp-stable",
        "unexpected RP catalog channel"
    );
    ensure!(
        document.label == "RP Extensions",
        "unexpected RP catalog label"
    );
    ensure!(
        document
            .generated_at
            .parse::<chrono::DateTime<chrono::Utc>>()
            .is_ok()
            && document
                .snapshot_taken_at
                .parse::<chrono::DateTime<chrono::Utc>>()
                .is_ok(),
        "RP catalog timestamps are invalid"
    );
    let revision = document
        .snapshot_revision
        .parse::<u64>()
        .context("invalid RP catalog snapshot revision")?;
    ensure!(
        document.source.fork_repository == "https://github.com/JonathonRP/extensions"
            && document.source.upstream_repository
                == "https://github.com/zed-industries/extensions",
        "unexpected RP catalog source repositories"
    );
    validate_sha(&document.source.fork_revision, "fork revision")?;
    validate_sha(&document.source.upstream_revision, "upstream revision")?;
    ensure!(
        document.integrity.catalog_digest_algorithm == "sha256"
            && document.integrity.catalog_digest_url == CATALOG_DIGEST_URL,
        "unexpected RP catalog integrity declaration"
    );
    validate_authorities(&document.integrity.authorities)?;
    ensure!(
        document.revocations.is_empty() && document.yanks.is_empty(),
        "RP catalog contains revocations or yanks; refusing the snapshot fail-closed"
    );
    ensure!(
        document.entry_count == document.source_entries.len(),
        "RP catalog source entry count mismatch"
    );
    ensure!(
        document.installable_entry_count == document.packages.len()
            && document.installable_entry_count == document.data.len(),
        "RP catalog installable count mismatch"
    );
    ensure!(
        document.upstream_entry_count + document.additions.len() == document.entry_count,
        "RP catalog upstream/addition counts mismatch"
    );

    let mut source_keys = HashSet::default();
    let mut source_by_id = HashMap::default();
    for source in &document.source_entries {
        validate_id_version(&source.id, &source.version)?;
        validate_source_repository(&source.source_repository)?;
        validate_sha(&source.source_revision, "source revision")?;
        ensure!(
            source_keys.insert(format!("{}@{}", source.id, source.version)),
            "duplicate RP catalog source key"
        );
        ensure!(
            source_by_id.insert(source.id.as_str(), source).is_none(),
            "duplicate RP catalog source id"
        );
    }
    let mut entries_json = serde_json::to_vec(&document.source_entries)?;
    entries_json.push(b'\n');
    ensure!(
        sha256_hex(&entries_json) == document.entries_sha256,
        "RP catalog deterministic entries digest mismatch"
    );

    let additions = document
        .additions
        .iter()
        .map(|addition| {
            validate_id_version(&addition.id, &addition.version)?;
            validate_source_repository(&addition.source_repository)?;
            validate_sha(&addition.source_revision, "addition source revision")?;
            Ok((addition.id.as_str(), addition))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    ensure!(
        additions.len() == document.additions.len(),
        "duplicate RP catalog addition"
    );
    let unavailable = document
        .unavailable_source_entries
        .iter()
        .map(|entry| {
            ensure!(
                entry.reason == "not-published-by-upstream",
                "unexpected unavailable source reason"
            );
            ensure!(
                !entry.id.is_empty() && !entry.version.is_empty(),
                "invalid unavailable source entry"
            );
            let _ = &entry.published_versions;
            Ok((format!("{}@{}", entry.id, entry.version), entry))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    ensure!(
        unavailable.len() == document.unavailable_source_entries.len(),
        "duplicate unavailable RP source entry"
    );
    let unavailable_count = document
        .source_entries
        .iter()
        .filter(|entry| !entry.available)
        .count();
    ensure!(
        unavailable_count == unavailable.len()
            && document.published_upstream_entry_count + unavailable_count
                == document.upstream_entry_count,
        "RP catalog unavailable source count mismatch"
    );

    let mut metadata_by_key = HashMap::default();
    for metadata in &document.data {
        validate_metadata(metadata)?;
        ensure!(
            metadata_by_key
                .insert(format!("{}@{}", metadata.id, metadata.version), metadata)
                .is_none(),
            "duplicate RP catalog metadata key"
        );
    }
    ensure!(
        document.versions.len() == document.source_entries.len(),
        "RP catalog version history is incomplete"
    );
    for (id, versions) in &document.versions {
        let source = source_by_id
            .get(id.as_str())
            .context("version history has no source entry")?;
        let mut version_keys = HashSet::default();
        for metadata in versions {
            validate_metadata(metadata)?;
            ensure!(metadata.id.as_ref() == id, "version history id mismatch");
            ensure!(
                version_keys.insert(metadata.version.clone()),
                "duplicate version history entry"
            );
        }
        if source.available {
            ensure!(
                versions
                    .iter()
                    .any(|metadata| metadata.version.as_ref() == source.version),
                "version history omits installable source version"
            );
        }
    }

    let mut packages = BTreeMap::new();
    for package in document.packages {
        validate_package(&package, &document.source, &additions)?;
        let key = format!("{}@{}", package.id, package.version);
        let source = source_by_id
            .get(package.id.as_str())
            .context("package has no source entry")?;
        ensure!(
            source.available,
            "unavailable source has an installable package"
        );
        ensure!(
            source.version == package.version
                && source.source_repository == package.source_repository
                && source.source_revision == package.source_revision,
            "package/source provenance mismatch"
        );
        ensure!(
            metadata_by_key.contains_key(&key),
            "package has no matching catalog metadata"
        );
        let metadata = metadata_by_key[&key];
        ensure!(
            metadata.schema_version == Some(package.schema_version)
                && metadata.wasm_api_version == package.wasm_api_version,
            "package compatibility metadata mismatch"
        );
        ensure!(
            packages.insert(key, package).is_none(),
            "duplicate RP catalog package"
        );
    }
    ensure!(
        packages == document.package_index,
        "RP catalog package_index does not exactly match packages"
    );
    for source in &document.source_entries {
        let key = format!("{}@{}", source.id, source.version);
        ensure!(
            source.available == packages.contains_key(&key),
            "RP catalog installable completeness mismatch"
        );
        ensure!(
            source.available || unavailable.contains_key(&key),
            "unavailable source lacks reason record"
        );
    }

    Ok(ValidatedCatalog {
        revision,
        digest,
        data: document.data.into_iter().map(Into::into).collect(),
        versions: document
            .versions
            .into_iter()
            .map(|(id, versions)| (id, versions.into_iter().map(Into::into).collect()))
            .collect(),
        packages,
    })
}

fn validate_authorities(authorities: &Authorities) -> Result<()> {
    ensure!(
        authorities.upstream.initial_hosts == [UPSTREAM_API_HOST]
            && authorities.upstream.final_hosts == [UPSTREAM_ARCHIVE_HOST]
            && authorities.upstream.owner == "Zed Industries",
        "unexpected upstream package authority"
    );
    ensure!(
        authorities.rp.initial_hosts == [RP_CATALOG_HOST]
            && authorities.rp.final_hosts == [RP_CATALOG_HOST]
            && authorities.rp.owner == "JonathonRP",
        "unexpected RP package authority"
    );
    Ok(())
}

fn validate_metadata(metadata: &CatalogMetadata) -> Result<()> {
    validate_id_version(&metadata.id, &metadata.version)?;
    ensure!(
        !metadata.name.is_empty(),
        "extension metadata name is empty"
    );
    ensure!(
        metadata.schema_version.is_some_and(|version| version >= 0),
        "extension metadata schema version is missing or invalid"
    );
    Ok(())
}

fn validate_package(
    package: &RpPackage,
    source: &CatalogSource,
    additions: &HashMap<&str, &Addition>,
) -> Result<()> {
    validate_id_version(&package.id, &package.version)?;
    Version::parse(&package.version).context("installable extension version is not SemVer")?;
    validate_sha(&package.registry_revision, "registry revision")?;
    validate_sha(&package.source_revision, "source revision")?;
    validate_sha256(&package.archive_sha256, "archive digest")?;
    validate_source_repository(&package.source_repository)?;
    ensure!(package.archive_size > 0, "extension archive size is zero");
    ensure!(
        package.archive_size <= MAX_ARCHIVE_COMPRESSED_BYTES,
        "extension archive exceeds global compressed size limit"
    );
    let addition = additions.get(package.id.as_str());
    match package.authority {
        PackageAuthority::Upstream => {
            ensure!(addition.is_none(), "RP addition uses upstream authority");
            ensure!(
                package.registry_revision == source.upstream_revision,
                "upstream package registry revision mismatch"
            );
            validate_url(&package.archive_url, UPSTREAM_API_HOST)?;
            ensure!(
                package.archive_url
                    == format!(
                        "https://{UPSTREAM_API_HOST}/extensions/{}/{}/download",
                        package.id, package.version
                    ),
                "unexpected upstream archive path"
            );
        }
        PackageAuthority::Rp => {
            let addition = addition.context("RP-authority package is not a declared addition")?;
            ensure!(
                package.registry_revision == source.fork_revision
                    && package.version == addition.version
                    && package.source_repository == addition.source_repository
                    && package.source_revision == addition.source_revision,
                "RP addition provenance mismatch"
            );
            validate_url(&package.archive_url, RP_CATALOG_HOST)?;
            ensure!(
                package.archive_url
                    == format!(
                        "https://{RP_CATALOG_HOST}/extensions/rp-catalog/v1/extensions/{}/{}/archive.tar.gz",
                        package.id, package.version
                    ),
                "unexpected RP archive path"
            );
        }
    }
    Ok(())
}

fn validate_url(value: &str, expected_host: &str) -> Result<Url> {
    let url = Url::parse(value).context("invalid package URL")?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some(expected_host)
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "package URL violates its authority allowlist"
    );
    Ok(url)
}

fn validate_id_version(id: &str, version: &str) -> Result<()> {
    validate_catalog_component(id, false).context("invalid extension id")?;
    validate_catalog_component(version, true).context("invalid extension version")?;
    Ok(())
}

fn validate_catalog_component(value: &str, allow_plus: bool) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value != "."
            && value != ".."
            && !value.bytes().all(|byte| byte == b'.')
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b'-' | b'_' | b'.')
                    || (allow_plus && byte == b'+')
            }),
        "unsafe catalog path component"
    );
    Ok(())
}

fn validate_sha(value: &str, label: &str) -> Result<()> {
    ensure!(
        value.len() == 40
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
        "invalid {label}"
    );
    Ok(())
}

fn validate_source_repository(value: &str) -> Result<()> {
    let url = Url::parse(value).context("invalid source repository URL")?;
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "source repository URL is not an exact uncredentialed HTTPS URL"
    );
    Ok(())
}

fn validate_sha256(value: &str, label: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
        "invalid {label}"
    );
    Ok(())
}

fn parse_digest_file(bytes: &[u8]) -> Result<String> {
    let value = std::str::from_utf8(bytes).context("RP catalog digest is not UTF-8")?;
    let mut fields = value.split_ascii_whitespace();
    let digest = fields.next().context("RP catalog digest is empty")?;
    ensure!(
        fields.next() == Some("catalog.json") && fields.next().is_none(),
        "invalid RP catalog digest file"
    );
    validate_sha256(digest, "catalog digest")?;
    Ok(digest.to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Copy)]
enum RequestKind {
    Catalog,
    Archive(PackageAuthority),
}

async fn get_bounded(
    client: &Arc<HttpClientWithUrl>,
    initial_url: &str,
    kind: RequestKind,
    limit: u64,
) -> Result<Vec<u8>> {
    let mut url = validate_request_url(initial_url, kind, true)?;
    for redirects in 0..=MAX_REDIRECTS {
        let request = Request::builder()
            .uri(url.as_str())
            .header(http_client::http::header::ACCEPT_ENCODING, "identity")
            .follow_redirects(RedirectPolicy::NoFollow)
            .body(AsyncBody::default())?;
        let mut response = client.send(request).await?;
        if matches!(
            response.status(),
            StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::SEE_OTHER
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT
        ) {
            ensure!(
                redirects < MAX_REDIRECTS,
                "RP request redirect limit exceeded"
            );
            let location = response
                .headers()
                .get(http_client::http::header::LOCATION)
                .context("RP request redirect omitted Location")?
                .to_str()
                .context("RP request redirect Location is not text")?;
            url = validate_request_url(
                url.join(location)
                    .context("invalid RP redirect URL")?
                    .as_str(),
                kind,
                false,
            )?;
            continue;
        }
        ensure!(
            response.status() == StatusCode::OK,
            "RP request failed with status {}",
            response.status()
        );
        if let Some(content_length) = response
            .headers()
            .get(http_client::http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
        {
            ensure!(content_length <= limit, "RP response exceeds size limit");
        }
        let mut body = Vec::new();
        response
            .body_mut()
            .take(limit.saturating_add(1))
            .read_to_end(&mut body)
            .await?;
        ensure!(body.len() as u64 <= limit, "RP response exceeds size limit");
        return Ok(body);
    }
    unreachable!()
}

fn validate_request_url(value: &str, kind: RequestKind, initial: bool) -> Result<Url> {
    let url = Url::parse(value).context("invalid RP request URL")?;
    ensure!(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none(),
        "RP request requires uncredentialed HTTPS"
    );
    let host = url.host_str().context("RP request URL has no host")?;
    match kind {
        RequestKind::Catalog => ensure!(
            host == RP_CATALOG_HOST
                && (!initial
                    || matches!(value, CATALOG_URL | CATALOG_DIGEST_URL | CATALOG_SCHEMA_URL)),
            "RP catalog URL is outside the pinned endpoint"
        ),
        RequestKind::Archive(PackageAuthority::Upstream) => ensure!(
            (initial && host == UPSTREAM_API_HOST) || (!initial && host == UPSTREAM_ARCHIVE_HOST),
            "upstream archive URL is outside its authority allowlist"
        ),
        RequestKind::Archive(PackageAuthority::Rp) => ensure!(
            host == RP_CATALOG_HOST,
            "RP archive URL is outside its authority allowlist"
        ),
    }
    Ok(url)
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InstalledProvenanceFile {
    #[serde(default)]
    installed: BTreeMap<String, InstalledProvenance>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InstalledProvenance {
    authority: PackageAuthority,
    version: String,
    source_repository: String,
    source_revision: String,
    catalog_revision: u64,
    catalog_digest: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedCatalogState {
    revision: u64,
    digest: String,
}

fn validate_revision(
    previous: Option<&PersistedCatalogState>,
    revision: u64,
    digest: &str,
) -> Result<()> {
    if let Some(previous) = previous {
        ensure!(
            revision >= previous.revision,
            "RP catalog rollback rejected: revision {revision} is below {}",
            previous.revision
        );
        ensure!(
            revision != previous.revision || digest == previous.digest,
            "RP catalog revision has conflicting identity"
        );
    }
    Ok(())
}

fn replace_accepted_catalog(
    slot: &Mutex<Option<Arc<ValidatedCatalog>>>,
    next: Arc<ValidatedCatalog>,
) -> Result<()> {
    let mut slot = slot
        .lock()
        .map_err(|_| anyhow::anyhow!("RP catalog lock poisoned"))?;
    if let Some(current) = slot.as_ref() {
        let previous = PersistedCatalogState {
            revision: current.revision,
            digest: current.digest.clone(),
        };
        validate_revision(Some(&previous), next.revision, &next.digest)?;
    }
    *slot = Some(next);
    Ok(())
}

pub(crate) async fn validate_archive_paths(bytes: &[u8]) -> Result<()> {
    validate_archive_paths_with_limits(
        bytes,
        MAX_ARCHIVE_FILE_BYTES,
        MAX_ARCHIVE_UNCOMPRESSED_BYTES,
    )
    .await
}

async fn validate_archive_paths_with_limits(
    bytes: &[u8],
    max_file_bytes: u64,
    max_uncompressed_bytes: u64,
) -> Result<()> {
    let decoder =
        async_compression::futures::bufread::GzipDecoder::new(futures::io::BufReader::new(bytes));
    let archive = async_tar::Archive::new(decoder);
    let mut entries = archive.entries().context("reading extension archive")?;
    let mut seen = HashSet::default();
    let mut seen_folded = HashSet::default();
    let mut seen_types: BTreeMap<String, bool> = BTreeMap::new();
    let mut count = 0usize;
    let mut total_size = 0u64;
    while let Some(entry) = entries.next().await {
        let entry = entry.context("reading extension archive entry")?;
        count += 1;
        ensure!(
            count <= MAX_ARCHIVE_ENTRIES,
            "extension archive has too many entries"
        );
        let kind = entry.header().entry_type();
        ensure!(
            kind.is_file() || kind.is_dir(),
            "extension archive contains a link or special file"
        );
        let path = entry.path().context("invalid archive entry path")?;
        if kind.is_dir() && matches!(path.to_string_lossy().as_ref(), "." | "./") {
            continue;
        }
        let normalized_path = PathBuf::from(path.to_string_lossy().into_owned());
        let normalized = validate_archive_path(&normalized_path)?;
        let normalized_text = normalized
            .to_str()
            .context("extension archive path is not UTF-8")?
            .replace('\\', "/");
        ensure!(
            seen.insert(normalized_text.clone()),
            "duplicate archive path"
        );
        ensure!(
            seen_folded.insert(normalized_text.to_ascii_lowercase()),
            "case-colliding archive path"
        );
        let folded = normalized_text.to_ascii_lowercase();
        let mut parent = String::new();
        for component in folded
            .split('/')
            .take(folded.split('/').count().saturating_sub(1))
        {
            if !parent.is_empty() {
                parent.push('/');
            }
            parent.push_str(component);
            ensure!(
                seen_types.get(&parent).copied() != Some(true),
                "archive path is nested below a file"
            );
        }
        if kind.is_file() {
            let descendant_prefix = format!("{folded}/");
            ensure!(
                seen_types
                    .range(descendant_prefix.clone()..)
                    .next()
                    .is_none_or(|(path, _)| !path.starts_with(&descendant_prefix)),
                "archive file replaces an existing directory"
            );
        }
        seen_types.insert(folded, kind.is_file());
        if kind.is_file() {
            let remaining_total = max_uncompressed_bytes
                .checked_sub(total_size)
                .context("extension archive exceeds uncompressed size limit")?;
            let read_limit = max_file_bytes.min(remaining_total).saturating_add(1);
            let mut measured_size = 0u64;
            let mut buffer = [0u8; 64 * 1024];
            let mut limited = entry.take(read_limit);
            loop {
                let read = limited.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                measured_size = measured_size
                    .checked_add(read as u64)
                    .context("extension archive file size overflow")?;
            }
            ensure!(
                measured_size <= max_file_bytes,
                "extension archive file exceeds size limit"
            );
            total_size = total_size
                .checked_add(measured_size)
                .context("extension archive size overflow")?;
            ensure!(
                total_size <= max_uncompressed_bytes,
                "extension archive exceeds uncompressed size limit"
            );
        }
    }
    Ok(())
}

fn validate_archive_path(path: &Path) -> Result<PathBuf> {
    ensure!(!path.as_os_str().is_empty(), "empty archive path");
    let raw = path
        .to_str()
        .context("extension archive path is not UTF-8")?;
    ensure!(!raw.contains('\\'), "archive path contains a backslash");
    let mut normalized = PathBuf::new();
    let mut depth = 0usize;
    for component in path.components() {
        match component {
            Component::CurDir if normalized.as_os_str().is_empty() => {}
            Component::Normal(value) => {
                let value = value.to_str().context("archive path is not UTF-8")?;
                validate_windows_component(value)?;
                normalized.push(value);
                depth += 1;
                ensure!(depth <= MAX_ARCHIVE_DEPTH, "archive path is too deep");
            }
            _ => bail!("archive path is absolute or traverses a parent"),
        }
    }
    ensure!(
        !normalized.as_os_str().is_empty(),
        "empty normalized archive path"
    );
    Ok(normalized)
}

fn validate_windows_component(value: &str) -> Result<()> {
    ensure!(
        !value.ends_with(['.', ' ']) && !value.contains(':'),
        "archive path is unsafe on Windows"
    );
    let stem = value
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let full = value.to_ascii_uppercase();
    ensure!(
        !matches!(full.as_str(), "CONIN$" | "CONOUT$" | "CLOCK$")
            && !matches!(
                stem.as_str(),
                "CON"
                    | "PRN"
                    | "AUX"
                    | "NUL"
                    | "COM0"
                    | "COM1"
                    | "COM2"
                    | "COM3"
                    | "COM4"
                    | "COM5"
                    | "COM6"
                    | "COM7"
                    | "COM8"
                    | "COM9"
                    | "LPT0"
                    | "LPT1"
                    | "LPT2"
                    | "LPT3"
                    | "LPT4"
                    | "LPT5"
                    | "LPT6"
                    | "LPT7"
                    | "LPT8"
                    | "LPT9"
                    | "COM¹"
                    | "COM²"
                    | "COM³"
                    | "LPT¹"
                    | "LPT²"
                    | "LPT³"
            ),
        "archive path uses a Windows reserved name"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn catalog_fixture() -> Value {
        let source_entries = json!([{
            "id": "example",
            "version": "1.0.0",
            "source_repository": "https://github.com/example/example.git",
            "source_revision": "2222222222222222222222222222222222222222",
            "available": true
        }]);
        let mut entries_bytes = serde_json::to_vec(&source_entries).unwrap();
        entries_bytes.push(b'\n');
        let package = json!({
            "id": "example",
            "version": "1.0.0",
            "authority": "upstream",
            "schema_version": 1,
            "wasm_api_version": null,
            "registry_revision": "1111111111111111111111111111111111111111",
            "source_repository": "https://github.com/example/example.git",
            "source_revision": "2222222222222222222222222222222222222222",
            "archive_url": "https://api.zed.dev/extensions/example/1.0.0/download",
            "archive_size": 123,
            "archive_sha256": "3333333333333333333333333333333333333333333333333333333333333333"
        });
        let metadata = json!({
            "id": "example",
            "name": "Example",
            "version": "1.0.0",
            "description": "Example extension",
            "authors": ["Example"],
            "repository": "https://github.com/example/example",
            "schema_version": 1,
            "wasm_api_version": null,
            "provides": ["themes"],
            "published_at": "2026-09-01T00:00:00Z",
            "download_count": 1
        });
        json!({
            "schema_version": 1,
            "channel": "rp-stable",
            "label": "RP Extensions",
            "generated_at": "2026-09-01T00:00:00Z",
            "snapshot_revision": "100",
            "snapshot_taken_at": "2026-09-01T00:00:00Z",
            "source": {
                "fork_repository": "https://github.com/JonathonRP/extensions",
                "fork_revision": "4444444444444444444444444444444444444444",
                "upstream_repository": "https://github.com/zed-industries/extensions",
                "upstream_revision": "1111111111111111111111111111111111111111"
            },
            "integrity": {
                "catalog_digest_algorithm": "sha256",
                "catalog_digest_url": CATALOG_DIGEST_URL,
                "authorities": {
                    "upstream": {
                        "initial_hosts": [UPSTREAM_API_HOST],
                        "final_hosts": [UPSTREAM_ARCHIVE_HOST],
                        "owner": "Zed Industries"
                    },
                    "rp": {
                        "initial_hosts": [RP_CATALOG_HOST],
                        "final_hosts": [RP_CATALOG_HOST],
                        "owner": "JonathonRP"
                    }
                }
            },
            "entry_count": 1,
            "installable_entry_count": 1,
            "upstream_entry_count": 1,
            "published_upstream_entry_count": 1,
            "entries_sha256": sha256_hex(&entries_bytes),
            "additions": [],
            "source_entries": source_entries,
            "unavailable_source_entries": [],
            "revocations": [],
            "yanks": [],
            "data": [metadata],
            "versions": {"example": [metadata]},
            "packages": [package],
            "package_index": {"example@1.0.0": package}
        })
    }

    fn validate_fixture(value: &Value) -> Result<ValidatedCatalog> {
        validate_catalog(
            &serde_json::to_vec_pretty(value).unwrap(),
            "5555555555555555555555555555555555555555555555555555555555555555".to_string(),
        )
    }

    fn gzip_tar(entries: &[(&str, async_tar::EntryType)]) -> Vec<u8> {
        futures::executor::block_on(async {
            let mut tar_bytes = Vec::new();
            let mut archive = async_tar::Builder::new(&mut tar_bytes);
            for (path, entry_type) in entries {
                let content = if entry_type.is_file() {
                    b"contents".as_slice()
                } else {
                    b"".as_slice()
                };
                let mut header = async_tar::Header::new_gnu();
                header.set_entry_type(*entry_type);
                header.set_size(content.len() as u64);
                header.set_cksum();
                archive
                    .append_data(&mut header, *path, content)
                    .await
                    .unwrap();
            }
            archive.into_inner().await.unwrap();
            let mut compressed = Vec::new();
            let mut encoder = async_compression::futures::bufread::GzipEncoder::new(
                futures::io::BufReader::new(tar_bytes.as_slice()),
            );
            encoder.read_to_end(&mut compressed).await.unwrap();
            compressed
        })
    }

    fn gzip_bytes(bytes: &[u8]) -> Vec<u8> {
        futures::executor::block_on(async {
            let mut compressed = Vec::new();
            let mut encoder = async_compression::futures::bufread::GzipEncoder::new(
                futures::io::BufReader::new(bytes),
            );
            encoder.read_to_end(&mut compressed).await.unwrap();
            compressed
        })
    }

    fn pax_size_override_archive() -> Vec<u8> {
        let mut bytes = Vec::new();
        let pax_record = b"9 size=9\n";
        let mut pax_header = async_tar::Header::new_gnu();
        pax_header.set_path("pax").unwrap();
        pax_header.set_entry_type(async_tar::EntryType::XHeader);
        pax_header.set_size(pax_record.len() as u64);
        pax_header.set_cksum();
        bytes.extend_from_slice(pax_header.as_bytes());
        bytes.extend_from_slice(pax_record);
        bytes.resize(bytes.len().next_multiple_of(512), 0);

        let mut file_header = async_tar::Header::new_gnu();
        file_header.set_path("payload").unwrap();
        file_header.set_entry_type(async_tar::EntryType::Regular);
        file_header.set_size(0);
        file_header.set_cksum();
        bytes.extend_from_slice(file_header.as_bytes());
        bytes.extend_from_slice(b"123456789");
        bytes.resize(bytes.len().next_multiple_of(512), 0);
        bytes.resize(bytes.len() + 1024, 0);
        gzip_bytes(&bytes)
    }

    #[test]
    fn digest_file_is_strict() {
        assert_eq!(
            parse_digest_file(
                b"75e684d453f2dfa1074211d6ea6dc49e1c4b3b818d303d0eda4f0cb59e66900b  catalog.json\n"
            )
            .unwrap(),
            "75e684d453f2dfa1074211d6ea6dc49e1c4b3b818d303d0eda4f0cb59e66900b"
        );
        assert!(parse_digest_file(b"abc catalog.json\n").is_err());
        assert!(parse_digest_file(
            b"75e684d453f2dfa1074211d6ea6dc49e1c4b3b818d303d0eda4f0cb59e66900b catalog.json extra"
        )
        .is_err());
    }

    #[test]
    fn request_authorities_are_exact() {
        assert!(validate_request_url(CATALOG_URL, RequestKind::Catalog, true).is_ok());
        assert!(
            validate_request_url(
                "https://api.zed.dev/extensions/example/1.0/download",
                RequestKind::Archive(PackageAuthority::Upstream),
                true,
            )
            .is_ok()
        );
        assert!(
            validate_request_url(
                "https://zed-extensions.nyc3.digitaloceanspaces.com/example.tar.gz",
                RequestKind::Archive(PackageAuthority::Upstream),
                false,
            )
            .is_ok()
        );
        assert!(
            validate_request_url(
                "https://evil.example/archive.tar.gz",
                RequestKind::Archive(PackageAuthority::Upstream),
                false,
            )
            .is_err()
        );
        assert!(
            validate_request_url(
                "https://user@jonathonrp.github.io/extensions/rp-catalog/v1/catalog.json",
                RequestKind::Catalog,
                true,
            )
            .is_err()
        );
    }

    #[test]
    fn windows_paths_are_rejected_portably() {
        for path in [
            "../escape",
            "/absolute",
            "foo\\bar",
            "foo/CON",
            "foo/COM0.txt",
            "foo/LPT0",
            "foo/CONIN$",
            "foo/conout$",
            "foo/CLOCK$",
            "foo/COM¹.txt",
            "foo/LPT³",
            "foo/a:b",
            "foo/trailing.",
        ] {
            assert!(validate_archive_path(Path::new(path)).is_err(), "{path}");
        }
        assert_eq!(
            validate_archive_path(Path::new("./extension.toml")).unwrap(),
            Path::new("extension.toml")
        );
    }

    #[test]
    fn complete_catalog_fixture_is_accepted() {
        let catalog = validate_fixture(&catalog_fixture()).unwrap();
        assert_eq!(catalog.revision, 100);
        assert_eq!(catalog.data[0].id.as_ref(), "example");
    }

    #[test]
    fn catalog_integrity_correspondence_is_fail_closed() {
        let mutations: Vec<Box<dyn Fn(&mut Value)>> = vec![
            Box::new(|catalog| catalog["entry_count"] = json!(2)),
            Box::new(|catalog| catalog["entries_sha256"] = json!("0".repeat(64))),
            Box::new(|catalog| catalog["revocations"] = json!([{"id": "example"}])),
            Box::new(|catalog| {
                catalog["package_index"]["example@1.0.0"]["archive_size"] = json!(124)
            }),
            Box::new(|catalog| {
                catalog["packages"][0]["source_revision"] =
                    json!("9999999999999999999999999999999999999999")
            }),
            Box::new(|catalog| {
                catalog["integrity"]["authorities"]["upstream"]["final_hosts"] =
                    json!(["evil.example"])
            }),
            Box::new(|catalog| catalog["packages"][0]["schema_version"] = json!(0)),
        ];
        for mutate in mutations {
            let mut catalog = catalog_fixture();
            mutate(&mut catalog);
            assert!(validate_fixture(&catalog).is_err());
        }
    }

    #[test]
    fn catalog_duplicates_and_unavailable_packages_are_rejected() {
        let mut duplicate = catalog_fixture();
        let entry = duplicate["source_entries"][0].clone();
        duplicate["source_entries"]
            .as_array_mut()
            .unwrap()
            .push(entry);
        duplicate["entry_count"] = json!(2);
        assert!(validate_fixture(&duplicate).is_err());

        let mut unavailable = catalog_fixture();
        unavailable["source_entries"][0]["available"] = json!(false);
        let mut entries_bytes = serde_json::to_vec(&unavailable["source_entries"]).unwrap();
        entries_bytes.push(b'\n');
        unavailable["entries_sha256"] = json!(sha256_hex(&entries_bytes));
        unavailable["unavailable_source_entries"] = json!([{
            "id": "example",
            "version": "1.0.0",
            "reason": "not-published-by-upstream",
            "published_versions": []
        }]);
        unavailable["published_upstream_entry_count"] = json!(0);
        assert!(validate_fixture(&unavailable).is_err());
    }

    #[test]
    fn dynamic_revision_rejects_rollback_and_equal_conflict() {
        let previous = PersistedCatalogState {
            revision: 100,
            digest: "a".repeat(64),
        };
        assert!(validate_revision(Some(&previous), 99, &"a".repeat(64)).is_err());
        assert!(validate_revision(Some(&previous), 100, &"b".repeat(64)).is_err());
        assert!(validate_revision(Some(&previous), 100, &"a".repeat(64)).is_ok());
        assert!(validate_revision(Some(&previous), 101, &"b".repeat(64)).is_ok());
    }

    #[test]
    fn accepted_snapshot_advances_during_same_process() {
        let slot = Mutex::new(None);
        let first = Arc::new(validate_fixture(&catalog_fixture()).unwrap());
        replace_accepted_catalog(&slot, first).unwrap();

        let mut next_value = catalog_fixture();
        next_value["snapshot_revision"] = json!("101");
        let mut next = validate_fixture(&next_value).unwrap();
        next.digest = "6".repeat(64);
        replace_accepted_catalog(&slot, Arc::new(next)).unwrap();
        assert_eq!(slot.lock().unwrap().as_ref().unwrap().revision, 101);

        let mut rollback = validate_fixture(&catalog_fixture()).unwrap();
        rollback.digest = "7".repeat(64);
        assert!(replace_accepted_catalog(&slot, Arc::new(rollback)).is_err());

        let mut conflict_value = catalog_fixture();
        conflict_value["snapshot_revision"] = json!("101");
        let mut conflict = validate_fixture(&conflict_value).unwrap();
        conflict.digest = "8".repeat(64);
        assert!(replace_accepted_catalog(&slot, Arc::new(conflict)).is_err());
    }

    #[test]
    fn update_selection_only_advances_semver_within_authority() {
        assert!(is_strictly_newer("1.0.0", "1.0.1").unwrap());
        assert!(!is_strictly_newer("1.0.0", "1.0.0").unwrap());
        assert!(!is_strictly_newer("1.0.1", "1.0.0").unwrap());
        assert!(is_strictly_newer("invalid", "1.0.1").is_err());
        assert!(is_strictly_newer("1.0.0", "invalid").is_err());
        assert!(same_authority(PackageAuthority::Rp, PackageAuthority::Rp));
        assert!(!same_authority(
            PackageAuthority::Upstream,
            PackageAuthority::Rp
        ));
    }

    #[test]
    fn version_picker_only_exposes_packaged_versions() {
        let mut catalog = validate_fixture(&catalog_fixture()).unwrap();
        let mut unavailable = catalog.versions["example"][0].clone();
        unavailable.manifest.version = "0.9.0".into();
        catalog
            .versions
            .get_mut("example")
            .unwrap()
            .push(unavailable);

        let versions = installable_versions(&catalog, "example").unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].manifest.version.as_ref(), "1.0.0");
    }

    #[test]
    fn catalog_components_cannot_escape_url_or_install_paths() {
        for value in [".", "..", "...", "a/b", "a\\b", "a:b", "a%b"] {
            assert!(validate_catalog_component(value, true).is_err(), "{value}");
        }
        assert!(validate_catalog_component("v0.0.1", true).is_ok());
        assert!(validate_catalog_component("2025.08.0", true).is_ok());
    }

    #[test]
    fn archive_rejects_special_files_windows_names_and_case_collisions() {
        let safe = gzip_tar(&[
            ("extension.toml", async_tar::EntryType::Regular),
            ("themes/example.json", async_tar::EntryType::Regular),
        ]);
        futures::executor::block_on(validate_archive_paths(&safe)).unwrap();

        let symlink = gzip_tar(&[("link", async_tar::EntryType::Symlink)]);
        assert!(futures::executor::block_on(validate_archive_paths(&symlink)).is_err());

        let windows_reserved = gzip_tar(&[("themes/CON.json", async_tar::EntryType::Regular)]);
        assert!(futures::executor::block_on(validate_archive_paths(&windows_reserved)).is_err());

        let collision = gzip_tar(&[
            ("themes/Foo.json", async_tar::EntryType::Regular),
            ("themes/foo.json", async_tar::EntryType::Regular),
        ]);
        assert!(futures::executor::block_on(validate_archive_paths(&collision)).is_err());
    }

    #[test]
    fn pax_size_override_is_measured_from_decompressed_bytes() {
        let archive = pax_size_override_archive();
        assert!(
            futures::executor::block_on(validate_archive_paths_with_limits(&archive, 8, 64))
                .is_err()
        );
    }

    #[test]
    #[ignore = "requires ZED_RP_CATALOG_FIXTURE"]
    fn validates_live_catalog_fixture_without_pinning_snapshot_identity() {
        let path = std::env::var_os("ZED_RP_CATALOG_FIXTURE")
            .expect("ZED_RP_CATALOG_FIXTURE must point to catalog.json");
        let bytes = std::fs::read(path).unwrap();
        let catalog = validate_catalog(&bytes, sha256_hex(&bytes)).unwrap();
        assert!(
            catalog
                .data
                .iter()
                .any(|entry| entry.id.as_ref() == "pigments-lsp")
        );
        assert_eq!(
            catalog.packages["pigments-lsp@0.3.1"].authority,
            PackageAuthority::Rp
        );
    }

    #[test]
    #[ignore = "requires ZED_RP_CATALOG_FIXTURE and ZED_RP_ARCHIVE_FIXTURE"]
    fn validates_and_stages_live_rp_archive_in_isolation() {
        let catalog_path = std::env::var_os("ZED_RP_CATALOG_FIXTURE")
            .expect("ZED_RP_CATALOG_FIXTURE must point to catalog.json");
        let archive_path = std::env::var_os("ZED_RP_ARCHIVE_FIXTURE")
            .expect("ZED_RP_ARCHIVE_FIXTURE must point to an archive");
        let catalog_bytes = std::fs::read(catalog_path).unwrap();
        let catalog = validate_catalog(&catalog_bytes, sha256_hex(&catalog_bytes)).unwrap();
        let package = &catalog.packages["pigments-lsp@0.3.1"];
        let archive_bytes = std::fs::read(archive_path).unwrap();
        assert_eq!(archive_bytes.len() as u64, package.archive_size);
        assert_eq!(sha256_hex(&archive_bytes), package.archive_sha256);
        futures::executor::block_on(validate_archive_paths(&archive_bytes)).unwrap();

        let staging = tempfile::tempdir().unwrap();
        futures::executor::block_on(async {
            let decoder = async_compression::futures::bufread::GzipDecoder::new(
                futures::io::BufReader::new(archive_bytes.as_slice()),
            );
            async_tar::Archive::new(decoder)
                .unpack(staging.path())
                .await
                .unwrap();
        });
        let manifest: extension::ExtensionManifest = toml::from_str(
            &std::fs::read_to_string(staging.path().join("extension.toml")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.id.as_ref(), package.id);
        assert_eq!(manifest.version.as_ref(), package.version);
        assert_eq!(manifest.schema_version.0 as i32, package.schema_version);
        assert_eq!(
            manifest.lib.version.as_ref().map(ToString::to_string),
            package.wasm_api_version
        );
    }
}
