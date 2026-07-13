use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{AssetRef, MACRO_SCHEMA_VERSION, MacroDefinition};

static STORE_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedRevision {
    pub definition: MacroDefinition,
    pub definition_hash: String,
    pub pinned_assets: Vec<PinnedAsset>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PinnedAsset {
    pub asset: AssetRef,
    pub bytes: Vec<u8>,
}

pub const PACKAGE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq)]
pub struct PackageAsset {
    pub asset: AssetRef,
    pub relative_path: PathBuf,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MacroPackage {
    pub schema_version: u32,
    pub definition: MacroDefinition,
    pub assets: Vec<PackageAsset>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalLimits {
    pub max_bytes_per_run: u64,
    pub max_runs: usize,
}

impl JournalLimits {
    pub const fn new(max_bytes_per_run: u64, max_runs: usize) -> Self {
        Self {
            max_bytes_per_run,
            max_runs,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalKind {
    StateChange,
    Candidate,
    Arbitration,
    Action,
    Error,
    Aggregate,
    Diagnostic,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalRecord {
    pub sequence: u64,
    pub elapsed_ms: u64,
    pub kind: JournalKind,
    pub message: String,
    #[serde(default)]
    pub fields: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalAppendOutcome {
    Written,
    Dropped { diagnostic: String },
}

pub enum JournalOpenOutcome {
    Ready(RunJournal),
    Disabled { diagnostic: String },
}

pub struct RunJournal {
    path: PathBuf,
    file: File,
    max_bytes: u64,
    bytes_written: u64,
    failed: bool,
}

impl RunJournal {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&mut self, record: &JournalRecord) -> JournalAppendOutcome {
        if self.failed {
            return JournalAppendOutcome::Dropped {
                diagnostic: "journal is unavailable; runtime safety remains authoritative"
                    .to_string(),
            };
        }
        let mut bytes = match serde_json::to_vec(record) {
            Ok(bytes) => bytes,
            Err(error) => {
                return JournalAppendOutcome::Dropped {
                    diagnostic: format!("journal serialization failed: {error}"),
                };
            }
        };
        bytes.push(b'\n');
        if self.bytes_written.saturating_add(bytes.len() as u64) > self.max_bytes {
            return JournalAppendOutcome::Dropped {
                diagnostic: "journal byte cap reached".to_string(),
            };
        }
        if let Err(error) = self.file.write_all(&bytes).and_then(|_| self.file.flush()) {
            self.failed = true;
            return JournalAppendOutcome::Dropped {
                diagnostic: format!("journal write failed: {error}"),
            };
        }
        self.bytes_written += bytes.len() as u64;
        JournalAppendOutcome::Written
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PackageManifest {
    schema_version: u32,
    definition: PathBuf,
    assets: Vec<PackageManifestAsset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PackageManifestAsset {
    asset: AssetRef,
    path: PathBuf,
}

const ASSET_IDENTITY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AssetIdentityIndex {
    schema_version: u32,
    bindings: Vec<AssetRef>,
}

impl Default for AssetIdentityIndex {
    fn default() -> Self {
        Self {
            schema_version: ASSET_IDENTITY_SCHEMA_VERSION,
            bindings: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct InstalledAsset {
    asset: AssetRef,
    created_binding: bool,
    created_file: bool,
}

#[derive(Debug, Clone)]
pub struct AssetStore {
    root: PathBuf,
    identity_index: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl AssetStore {
    pub fn put_png(&self, bytes: &[u8]) -> Result<AssetRef> {
        let content_hash = sha256_hex(bytes);
        let asset = AssetRef {
            id: content_hash.clone(),
            revision: 1,
            content_hash,
        };
        self.put_png_revision(asset.clone(), bytes)?;
        Ok(asset)
    }

    pub fn put_png_revision(&self, asset: AssetRef, bytes: &[u8]) -> Result<()> {
        let _guard = lock_store(&self.lock)?;
        self.install_locked(asset, bytes).map(|_| ())
    }

    pub fn read(&self, asset: &AssetRef) -> Result<Vec<u8>> {
        let _guard = lock_store(&self.lock)?;
        self.read_locked(asset)
    }

    fn read_locked(&self, asset: &AssetRef) -> Result<Vec<u8>> {
        let index = self.load_index_locked()?;
        validate_identity_binding(&index.bindings, asset)?;
        let path = self.path_for_hash(&asset.content_hash)?;
        let bytes =
            fs::read(&path).with_context(|| format!("missing asset {}", asset.content_hash))?;
        if sha256_hex(&bytes) != asset.content_hash {
            bail!("corrupt asset {}", asset.content_hash);
        }
        Ok(bytes)
    }

    fn install_locked(&self, asset: AssetRef, bytes: &[u8]) -> Result<InstalledAsset> {
        validate_asset_ref(&asset)?;
        if sha256_hex(bytes) != asset.content_hash {
            bail!("asset content hash does not match bytes");
        }
        fs::create_dir_all(&self.root)?;
        let mut index = self.load_index_locked()?;
        let identity = (asset.id.as_str(), asset.revision);
        if let Some(existing) = index
            .bindings
            .iter()
            .find(|binding| (binding.id.as_str(), binding.revision) == identity)
        {
            if existing.content_hash != asset.content_hash {
                bail!(
                    "immutable asset identity {} revision {} is already bound to {}",
                    asset.id,
                    asset.revision,
                    existing.content_hash
                );
            }
            let path = self.path_for_hash(&asset.content_hash)?;
            let created_file = if path.exists() {
                self.verify_hash_file_locked(&asset.content_hash)?;
                false
            } else {
                matches!(
                    atomic_publish_noclobber(&path, bytes)?,
                    PublishOutcome::Published
                )
            };
            return Ok(InstalledAsset {
                asset,
                created_binding: false,
                created_file,
            });
        }

        let path = self.path_for_hash(&asset.content_hash)?;
        let created_file = matches!(
            atomic_publish_noclobber(&path, bytes)?,
            PublishOutcome::Published
        );
        index.bindings.push(asset.clone());
        if let Err(error) = self.write_index_locked(&index) {
            if created_file {
                let _ = fs::remove_file(&path);
            }
            return Err(error);
        }
        Ok(InstalledAsset {
            asset,
            created_binding: true,
            created_file,
        })
    }

    fn rollback_locked(&self, installed: &[InstalledAsset]) -> Result<()> {
        let mut index = self.load_index_locked()?;
        let created: HashSet<_> = installed
            .iter()
            .filter(|change| change.created_binding)
            .map(|change| (change.asset.id.clone(), change.asset.revision))
            .collect();
        index
            .bindings
            .retain(|binding| !created.contains(&(binding.id.clone(), binding.revision)));
        self.write_index_locked(&index)?;
        for change in installed.iter().rev() {
            if change.created_file {
                let path = self.path_for_hash(&change.asset.content_hash)?;
                if path.exists() {
                    fs::remove_file(path)?;
                }
            }
        }
        Ok(())
    }

    fn load_index_locked(&self) -> Result<AssetIdentityIndex> {
        if !self.identity_index.exists() {
            return Ok(AssetIdentityIndex::default());
        }
        let index: AssetIdentityIndex = serde_json::from_slice(&fs::read(&self.identity_index)?)
            .context("corrupt asset identity index")?;
        if index.schema_version != ASSET_IDENTITY_SCHEMA_VERSION {
            bail!("unsupported asset identity schema {}", index.schema_version);
        }
        validate_identity_set(index.bindings.iter())?;
        Ok(index)
    }

    fn write_index_locked(&self, index: &AssetIdentityIndex) -> Result<()> {
        validate_identity_set(index.bindings.iter())?;
        atomic_write(&self.identity_index, &serde_json::to_vec_pretty(index)?)
    }

    fn verify_hash_file_locked(&self, hash: &str) -> Result<Vec<u8>> {
        let path = self.path_for_hash(hash)?;
        let bytes = fs::read(&path).with_context(|| format!("missing asset {hash}"))?;
        if sha256_hex(&bytes) != hash {
            bail!("corrupt asset {hash}");
        }
        Ok(bytes)
    }

    fn path_for_hash(&self, hash: &str) -> Result<PathBuf> {
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("invalid asset content hash");
        }
        Ok(self.root.join(format!("{hash}.png")))
    }
}

#[derive(Debug, Clone)]
pub struct MacroStore {
    root: PathBuf,
    assets: AssetStore,
    lock: Arc<Mutex<()>>,
    #[cfg(test)]
    fail_import_after_assets: Arc<AtomicBool>,
}

impl MacroStore {
    pub fn open(root: &Path) -> Result<Self> {
        let root = root.join("macro_data");
        fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        let lock = shared_store_lock(&root)?;
        let assets = AssetStore {
            root: root.join("assets"),
            identity_index: root.join("asset_identities.json"),
            lock: Arc::clone(&lock),
        };
        fs::create_dir_all(root.join("definitions"))?;
        fs::create_dir_all(&assets.root)?;
        fs::create_dir_all(root.join("runs"))?;
        let store = Self {
            root,
            assets,
            lock,
            #[cfg(test)]
            fail_import_after_assets: Arc::new(AtomicBool::new(false)),
        };
        {
            let _guard = lock_store(&store.lock)?;
            if !store.assets.identity_index.exists() {
                store
                    .assets
                    .write_index_locked(&AssetIdentityIndex::default())?;
            } else {
                store.assets.load_index_locked()?;
            }
        }
        Ok(store)
    }

    pub fn assets(&self) -> &AssetStore {
        &self.assets
    }

    pub fn save(&self, definition: MacroDefinition) -> Result<SavedRevision> {
        let _guard = lock_store(&self.lock)?;
        self.save_locked(definition)
    }

    fn save_locked(&self, definition: MacroDefinition) -> Result<SavedRevision> {
        if definition.schema_version != MACRO_SCHEMA_VERSION {
            bail!("unsupported macro schema {}", definition.schema_version);
        }
        validate_component("macro ID", &definition.id)?;
        validate_identity_set(referenced_assets(&definition))?;
        let mut pinned_assets = Vec::new();
        let mut pinned_refs = HashSet::new();
        for asset in referenced_assets(&definition) {
            let bytes = self.assets.read_locked(asset)?;
            if pinned_refs.insert(asset.clone()) {
                pinned_assets.push(PinnedAsset {
                    asset: asset.clone(),
                    bytes,
                });
            }
        }

        let bytes = serde_json::to_vec_pretty(&definition)?;
        let saved = SavedRevision {
            definition,
            definition_hash: sha256_hex(&bytes),
            pinned_assets,
        };
        let directory = self.root.join("definitions").join(&saved.definition.id);
        fs::create_dir_all(&directory)?;
        let revision_path = directory.join(format!("{}.json", saved.definition.revision));
        let publication = atomic_publish_noclobber(&revision_path, &bytes).with_context(|| {
            format!(
                "immutable revision {} publication failed",
                saved.definition.revision
            )
        })?;
        let checksum_path = revision_checksum_path(&revision_path)?;
        let checksum_publication =
            match atomic_publish_noclobber(&checksum_path, saved.definition_hash.as_bytes()) {
                Ok(publication) => publication,
                Err(error) => {
                    if matches!(publication, PublishOutcome::Published) {
                        let _ = fs::remove_file(&revision_path);
                    }
                    return Err(error.context("immutable revision checksum publication failed"));
                }
            };
        if let Err(error) = atomic_write(&directory.join("current.json"), &bytes) {
            if matches!(checksum_publication, PublishOutcome::Published) {
                let _ = fs::remove_file(&checksum_path);
            }
            if matches!(publication, PublishOutcome::Published) {
                let _ = fs::remove_file(&revision_path);
            }
            return Err(error.context("current revision publication failed"));
        }
        Ok(saved)
    }

    pub fn validate_package(package_root: &Path) -> Result<MacroPackage> {
        let root = package_root.canonicalize().with_context(|| {
            format!("package folder does not exist: {}", package_root.display())
        })?;
        if !root.is_dir() {
            bail!("package path is not a folder");
        }
        let manifest_path = package_file(&root, Path::new("manifest.json"))?;
        let manifest: PackageManifest = serde_json::from_slice(
            &fs::read(&manifest_path).context("could not read package manifest")?,
        )
        .context("corrupt package manifest JSON")?;
        if manifest.schema_version != PACKAGE_SCHEMA_VERSION {
            bail!("unsupported package schema {}", manifest.schema_version);
        }
        validate_package_relative(&manifest.definition)?;
        for asset in &manifest.assets {
            validate_package_relative(&asset.path)?;
        }

        let definition_path = package_file(&root, &manifest.definition)?;
        let definition: MacroDefinition = serde_json::from_slice(
            &fs::read(&definition_path).context("could not read package definition")?,
        )
        .context("corrupt macro JSON")?;
        if definition.schema_version != MACRO_SCHEMA_VERSION {
            bail!("unsupported macro schema {}", definition.schema_version);
        }
        validate_component("macro ID", &definition.id)?;
        validate_identity_set(referenced_assets(&definition))?;
        validate_identity_set(manifest.assets.iter().map(|entry| &entry.asset))?;

        let mut assets = Vec::with_capacity(manifest.assets.len());
        let mut manifest_refs = HashSet::new();
        for entry in manifest.assets {
            if !manifest_refs.insert(entry.asset.clone()) {
                bail!("duplicate asset entry in package manifest");
            }
            let path = package_file(&root, &entry.path)?;
            let bytes = fs::read(&path)
                .with_context(|| format!("missing package asset {}", entry.asset.content_hash))?;
            if sha256_hex(&bytes) != entry.asset.content_hash {
                bail!("package asset hash mismatch: {}", entry.asset.content_hash);
            }
            assets.push(PackageAsset {
                asset: entry.asset,
                relative_path: entry.path,
                bytes,
            });
        }

        let definition_refs: HashSet<_> = referenced_assets(&definition).cloned().collect();
        if definition_refs != manifest_refs {
            bail!("package asset references do not match definition");
        }

        let package = MacroPackage {
            schema_version: manifest.schema_version,
            definition,
            assets,
        };
        validate_package_memory(&package)?;
        Ok(package)
    }

    pub fn export_package(&self, saved: &SavedRevision, package_root: &Path) -> Result<()> {
        let _guard = lock_store(&self.lock)?;
        validate_identity_set(referenced_assets(&saved.definition))?;
        let parent = package_root
            .parent()
            .context("package destination has no parent")?;
        fs::create_dir_all(parent)?;
        let canonical_parent = parent.canonicalize()?;
        let name = package_root
            .file_name()
            .context("package destination has no folder name")?;
        validate_component("package folder name", &name.to_string_lossy())?;
        let destination = canonical_parent.join(name);
        if destination.exists() {
            bail!("package destination already exists");
        }
        let temporary = tempfile::Builder::new()
            .prefix(".macro-package-")
            .tempdir_in(&canonical_parent)?;
        let staging_root = temporary.path();
        let definition_path = staging_root.join("macro.json");
        let definition_bytes = serde_json::to_vec_pretty(&saved.definition)?;
        atomic_write(&definition_path, &definition_bytes)?;

        let mut manifest_assets = Vec::new();
        let mut seen = HashSet::new();
        for asset in referenced_assets(&saved.definition) {
            if !seen.insert(asset.clone()) {
                continue;
            }
            let bytes = self.assets.read_locked(asset)?;
            let relative_path = PathBuf::from("assets").join(format!("{}.png", asset.content_hash));
            let output_path = staging_root.join(&relative_path);
            atomic_write(&output_path, &bytes)?;
            manifest_assets.push(PackageManifestAsset {
                asset: asset.clone(),
                path: relative_path,
            });
        }
        let manifest = PackageManifest {
            schema_version: PACKAGE_SCHEMA_VERSION,
            definition: PathBuf::from("macro.json"),
            assets: manifest_assets,
        };
        atomic_write(
            &staging_root.join("manifest.json"),
            &serde_json::to_vec_pretty(&manifest)?,
        )?;
        let staged = temporary.keep();
        if destination.exists() {
            let _ = fs::remove_dir_all(&staged);
            bail!("package destination already exists");
        }
        if let Err(error) = fs::rename(&staged, &destination) {
            let _ = fs::remove_dir_all(&staged);
            return Err(error).context("atomic package publication failed");
        }
        sync_directory(&canonical_parent)?;
        Ok(())
    }

    pub fn import_package(&self, package_root: &Path) -> Result<SavedRevision> {
        let package = Self::validate_package(package_root)?;
        self.import_validated_package(package)
    }

    pub fn import_validated_package(&self, mut package: MacroPackage) -> Result<SavedRevision> {
        validate_package_memory(&package)?;
        let _guard = lock_store(&self.lock)?;
        let existing_macro_ids = fs::read_dir(self.root.join("definitions"))?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        package.definition.id = remap_id(&package.definition.id, &existing_macro_ids);

        let identity_index = self.assets.load_index_locked()?;
        let mut remaps = HashMap::new();
        let mut reserved_asset_ids: HashSet<String> = identity_index
            .bindings
            .iter()
            .map(|asset| asset.id.clone())
            .collect();
        reserved_asset_ids.extend(
            package
                .assets
                .iter()
                .map(|package_asset| package_asset.asset.id.clone()),
        );
        for package_asset in &package.assets {
            let key = (package_asset.asset.id.clone(), package_asset.asset.revision);
            if identity_index.bindings.iter().any(|existing| {
                existing.id == key.0
                    && existing.revision == key.1
                    && existing.content_hash != package_asset.asset.content_hash
            }) {
                let remapped = remap_id(&package_asset.asset.id, &reserved_asset_ids);
                reserved_asset_ids.insert(remapped.clone());
                remaps.insert(key, remapped);
            }
        }
        for asset in referenced_assets_mut(&mut package.definition) {
            if let Some(id) = remaps.get(&(asset.id.clone(), asset.revision)) {
                asset.id = id.clone();
            }
        }
        let mut installed = Vec::new();
        for package_asset in &mut package.assets {
            if let Some(id) =
                remaps.get(&(package_asset.asset.id.clone(), package_asset.asset.revision))
            {
                package_asset.asset.id = id.clone();
            }
        }
        validate_package_memory(&package)?;
        for package_asset in &mut package.assets {
            match self
                .assets
                .install_locked(package_asset.asset.clone(), &package_asset.bytes)
            {
                Ok(change) => installed.push(change),
                Err(error) => {
                    let rollback = self.assets.rollback_locked(&installed);
                    return match rollback {
                        Ok(()) => Err(error),
                        Err(rollback_error) => Err(error
                            .context(format!("asset rollback also failed: {rollback_error:#}"))),
                    };
                }
            }
        }
        #[cfg(test)]
        if self.fail_import_after_assets.swap(false, Ordering::SeqCst) {
            self.assets.rollback_locked(&installed)?;
            bail!("injected import failure after asset installs");
        }
        match self.save_locked(package.definition) {
            Ok(saved) => Ok(saved),
            Err(error) => {
                let rollback = self.assets.rollback_locked(&installed);
                match rollback {
                    Ok(()) => Err(error),
                    Err(rollback_error) => {
                        Err(error
                            .context(format!("asset rollback also failed: {rollback_error:#}")))
                    }
                }
            }
        }
    }

    #[cfg(test)]
    fn fail_next_import_after_asset_installs(&self) {
        self.fail_import_after_assets.store(true, Ordering::SeqCst);
    }

    pub fn open_journal(&self, run_id: &str, limits: JournalLimits) -> JournalOpenOutcome {
        let result = (|| -> Result<RunJournal> {
            let _guard = lock_store(&self.lock)?;
            validate_component("run ID", run_id)?;
            if limits.max_bytes_per_run == 0 || limits.max_runs == 0 {
                bail!("journal limits must be non-zero");
            }
            let runs = self.root.join("runs");
            fs::create_dir_all(&runs)?;
            let path = runs.join(format!("{run_id}.jsonl"));
            let file = match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    bail!("journal run ID already exists: {run_id}")
                }
                Err(error) => return Err(error.into()),
            };
            if let Err(error) =
                prune_run_files(&runs, limits.max_runs.saturating_sub(1), Some(&path))
            {
                drop(file);
                let _ = fs::remove_file(&path);
                return Err(error);
            }
            Ok(RunJournal {
                path,
                file,
                max_bytes: limits.max_bytes_per_run,
                bytes_written: 0,
                failed: false,
            })
        })();
        match result {
            Ok(journal) => JournalOpenOutcome::Ready(journal),
            Err(error) => JournalOpenOutcome::Disabled {
                diagnostic: format!("journal initialization failed: {error:#}"),
            },
        }
    }

    pub fn cleanup_orphan_assets(&self, running_assets: &HashSet<AssetRef>) -> Result<usize> {
        let _guard = lock_store(&self.lock)?;
        validate_identity_set(running_assets.iter())?;
        let index = self.assets.load_index_locked()?;
        for binding in &index.bindings {
            let path = self.assets.path_for_hash(&binding.content_hash)?;
            if path.exists() {
                self.assets.verify_hash_file_locked(&binding.content_hash)?;
            }
        }
        for running in running_assets {
            validate_identity_binding(&index.bindings, running)?;
            self.assets.verify_hash_file_locked(&running.content_hash)?;
        }

        let mut protected = running_assets.clone();
        let definitions_root = self.root.join("definitions");
        for macro_directory in fs::read_dir(definitions_root)? {
            let macro_directory = macro_directory?;
            if !macro_directory.file_type()?.is_dir() {
                continue;
            }
            let macro_id = macro_directory.file_name().to_string_lossy().into_owned();
            validate_component("saved macro directory", &macro_id)?;
            validate_revision_sidecars(&macro_directory.path())?;
            for revision_file in fs::read_dir(macro_directory.path())? {
                let revision_file = revision_file?;
                if !revision_file.file_type()?.is_file()
                    || revision_file
                        .path()
                        .extension()
                        .and_then(|ext| ext.to_str())
                        != Some("json")
                {
                    continue;
                }
                let file_name = revision_file.file_name().to_string_lossy().into_owned();
                let bytes = fs::read(revision_file.path())?;
                let definition: MacroDefinition = serde_json::from_slice(&bytes)
                    .context("corrupt saved macro definition blocks orphan cleanup")?;
                if definition.schema_version != MACRO_SCHEMA_VERSION {
                    bail!(
                        "unsupported macro schema {} blocks orphan cleanup",
                        definition.schema_version
                    );
                }
                if definition.id != macro_id {
                    bail!("saved macro ID does not match definition directory");
                }
                if file_name == "current.json" {
                    let immutable = macro_directory
                        .path()
                        .join(format!("{}.json", definition.revision));
                    if fs::read(&immutable).context("current revision file is missing")? != bytes {
                        bail!("current definition does not match immutable revision");
                    }
                } else {
                    verify_revision_checksum(&revision_file.path(), &bytes)?;
                    let file_revision = file_name
                        .strip_suffix(".json")
                        .and_then(|stem| stem.parse::<u64>().ok())
                        .context("saved revision filename is invalid")?;
                    if definition.revision != file_revision {
                        bail!("saved revision does not match revision filename");
                    }
                }
                validate_identity_set(referenced_assets(&definition))?;
                for asset in referenced_assets(&definition) {
                    validate_identity_binding(&index.bindings, asset)?;
                    self.assets.verify_hash_file_locked(&asset.content_hash)?;
                    protected.insert(asset.clone());
                }
            }
        }

        let mut asset_files = Vec::new();
        for entry in fs::read_dir(&self.assets.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file()
                || entry.path().extension().and_then(|ext| ext.to_str()) != Some("png")
            {
                continue;
            }
            let hash = entry
                .path()
                .file_stem()
                .and_then(|stem| stem.to_str())
                .context("asset filename is not valid UTF-8")?
                .to_string();
            self.assets.verify_hash_file_locked(&hash)?;
            asset_files.push((hash, entry.path()));
        }

        let referenced_hashes: HashSet<_> = protected
            .iter()
            .map(|asset| asset.content_hash.as_str())
            .collect();
        let mut removed = 0;
        for (hash, path) in asset_files {
            if !referenced_hashes.contains(hash.as_str()) {
                fs::remove_file(path)?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

fn prune_run_files(runs: &Path, keep: usize, exclude: Option<&Path>) -> Result<()> {
    let mut files = Vec::new();
    for entry in fs::read_dir(runs)? {
        let entry = entry?;
        if exclude.is_some_and(|excluded| entry.path() == excluded) {
            continue;
        }
        if entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl")
            && entry.file_type()?.is_file()
        {
            files.push(entry);
        }
    }
    files.sort_by_key(|entry| {
        entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    let remove_count = files.len().saturating_sub(keep);
    for entry in files.into_iter().take(remove_count) {
        fs::remove_file(entry.path())?;
    }
    Ok(())
}

fn validate_revision_sidecars(definition_directory: &Path) -> Result<()> {
    for entry in fs::read_dir(definition_directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("sha256") {
            continue;
        }
        if !entry.file_type()?.is_file() {
            bail!("revision checksum metadata is not a file");
        }
        let file_name = entry
            .file_name()
            .to_str()
            .context("revision checksum filename is invalid UTF-8")?
            .to_string();
        let revision_text = file_name
            .strip_suffix(".json.sha256")
            .context("revision checksum filename is invalid")?;
        let revision = revision_text
            .parse::<u64>()
            .context("revision checksum filename is invalid")?;
        if revision.to_string() != revision_text {
            bail!("revision checksum filename is invalid");
        }
        let revision_path = definition_directory.join(format!("{revision}.json"));
        if !revision_path.is_file() {
            bail!(
                "orphan revision checksum has no immutable revision: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn referenced_assets(definition: &MacroDefinition) -> impl Iterator<Item = &AssetRef> {
    definition
        .image_rules
        .iter()
        .flat_map(|rule| std::iter::once(&rule.template).chain(rule.transparent_mask.as_ref()))
}

fn referenced_assets_mut(definition: &mut MacroDefinition) -> impl Iterator<Item = &mut AssetRef> {
    definition
        .image_rules
        .iter_mut()
        .flat_map(|rule| std::iter::once(&mut rule.template).chain(rule.transparent_mask.as_mut()))
}

fn validate_asset_ref(asset: &AssetRef) -> Result<()> {
    validate_component("asset ID", &asset.id)?;
    if asset.content_hash.len() != 64
        || !asset
            .content_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("invalid asset content hash");
    }
    Ok(())
}

fn validate_identity_set<'a>(assets: impl IntoIterator<Item = &'a AssetRef>) -> Result<()> {
    let mut identities: HashMap<(&str, u64), &str> = HashMap::new();
    for asset in assets {
        validate_asset_ref(asset)?;
        let key = (asset.id.as_str(), asset.revision);
        if let Some(existing) = identities.insert(key, asset.content_hash.as_str()) {
            if existing != asset.content_hash {
                bail!(
                    "conflicting asset identity {} revision {} has multiple hashes",
                    asset.id,
                    asset.revision
                );
            }
        }
    }
    Ok(())
}

fn validate_identity_binding(bindings: &[AssetRef], asset: &AssetRef) -> Result<()> {
    validate_asset_ref(asset)?;
    let Some(binding) = bindings
        .iter()
        .find(|binding| binding.id == asset.id && binding.revision == asset.revision)
    else {
        bail!(
            "missing asset identity {} revision {}",
            asset.id,
            asset.revision
        );
    };
    if binding.content_hash != asset.content_hash {
        bail!(
            "immutable asset identity {} revision {} is bound to a different hash",
            asset.id,
            asset.revision
        );
    }
    Ok(())
}

fn validate_package_memory(package: &MacroPackage) -> Result<()> {
    if package.schema_version != PACKAGE_SCHEMA_VERSION {
        bail!("unsupported package schema {}", package.schema_version);
    }
    if package.definition.schema_version != MACRO_SCHEMA_VERSION {
        bail!(
            "unsupported macro schema {}",
            package.definition.schema_version
        );
    }
    validate_component("macro ID", &package.definition.id)?;
    validate_identity_set(referenced_assets(&package.definition))?;
    validate_identity_set(package.assets.iter().map(|asset| &asset.asset))?;
    let mut package_refs = HashSet::new();
    for package_asset in &package.assets {
        if !package_refs.insert(package_asset.asset.clone()) {
            bail!("duplicate asset entry in package");
        }
        if sha256_hex(&package_asset.bytes) != package_asset.asset.content_hash {
            bail!(
                "package asset hash mismatch: {}",
                package_asset.asset.content_hash
            );
        }
    }
    let definition_refs: HashSet<_> = referenced_assets(&package.definition).cloned().collect();
    if definition_refs != package_refs {
        bail!("package asset references do not match definition");
    }
    Ok(())
}

fn lock_store(lock: &Mutex<()>) -> Result<MutexGuard<'_, ()>> {
    lock.lock()
        .map_err(|_| anyhow::anyhow!("macro store lock poisoned"))
}

fn shared_store_lock(root: &Path) -> Result<Arc<Mutex<()>>> {
    let registry = STORE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = registry
        .lock()
        .map_err(|_| anyhow::anyhow!("macro store lock registry poisoned"))?;
    registry.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = registry.get(root).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    registry.insert(root.to_path_buf(), Arc::downgrade(&lock));
    Ok(lock)
}

fn package_file(root: &Path, relative: &Path) -> Result<PathBuf> {
    validate_package_relative(relative)?;
    let joined = root.join(relative);
    let canonical = joined
        .canonicalize()
        .with_context(|| format!("missing package file: {}", relative.display()))?;
    if !canonical.starts_with(root) {
        bail!("reference points outside package");
    }
    Ok(canonical)
}

fn validate_package_relative(relative: &Path) -> Result<()> {
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("reference points outside package");
    }
    Ok(())
}

fn remap_id(id: &str, existing: &HashSet<String>) -> String {
    if !existing.contains(id) {
        return id.to_string();
    }
    for suffix in 1_u64.. {
        let candidate = format!("{id}-imported-{suffix}");
        if !existing.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

fn validate_component(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
    {
        bail!("invalid {label}");
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn revision_checksum_path(revision_path: &Path) -> Result<PathBuf> {
    let name = revision_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("revision path has no valid filename")?;
    Ok(revision_path.with_file_name(format!("{name}.sha256")))
}

fn verify_revision_checksum(revision_path: &Path, bytes: &[u8]) -> Result<()> {
    let checksum_path = revision_checksum_path(revision_path)?;
    let expected = fs::read_to_string(&checksum_path)
        .with_context(|| format!("revision checksum is missing: {}", checksum_path.display()))?;
    if expected != sha256_hex(bytes) {
        bail!("revision checksum mismatch for {}", revision_path.display());
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishOutcome {
    Published,
    Identical,
}

fn atomic_publish_noclobber(path: &Path, bytes: &[u8]) -> Result<PublishOutcome> {
    let parent = path.parent().context("atomic publication has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file_mut().flush()?;
    temp.as_file_mut().sync_all()?;
    match temp.persist_noclobber(path) {
        Ok(_) => {
            sync_directory(parent)?;
            Ok(PublishOutcome::Published)
        }
        Err(error) => {
            if path.exists() {
                let existing = fs::read(path)?;
                if existing == bytes {
                    return Ok(PublishOutcome::Identical);
                }
                bail!("immutable content conflict at {}", path.display());
            }
            Err(error.error.into())
        }
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("atomic write has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file_mut().flush()?;
    temp.as_file_mut().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    sync_directory(parent)?;
    Ok(())
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        File::open(path)?.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::macro_engine::{
        FocusLossPolicy, ImageRule, Limit, MatchSelectionPolicy, SafetyPolicy, TargetProfile,
    };
    use std::sync::{Arc, Barrier};

    fn fixture_definition(asset: AssetRef) -> MacroDefinition {
        MacroDefinition {
            schema_version: MACRO_SCHEMA_VERSION,
            id: "macro-one".to_string(),
            name: "Macro One".to_string(),
            revision: 1,
            target: TargetProfile {
                process_path: "game.exe".to_string(),
                window_class: "game".to_string(),
                title_contains: "Diablo".to_string(),
                captured_client_width: 1920,
                captured_client_height: 1080,
                captured_dpi: 96,
            },
            regions: vec![],
            points: vec![],
            text_rules: vec![],
            image_rules: vec![ImageRule {
                id: "image-one".to_string(),
                revision: 1,
                region_id: "region-one".to_string(),
                template: asset,
                transparent_mask: None,
                threshold: 0.95,
                scales_percent: vec![100],
                stable_frames: 2,
                maximum_center_drift_px: 2,
                minimum_runner_up_margin: 0.05,
                verification: None,
                match_policy: MatchSelectionPolicy::ExactlyOne,
                poll_interval_ms: 100,
                timeout_ms: Limit::Finite(5_000),
            }],
            blocks: vec![],
            safety: SafetyPolicy {
                max_runtime_ms: Limit::Finite(10_000),
                max_clicks: Limit::Finite(10),
                max_observation_retries: Limit::Finite(10),
                max_observations_per_second: 10,
                minimum_click_interval_ms: 100,
                focus_loss: FocusLossPolicy::Stop,
            },
        }
    }

    fn ready_journal(outcome: JournalOpenOutcome) -> RunJournal {
        match outcome {
            JournalOpenOutcome::Ready(journal) => journal,
            JournalOpenOutcome::Disabled { diagnostic } => panic!("{diagnostic}"),
        }
    }

    #[test]
    fn run_snapshot_pins_template_hash() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let asset = store.assets().put_png(&[1, 2, 3, 4]).unwrap();
        let saved = store.save(fixture_definition(asset.clone())).unwrap();

        store.assets().put_png(&[9, 8, 7]).unwrap();

        assert_eq!(
            saved.definition.image_rules[0].template.content_hash,
            asset.content_hash
        );
        assert_eq!(saved.pinned_assets[0].bytes, vec![1, 2, 3, 4]);
    }

    #[test]
    fn atomic_save_replaces_current_and_preserves_it_after_validation_failure() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let asset = store.assets().put_png(&[1, 2, 3, 4]).unwrap();
        store.save(fixture_definition(asset.clone())).unwrap();

        let mut second = fixture_definition(asset);
        second.revision = 2;
        second.name = "Second".to_string();
        store.save(second).unwrap();

        let current_path = temp
            .path()
            .join("macro_data/definitions/macro-one/current.json");
        let current: MacroDefinition =
            serde_json::from_slice(&fs::read(&current_path).unwrap()).unwrap();
        assert_eq!(current.revision, 2);

        let missing = AssetRef {
            id: "missing".to_string(),
            revision: 1,
            content_hash: "0".repeat(64),
        };
        let mut invalid = fixture_definition(missing);
        invalid.revision = 3;
        assert!(store.save(invalid).is_err());

        let current: MacroDefinition =
            serde_json::from_slice(&fs::read(current_path).unwrap()).unwrap();
        assert_eq!(current.revision, 2);
    }

    #[test]
    fn saved_revision_is_immutable() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let asset = store.assets().put_png(b"template").unwrap();
        store.save(fixture_definition(asset.clone())).unwrap();

        let mut changed_same_revision = fixture_definition(asset);
        changed_same_revision.name = "mutated".to_string();
        let error = store.save(changed_same_revision).unwrap_err();

        assert!(error.to_string().contains("immutable revision"));
        let current: MacroDefinition = serde_json::from_slice(
            &fs::read(
                temp.path()
                    .join("macro_data/definitions/macro-one/current.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(current.name, "Macro One");
    }

    #[test]
    fn import_rejects_outside_asset_reference() {
        let package =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/macro/packages/traversal");
        let error = MacroStore::validate_package(&package).unwrap_err();
        assert!(error.to_string().contains("outside package"));
    }

    #[test]
    fn journal_caps_bytes_during_unlimited_runs() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let mut journal = ready_journal(store.open_journal("run-one", JournalLimits::new(220, 3)));

        let mut dropped = false;
        for sequence in 0..100 {
            let outcome = journal.append(&JournalRecord {
                sequence,
                elapsed_ms: sequence,
                kind: JournalKind::Candidate,
                message: "candidate observed".to_string(),
                fields: serde_json::json!({"score": 0.95}),
            });
            dropped |= matches!(outcome, JournalAppendOutcome::Dropped { .. });
        }

        assert!(dropped);
        assert!(fs::metadata(journal.path()).unwrap().len() <= 220);
    }

    #[test]
    fn orphan_cleanup_keeps_saved_and_running_assets() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let saved_asset = store.assets().put_png(b"saved").unwrap();
        let running_asset = store.assets().put_png(b"running").unwrap();
        let orphan = store.assets().put_png(b"orphan").unwrap();
        store.save(fixture_definition(saved_asset.clone())).unwrap();

        let removed = store
            .cleanup_orphan_assets(&HashSet::from([running_asset.clone()]))
            .unwrap();

        assert_eq!(removed, 1);
        assert!(store.assets().read(&saved_asset).is_ok());
        assert!(store.assets().read(&running_asset).is_ok());
        assert!(store.assets().read(&orphan).is_err());
    }

    #[test]
    fn package_import_remaps_colliding_macro_and_asset_ids_and_owns_bytes() {
        let source_temp = tempfile::tempdir().unwrap();
        let source = MacroStore::open(source_temp.path()).unwrap();
        let source_bytes = b"source-template";
        let source_asset = AssetRef {
            id: "shared-template".to_string(),
            revision: 1,
            content_hash: sha256_hex(source_bytes),
        };
        source
            .assets()
            .put_png_revision(source_asset.clone(), source_bytes)
            .unwrap();
        let source_saved = source
            .save(fixture_definition(source_asset.clone()))
            .unwrap();
        let package_folder = source_temp.path().join("package");
        source
            .export_package(&source_saved, &package_folder)
            .unwrap();

        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();
        let existing_bytes = b"existing-template";
        let existing_asset = AssetRef {
            id: "shared-template".to_string(),
            revision: 1,
            content_hash: sha256_hex(existing_bytes),
        };
        destination
            .assets()
            .put_png_revision(existing_asset.clone(), existing_bytes)
            .unwrap();
        destination
            .save(fixture_definition(existing_asset))
            .unwrap();

        let imported = destination.import_package(&package_folder).unwrap();
        fs::remove_dir_all(package_folder).unwrap();

        assert_eq!(imported.definition.id, "macro-one-imported-1");
        let imported_asset = &imported.definition.image_rules[0].template;
        assert_eq!(imported_asset.id, "shared-template-imported-1");
        assert_eq!(
            destination.assets().read(imported_asset).unwrap(),
            source_bytes
        );
    }

    #[test]
    fn valid_package_verifies_definition_and_asset_hash() {
        let package =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/macro/packages/valid");
        let validated = MacroStore::validate_package(&package).unwrap();

        assert_eq!(validated.definition.id, "fixture-macro");
        assert_eq!(validated.assets.len(), 1);
    }

    #[test]
    fn corrupt_package_json_is_rejected() {
        let package =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/macro/packages/corrupt");
        let error = MacroStore::validate_package(&package).unwrap_err();

        assert!(error.to_string().contains("corrupt macro JSON"));
    }

    #[test]
    fn package_rejects_missing_and_hash_mismatched_assets() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let asset = store.assets().put_png(b"template").unwrap();
        let saved = store.save(fixture_definition(asset.clone())).unwrap();
        let package = temp.path().join("package");
        store.export_package(&saved, &package).unwrap();
        let asset_path = package
            .join("assets")
            .join(format!("{}.png", asset.content_hash));

        fs::write(&asset_path, b"tampered").unwrap();
        let hash_error = MacroStore::validate_package(&package).unwrap_err();
        assert!(hash_error.to_string().contains("hash mismatch"));

        fs::remove_file(asset_path).unwrap();
        let missing_error = MacroStore::validate_package(&package).unwrap_err();
        assert!(missing_error.to_string().contains("missing package file"));
    }

    #[test]
    fn journal_prunes_old_runs_to_run_cap() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        for run in 0..5 {
            ready_journal(store.open_journal(&format!("run-{run}"), JournalLimits::new(1_024, 2)));
        }

        let runs = fs::read_dir(temp.path().join("macro_data/runs"))
            .unwrap()
            .count();
        assert_eq!(runs, 2);
    }

    #[test]
    fn concurrent_first_revision_publication_never_overwrites_the_winner() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(MacroStore::open(temp.path()).unwrap());
        let asset = store.assets().put_png(b"template").unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let mut threads = Vec::new();
        for name in ["first", "second"] {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let asset = asset.clone();
            threads.push(std::thread::spawn(move || {
                let mut definition = fixture_definition(asset);
                definition.name = name.to_string();
                barrier.wait();
                store.save(definition)
            }));
        }
        barrier.wait();
        let outcomes = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        let winner = outcomes.into_iter().find_map(Result::ok).unwrap();
        let revision: MacroDefinition = serde_json::from_slice(
            &fs::read(temp.path().join("macro_data/definitions/macro-one/1.json")).unwrap(),
        )
        .unwrap();
        let current: MacroDefinition = serde_json::from_slice(
            &fs::read(
                temp.path()
                    .join("macro_data/definitions/macro-one/current.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(revision.name, winner.definition.name);
        assert_eq!(current.name, winner.definition.name);
    }

    #[test]
    fn asset_identity_is_immutable_but_distinct_identities_may_share_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let bytes = b"same bytes";
        let hash = sha256_hex(bytes);
        let first = AssetRef {
            id: "first".to_string(),
            revision: 1,
            content_hash: hash.clone(),
        };
        let second = AssetRef {
            id: "second".to_string(),
            revision: 1,
            content_hash: hash,
        };
        store
            .assets()
            .put_png_revision(first.clone(), bytes)
            .unwrap();
        store
            .assets()
            .put_png_revision(second.clone(), bytes)
            .unwrap();
        let mut definition = fixture_definition(first.clone());
        definition.image_rules[0].transparent_mask = Some(second.clone());
        let saved = store.save(definition).unwrap();
        assert_eq!(saved.pinned_assets.len(), 2);
        assert!(saved.pinned_assets.iter().any(|asset| asset.asset == first));
        assert!(
            saved
                .pinned_assets
                .iter()
                .any(|asset| asset.asset == second)
        );

        let conflicting_bytes = b"different";
        let conflicting = AssetRef {
            id: "first".to_string(),
            revision: 1,
            content_hash: sha256_hex(conflicting_bytes),
        };
        let error = store
            .assets()
            .put_png_revision(conflicting, conflicting_bytes)
            .unwrap_err();
        assert!(error.to_string().contains("immutable asset identity"));
    }

    #[test]
    fn direct_save_rejects_conflicting_asset_identities() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let first_bytes = b"first";
        let second_bytes = b"second";
        let first = AssetRef {
            id: "same".to_string(),
            revision: 1,
            content_hash: sha256_hex(first_bytes),
        };
        let second = AssetRef {
            id: "same".to_string(),
            revision: 1,
            content_hash: sha256_hex(second_bytes),
        };
        // Seed corrupt durable state directly to prove save validates the
        // definition independently of the identity installer.
        atomic_write(
            &store.assets.path_for_hash(&first.content_hash).unwrap(),
            first_bytes,
        )
        .unwrap();
        atomic_write(
            &store.assets.path_for_hash(&second.content_hash).unwrap(),
            second_bytes,
        )
        .unwrap();
        let mut definition = fixture_definition(first);
        definition.image_rules[0].transparent_mask = Some(second);

        let error = store.save(definition).unwrap_err();
        assert!(error.to_string().contains("conflicting asset identity"));
    }

    #[test]
    fn package_rejects_duplicate_identity_with_different_hashes() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let asset = store.assets().put_png(b"template").unwrap();
        let saved = store.save(fixture_definition(asset)).unwrap();
        let package = temp.path().join("package");
        store.export_package(&saved, &package).unwrap();

        let manifest_path = package.join("manifest.json");
        let mut manifest: PackageManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        let other_bytes = b"other";
        let other_hash = sha256_hex(other_bytes);
        let mut duplicate = manifest.assets[0].clone();
        duplicate.asset.content_hash = other_hash.clone();
        duplicate.path = PathBuf::from("assets").join(format!("{other_hash}.png"));
        fs::write(package.join(&duplicate.path), other_bytes).unwrap();
        manifest.assets.push(duplicate);
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let error = MacroStore::validate_package(&package).unwrap_err();
        assert!(error.to_string().contains("conflicting asset identity"));
    }

    #[test]
    fn cleanup_aborts_without_deleting_when_saved_hash_is_parseably_corrupt() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let saved_asset = store.assets().put_png(b"saved").unwrap();
        let orphan = store.assets().put_png(b"orphan").unwrap();
        store.save(fixture_definition(saved_asset)).unwrap();
        let revision_path = temp.path().join("macro_data/definitions/macro-one/1.json");
        let mut definition: MacroDefinition =
            serde_json::from_slice(&fs::read(&revision_path).unwrap()).unwrap();
        definition.image_rules[0].template.content_hash = "0".repeat(64);
        fs::write(
            &revision_path,
            serde_json::to_vec_pretty(&definition).unwrap(),
        )
        .unwrap();

        assert!(store.cleanup_orphan_assets(&HashSet::new()).is_err());
        assert!(store.assets().read(&orphan).is_ok());
    }

    #[test]
    fn failed_import_rolls_back_new_assets_and_identity_bindings() {
        let source_temp = tempfile::tempdir().unwrap();
        let source = MacroStore::open(source_temp.path()).unwrap();
        let asset = source.assets().put_png(b"source").unwrap();
        let saved = source.save(fixture_definition(asset.clone())).unwrap();
        let package_path = source_temp.path().join("package");
        source.export_package(&saved, &package_path).unwrap();

        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();
        destination.fail_next_import_after_asset_installs();
        assert!(destination.import_package(&package_path).is_err());
        assert!(destination.assets().read(&asset).is_err());
        assert_eq!(
            fs::read_dir(destination_temp.path().join("macro_data/definitions"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn validated_package_import_uses_captured_bytes_after_source_substitution() {
        let source_temp = tempfile::tempdir().unwrap();
        let source = MacroStore::open(source_temp.path()).unwrap();
        let original = b"original";
        let asset = source.assets().put_png(original).unwrap();
        let saved = source.save(fixture_definition(asset.clone())).unwrap();
        let package_path = source_temp.path().join("package");
        source.export_package(&saved, &package_path).unwrap();
        let package = MacroStore::validate_package(&package_path).unwrap();
        fs::write(
            package_path
                .join("assets")
                .join(format!("{}.png", asset.content_hash)),
            b"substituted",
        )
        .unwrap();

        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();
        let imported = destination.import_validated_package(package).unwrap();
        assert_eq!(
            destination
                .assets()
                .read(&imported.definition.image_rules[0].template)
                .unwrap(),
            original
        );
    }

    #[test]
    fn export_is_no_clobber_for_an_existing_package_folder() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let asset = store.assets().put_png(b"template").unwrap();
        let first = store.save(fixture_definition(asset.clone())).unwrap();
        let package = temp.path().join("package");
        store.export_package(&first, &package).unwrap();
        let original = fs::read(package.join("macro.json")).unwrap();

        let mut second_definition = fixture_definition(asset);
        second_definition.revision = 2;
        second_definition.name = "second".to_string();
        let second = store.save(second_definition).unwrap();
        assert!(store.export_package(&second, &package).is_err());
        assert_eq!(fs::read(package.join("macro.json")).unwrap(), original);
    }

    #[test]
    fn journal_initialization_failure_disables_persistence_without_run_error() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        fs::remove_dir(temp.path().join("macro_data/runs")).unwrap();
        fs::write(temp.path().join("macro_data/runs"), b"not a directory").unwrap();

        let outcome = store.open_journal("run-one", JournalLimits::new(1_024, 2));
        assert!(matches!(
            outcome,
            JournalOpenOutcome::Disabled { diagnostic }
                if diagnostic.contains("journal initialization failed")
        ));
    }

    #[test]
    fn cleanup_keeps_asset_identity_history_after_removing_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let original_bytes = b"original";
        let original = AssetRef {
            id: "historical".to_string(),
            revision: 7,
            content_hash: sha256_hex(original_bytes),
        };
        store
            .assets()
            .put_png_revision(original.clone(), original_bytes)
            .unwrap();

        assert_eq!(store.cleanup_orphan_assets(&HashSet::new()).unwrap(), 1);
        assert!(store.assets().read(&original).is_err());

        let conflicting_bytes = b"different";
        let conflicting = AssetRef {
            id: original.id.clone(),
            revision: original.revision,
            content_hash: sha256_hex(conflicting_bytes),
        };
        let error = store
            .assets()
            .put_png_revision(conflicting, conflicting_bytes)
            .unwrap_err();
        assert!(error.to_string().contains("immutable asset identity"));

        store
            .assets()
            .put_png_revision(original.clone(), original_bytes)
            .unwrap();
        assert_eq!(store.assets().read(&original).unwrap(), original_bytes);
    }

    #[test]
    fn independently_opened_stores_share_lock_and_serialize_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let first = MacroStore::open(temp.path()).unwrap();
        let second = MacroStore::open(temp.path()).unwrap();
        assert!(Arc::ptr_eq(&first.lock, &second.lock));

        let base = first.assets().put_png(b"base").unwrap();
        let identity_barrier = Arc::new(Barrier::new(3));
        let contenders = [b"alpha".as_slice(), b"beta".as_slice()]
            .into_iter()
            .enumerate()
            .map(|(index, bytes)| {
                let store = if index == 0 {
                    first.clone()
                } else {
                    second.clone()
                };
                let barrier = Arc::clone(&identity_barrier);
                let bytes = bytes.to_vec();
                std::thread::spawn(move || {
                    let asset = AssetRef {
                        id: "contended".to_string(),
                        revision: 1,
                        content_hash: sha256_hex(&bytes),
                    };
                    barrier.wait();
                    store
                        .assets()
                        .put_png_revision(asset.clone(), &bytes)
                        .map(|()| asset)
                })
            })
            .collect::<Vec<_>>();
        identity_barrier.wait();
        let identity_results = contenders
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            identity_results
                .iter()
                .filter(|result| result.is_ok())
                .count(),
            1
        );
        let identity_winner = identity_results.into_iter().find_map(Result::ok).unwrap();
        assert!(first.assets().read(&base).is_ok());
        assert!(second.assets().read(&identity_winner).is_ok());

        let save_barrier = Arc::new(Barrier::new(3));
        let writers = [first.clone(), second.clone()]
            .into_iter()
            .enumerate()
            .map(|(index, store)| {
                let barrier = Arc::clone(&save_barrier);
                let asset = base.clone();
                std::thread::spawn(move || {
                    let mut definition = fixture_definition(asset);
                    definition.name = format!("writer-{index}");
                    barrier.wait();
                    store.save(definition)
                })
            })
            .collect::<Vec<_>>();
        save_barrier.wait();
        let save_results = writers
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            save_results.iter().filter(|result| result.is_ok()).count(),
            1
        );
    }

    #[test]
    fn import_remap_reserves_ids_already_present_in_the_package() {
        let source_temp = tempfile::tempdir().unwrap();
        let source = MacroStore::open(source_temp.path()).unwrap();
        let x_bytes = b"package-x";
        let reserved_bytes = b"package-reserved";
        let x = AssetRef {
            id: "x".to_string(),
            revision: 1,
            content_hash: sha256_hex(x_bytes),
        };
        let reserved = AssetRef {
            id: "x-imported-1".to_string(),
            revision: 1,
            content_hash: sha256_hex(reserved_bytes),
        };
        source
            .assets()
            .put_png_revision(x.clone(), x_bytes)
            .unwrap();
        source
            .assets()
            .put_png_revision(reserved.clone(), reserved_bytes)
            .unwrap();
        let mut definition = fixture_definition(x);
        definition.image_rules[0].transparent_mask = Some(reserved.clone());
        let saved = source.save(definition).unwrap();
        let package_path = source_temp.path().join("package");
        source.export_package(&saved, &package_path).unwrap();

        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();
        let destination_bytes = b"destination-x";
        destination
            .assets()
            .put_png_revision(
                AssetRef {
                    id: "x".to_string(),
                    revision: 1,
                    content_hash: sha256_hex(destination_bytes),
                },
                destination_bytes,
            )
            .unwrap();

        let imported = destination.import_package(&package_path).unwrap();
        assert_eq!(
            imported.definition.image_rules[0].template.id,
            "x-imported-2"
        );
        assert_eq!(
            imported.definition.image_rules[0]
                .transparent_mask
                .as_ref()
                .unwrap()
                .id,
            reserved.id
        );
    }

    #[test]
    fn cleanup_rejects_valid_json_mutation_when_revision_checksum_mismatches() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let first_asset = store.assets().put_png(b"first").unwrap();
        let second_asset = store.assets().put_png(b"second").unwrap();
        let orphan = store.assets().put_png(b"orphan").unwrap();
        store.save(fixture_definition(first_asset)).unwrap();
        let mut second = fixture_definition(second_asset.clone());
        second.revision = 2;
        store.save(second).unwrap();
        let revision_path = temp.path().join("macro_data/definitions/macro-one/1.json");
        let mut old_revision: MacroDefinition =
            serde_json::from_slice(&fs::read(&revision_path).unwrap()).unwrap();
        old_revision.image_rules[0].template = second_asset;
        fs::write(
            &revision_path,
            serde_json::to_vec_pretty(&old_revision).unwrap(),
        )
        .unwrap();

        let error = store.cleanup_orphan_assets(&HashSet::new()).unwrap_err();
        assert!(error.to_string().contains("revision checksum"));
        assert!(store.assets().read(&orphan).is_ok());
    }

    #[test]
    fn duplicate_run_id_disables_second_journal_without_truncating_first() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let mut first = ready_journal(store.open_journal("same-run", JournalLimits::new(512, 3)));
        assert!(matches!(
            first.append(&JournalRecord {
                sequence: 1,
                elapsed_ms: 1,
                kind: JournalKind::StateChange,
                message: "started".to_string(),
                fields: serde_json::json!({}),
            }),
            JournalAppendOutcome::Written
        ));
        let length_before = fs::metadata(first.path()).unwrap().len();

        assert!(matches!(
            store.open_journal("same-run", JournalLimits::new(512, 3)),
            JournalOpenOutcome::Disabled { diagnostic }
                if diagnostic.contains("already exists")
        ));
        assert_eq!(fs::metadata(first.path()).unwrap().len(), length_before);
        assert!(matches!(
            first.append(&JournalRecord {
                sequence: 2,
                elapsed_ms: 2,
                kind: JournalKind::Aggregate,
                message: "still active".to_string(),
                fields: serde_json::json!({}),
            }),
            JournalAppendOutcome::Written
        ));
        assert!(fs::metadata(first.path()).unwrap().len() <= 512);
    }

    #[test]
    fn cleanup_rejects_orphan_revision_checksum_before_deleting_assets() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let old_asset = store.assets().put_png(b"old revision").unwrap();
        let current_asset = store.assets().put_png(b"current revision").unwrap();
        let orphan = store.assets().put_png(b"unreferenced").unwrap();
        store.save(fixture_definition(old_asset.clone())).unwrap();
        let mut current = fixture_definition(current_asset.clone());
        current.revision = 2;
        store.save(current).unwrap();
        fs::remove_file(temp.path().join("macro_data/definitions/macro-one/1.json")).unwrap();

        let error = store.cleanup_orphan_assets(&HashSet::new()).unwrap_err();
        assert!(error.to_string().contains("orphan revision checksum"));
        assert!(store.assets().read(&old_asset).is_ok());
        assert!(store.assets().read(&current_asset).is_ok());
        assert!(store.assets().read(&orphan).is_ok());
    }

    #[test]
    fn cleanup_rejects_malformed_revision_checksum_filename() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let saved_asset = store.assets().put_png(b"saved").unwrap();
        let orphan = store.assets().put_png(b"orphan").unwrap();
        store.save(fixture_definition(saved_asset)).unwrap();
        fs::write(
            temp.path()
                .join("macro_data/definitions/macro-one/not-a-revision.json.sha256"),
            "0".repeat(64),
        )
        .unwrap();

        let error = store.cleanup_orphan_assets(&HashSet::new()).unwrap_err();
        assert!(error.to_string().contains("checksum filename is invalid"));
        assert!(store.assets().read(&orphan).is_ok());
    }
}
