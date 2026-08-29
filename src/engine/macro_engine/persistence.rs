use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
#[cfg(windows)]
use std::os::windows::{
    ffi::OsStrExt,
    io::{AsRawHandle, FromRawHandle},
};
#[cfg(windows)]
use windows::Wdk::{
    Foundation::OBJECT_ATTRIBUTES,
    Storage::FileSystem::{
        FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
        FILE_SYNCHRONOUS_IO_NONALERT, NTCREATEFILE_CREATE_OPTIONS, NtCreateFile,
    },
};
#[cfg(windows)]
use windows::Win32::{
    Foundation::{HANDLE, OBJ_CASE_INSENSITIVE, UNICODE_STRING},
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ, FILE_SHARE_READ,
        FILE_SHARE_WRITE, GetFileInformationByHandle,
    },
    System::IO::IO_STATUS_BLOCK,
};

#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    AssetRef, ClusterPolicy, ImageMatchConfig, ImageMatcher, ImageRuleVerification,
    ImageRuleVerificationArtifact, ImageRuleVerificationInput, MACRO_SCHEMA_VERSION,
    MacroDefinition, NegativeCorpusSample, NegativeSampleEvaluationInputs, RegionDefinition,
    TargetProfile, cluster_peaks,
};

static STORE_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
static STORE_ACTIVITY: OnceLock<Mutex<HashMap<PathBuf, Weak<StoreActivity>>>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedRevision {
    pub definition: MacroDefinition,
    pub definition_hash: String,
    pub pinned_assets: Vec<PinnedAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActiveRevisionIdentity {
    pub macro_id: String,
    pub revision: u64,
    pub definition_hash: String,
}

impl From<&SavedRevision> for ActiveRevisionIdentity {
    fn from(saved: &SavedRevision) -> Self {
        Self {
            macro_id: saved.definition.id.clone(),
            revision: saved.definition.revision,
            definition_hash: saved.definition_hash.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActiveRunIdentity {
    pub run_id: String,
}

impl ActiveRunIdentity {
    pub fn new(run_id: &str) -> Result<Self> {
        validate_component("run ID", run_id)?;
        Ok(Self {
            run_id: run_id.to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedMacroSummary {
    pub id: String,
    pub name: String,
    pub current_revision: u64,
    pub definition_hash: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedRevisionSummary {
    pub revision: u64,
    pub definition_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunHistorySummary {
    pub run_id: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MacroLifecycleMetadata {
    enabled: bool,
}

#[derive(Debug, Default)]
struct StoreActivity {
    revisions: Mutex<HashSet<ActiveRevisionIdentity>>,
    runs: Mutex<HashSet<ActiveRunIdentity>>,
}

pub(crate) struct ActiveRevisionLease {
    activity: Arc<StoreActivity>,
    identity: ActiveRevisionIdentity,
}

impl Drop for ActiveRevisionLease {
    fn drop(&mut self) {
        self.activity
            .revisions
            .lock()
            .expect("store activity poisoned")
            .remove(&self.identity);
    }
}

pub(crate) struct ActiveRunLease {
    activity: Arc<StoreActivity>,
    identity: ActiveRunIdentity,
}

impl Drop for ActiveRunLease {
    fn drop(&mut self) {
        self.activity
            .runs
            .lock()
            .expect("store activity poisoned")
            .remove(&self.identity);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PinnedAsset {
    pub asset: AssetRef,
    pub bytes: Vec<u8>,
}

pub const PACKAGE_SCHEMA_VERSION: u32 = 1;
const MAX_PACKAGE_IMAGE_RULES: usize = 128;
const MAX_PACKAGE_ASSETS: usize = 256;
const MAX_PACKAGE_MANIFEST_BYTES: u64 = 1_048_576;
const MAX_PACKAGE_DEFINITION_BYTES: u64 = 4_194_304;
const MAX_PACKAGE_ASSET_BYTES: u64 = 4_194_304;
const MAX_PACKAGE_TOTAL_ASSET_BYTES: u64 = 67_108_864;

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

#[derive(Debug)]
pub enum PreparedPackageImport {
    Text(PreparedTextImport),
    Image(PendingImageImport),
}

#[derive(Debug)]
pub struct PreparedTextImport {
    package: MacroPackage,
    package_fingerprint: String,
}

#[derive(Debug)]
pub struct PendingImageImport {
    package_fingerprint: String,
    plan_fingerprint: String,
    destination_state_fingerprint: String,
    source_macro_id: String,
    destination_macro_id: String,
    definition: MacroDefinition,
    image_rule_ids: Vec<String>,
    portable_assets: HashSet<AssetRef>,
}

impl PendingImageImport {
    pub fn definition(&self) -> &MacroDefinition {
        &self.definition
    }

    pub fn image_rule_ids(&self) -> &[String] {
        &self.image_rule_ids
    }
}

/// Native composition supplies a local capture set. The store binds the
/// replacement target and region to the pending rule and derives every score,
/// hash, cluster, and proof field from the supplied capture bytes.
pub struct LocalImageRuleVerificationInput<'a> {
    rule_id: &'a str,
    template_png: &'a [u8],
    mask_png: Option<&'a [u8]>,
    target: TargetProfile,
    region: RegionDefinition,
    target_region_png: &'a [u8],
    negative_samples: &'a [LocalNegativeImageSample<'a>],
    maximum_score_cells: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalNegativeImageSample<'a> {
    stable_id: String,
    png: &'a [u8],
}

impl<'a> LocalImageRuleVerificationInput<'a> {
    /// This construction boundary is crate-private so only the native capture
    /// composition can supply fresh local evidence to the persistence layer.
    pub(crate) fn from_local_capture(
        rule_id: &'a str,
        template_png: &'a [u8],
        mask_png: Option<&'a [u8]>,
        target: TargetProfile,
        region: RegionDefinition,
        target_region_png: &'a [u8],
        negative_samples: &'a [LocalNegativeImageSample<'a>],
        maximum_score_cells: u64,
    ) -> Self {
        Self {
            rule_id,
            template_png,
            mask_png,
            target,
            region,
            target_region_png,
            negative_samples,
            maximum_score_cells,
        }
    }
}

impl<'a> LocalNegativeImageSample<'a> {
    pub(crate) fn from_local_capture(stable_id: String, png: &'a [u8]) -> Self {
        Self { stable_id, png }
    }
}

#[derive(Debug)]
pub struct LocalImageRuleReverification {
    package_fingerprint: String,
    plan_fingerprint: String,
    destination_state_fingerprint: String,
    rule_id: String,
    target: TargetProfile,
    region: RegionDefinition,
    template: PackageAsset,
    mask: Option<PackageAsset>,
    artifact: ImageRuleVerificationArtifact,
}

/// Portable image-verification artifacts are untrusted until the destination
/// machine reruns verification against its own captured target and corpus.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("package image rules require local re-verification before import: {image_rule_ids:?}")]
pub struct LocalReverificationRequired {
    pub image_rule_ids: Vec<String>,
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
const STAGED_ASSET_REVISION_SCHEMA_VERSION: u32 = 1;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StagedAssetRevisionJournal {
    schema_version: u32,
    entries: Vec<StagedAssetRevision>,
}

impl Default for StagedAssetRevisionJournal {
    fn default() -> Self {
        Self {
            schema_version: STAGED_ASSET_REVISION_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StagedAssetRevision {
    previous: AssetRef,
    successor: AssetRef,
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
    staged_revision_journal: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl AssetStore {
    pub fn put_png(&self, bytes: &[u8]) -> Result<AssetRef> {
        let _guard = lock_store(&self.lock)?;
        let content_hash = sha256_hex(bytes);
        let index = self.load_index_locked()?;
        let id = next_captured_asset_id(&index.bindings, &content_hash)?;
        let asset = AssetRef {
            id,
            revision: 1,
            content_hash,
        };
        self.install_locked(asset.clone(), bytes)?;
        Ok(asset)
    }

    pub fn put_png_revision(&self, asset: AssetRef, bytes: &[u8]) -> Result<()> {
        let _guard = lock_store(&self.lock)?;
        self.install_locked(asset, bytes).map(|_| ())
    }

    /// Stores a recaptured template under the same logical identity at a new immutable revision.
    /// The previous binding and bytes remain readable for saved or running snapshots.
    pub fn put_next_png_revision(&self, previous: &AssetRef, bytes: &[u8]) -> Result<AssetRef> {
        let _guard = lock_store(&self.lock)?;
        let index = self.load_index_locked()?;
        validate_identity_binding(&index.bindings, previous)
            .context("previous template binding is unavailable or stale")?;
        self.verify_hash_file_locked(&previous.content_hash)
            .context("previous template bytes are unavailable or corrupt")?;
        let latest = index
            .bindings
            .iter()
            .filter(|binding| binding.id == previous.id)
            .map(|binding| binding.revision)
            .max()
            .context("previous template identity has no revisions")?;
        if latest != previous.revision {
            bail!(
                "stale template revision {} revision {}; latest is {}",
                previous.id,
                previous.revision,
                latest
            );
        }
        let revision = previous
            .revision
            .checked_add(1)
            .context("template asset revision overflow")?;
        let asset = AssetRef {
            id: previous.id.clone(),
            revision,
            content_hash: sha256_hex(bytes),
        };
        self.install_locked(asset.clone(), bytes)?;
        Ok(asset)
    }

    /// Publishes immutable successor bytes provisionally. The successor remains reserved until a
    /// durable macro definition references it; rejected authoring must discard it explicitly, and
    /// interrupted authoring is recovered when the store is reopened.
    pub fn stage_next_png_revision(&self, previous: &AssetRef, bytes: &[u8]) -> Result<AssetRef> {
        let _guard = lock_store(&self.lock)?;
        self.stage_next_png_revision_locked(previous, bytes)
    }

    /// Replaces provisional descendants before staging a successor from `previous`. This is only
    /// for the UI-serialized authoring owner after undo/abandon restores an older draft revision;
    /// concurrent capture callers must use [`Self::stage_next_png_revision`] instead.
    pub fn replace_staged_png_revision(
        &self,
        previous: &AssetRef,
        bytes: &[u8],
    ) -> Result<AssetRef> {
        let _guard = lock_store(&self.lock)?;
        let mut journal = self.load_staged_revision_journal_locked()?;
        let mut descendants = journal
            .entries
            .iter()
            .filter(|entry| {
                entry.successor.id == previous.id && entry.successor.revision > previous.revision
            })
            .cloned()
            .collect::<Vec<_>>();
        if descendants.is_empty() {
            return self.stage_next_png_revision_locked(previous, bytes);
        }
        let staged_identities = descendants
            .iter()
            .map(|entry| (entry.successor.id.clone(), entry.successor.revision))
            .collect::<HashSet<_>>();
        let index = self.load_index_locked()?;
        self.ensure_staged_revisions_are_not_durable_locked(&index, &descendants)?;
        if index.bindings.iter().any(|binding| {
            binding.id == previous.id
                && binding.revision > previous.revision
                && !staged_identities.contains(&(binding.id.clone(), binding.revision))
        }) {
            bail!(
                "stale template revision {} revision {}; a durable successor exists",
                previous.id,
                previous.revision
            );
        }
        descendants.sort_by_key(|entry| std::cmp::Reverse(entry.successor.revision));
        for staged in &descendants {
            self.rollback_staged_revision_locked(staged)?;
        }
        journal.entries.retain(|entry| {
            !staged_identities.contains(&(entry.successor.id.clone(), entry.successor.revision))
        });
        self.write_staged_revision_journal_locked(&journal)?;
        self.stage_next_png_revision_locked(previous, bytes)
    }

    fn stage_next_png_revision_locked(
        &self,
        previous: &AssetRef,
        bytes: &[u8],
    ) -> Result<AssetRef> {
        let index = self.load_index_locked()?;
        validate_identity_binding(&index.bindings, previous)
            .context("previous template binding is unavailable or stale")?;
        self.verify_hash_file_locked(&previous.content_hash)
            .context("previous template bytes are unavailable or corrupt")?;
        let latest = index
            .bindings
            .iter()
            .filter(|binding| binding.id == previous.id)
            .map(|binding| binding.revision)
            .max()
            .context("previous template identity has no revisions")?;
        if latest != previous.revision {
            bail!(
                "stale template revision {} revision {}; latest is {}",
                previous.id,
                previous.revision,
                latest
            );
        }
        let revision = previous
            .revision
            .checked_add(1)
            .context("template asset revision overflow")?;
        let successor = AssetRef {
            id: previous.id.clone(),
            revision,
            content_hash: sha256_hex(bytes),
        };
        let mut journal = self.load_staged_revision_journal_locked()?;
        if journal.entries.iter().any(|entry| {
            entry.successor.id == successor.id && entry.successor.revision == successor.revision
        }) {
            bail!(
                "staged template revision {} revision {} already exists",
                successor.id,
                successor.revision
            );
        }
        journal.entries.push(StagedAssetRevision {
            previous: previous.clone(),
            successor: successor.clone(),
        });
        self.write_staged_revision_journal_locked(&journal)?;
        if let Err(error) = self.install_locked(successor.clone(), bytes) {
            journal.entries.pop();
            let _ = self.write_staged_revision_journal_locked(&journal);
            return Err(error);
        }
        Ok(successor)
    }

    pub fn discard_staged_png_revision(&self, successor: &AssetRef) -> Result<()> {
        let _guard = lock_store(&self.lock)?;
        let mut journal = self.load_staged_revision_journal_locked()?;
        let Some(index) = journal
            .entries
            .iter()
            .position(|entry| entry.successor == *successor)
        else {
            bail!(
                "asset {} revision {} is not a staged template revision",
                successor.id,
                successor.revision
            );
        };
        let staged = journal.entries[index].clone();
        let identities = self.load_index_locked()?;
        self.ensure_staged_revisions_are_not_durable_locked(
            &identities,
            std::slice::from_ref(&staged),
        )?;
        self.rollback_staged_revision_locked(&staged)?;
        journal.entries.remove(index);
        self.write_staged_revision_journal_locked(&journal)
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

    fn load_staged_revision_journal_locked(&self) -> Result<StagedAssetRevisionJournal> {
        if !self.staged_revision_journal.exists() {
            return Ok(StagedAssetRevisionJournal::default());
        }
        let journal: StagedAssetRevisionJournal =
            serde_json::from_slice(&fs::read(&self.staged_revision_journal)?)
                .context("corrupt staged asset revision journal")?;
        if journal.schema_version != STAGED_ASSET_REVISION_SCHEMA_VERSION {
            bail!(
                "unsupported staged asset revision schema {}",
                journal.schema_version
            );
        }
        let mut identities = HashSet::new();
        for entry in &journal.entries {
            validate_asset_ref(&entry.previous)?;
            validate_asset_ref(&entry.successor)?;
            if entry.previous.id != entry.successor.id
                || entry.previous.revision.checked_add(1) != Some(entry.successor.revision)
            {
                bail!("invalid staged asset revision relationship");
            }
            if !identities.insert((entry.successor.id.clone(), entry.successor.revision)) {
                bail!("duplicate staged asset revision identity");
            }
        }
        Ok(journal)
    }

    fn write_staged_revision_journal_locked(
        &self,
        journal: &StagedAssetRevisionJournal,
    ) -> Result<()> {
        if journal.entries.is_empty() {
            if self.staged_revision_journal.exists() {
                fs::remove_file(&self.staged_revision_journal)?;
                sync_directory(
                    self.staged_revision_journal
                        .parent()
                        .context("staged revision journal has no parent")?,
                )?;
            }
            return Ok(());
        }
        atomic_write(
            &self.staged_revision_journal,
            &serde_json::to_vec_pretty(journal)?,
        )
    }

    fn rollback_staged_revision_locked(&self, staged: &StagedAssetRevision) -> Result<()> {
        let mut index = self.load_index_locked()?;
        if let Some(binding_index) = index.bindings.iter().position(|binding| {
            binding.id == staged.successor.id && binding.revision == staged.successor.revision
        }) {
            if index.bindings[binding_index] != staged.successor {
                bail!("staged asset identity conflicts with durable identity binding");
            }
            self.verify_hash_file_locked(&staged.successor.content_hash)
                .context("staged template bytes are unavailable or corrupt")?;
            index.bindings.remove(binding_index);
            self.write_index_locked(&index)?;
        }
        let path = self.path_for_hash(&staged.successor.content_hash)?;
        if path.exists()
            && !index
                .bindings
                .iter()
                .any(|binding| binding.content_hash == staged.successor.content_hash)
        {
            self.verify_hash_file_locked(&staged.successor.content_hash)
                .context("staged template bytes are unavailable or corrupt")?;
            fs::remove_file(path)?;
            sync_directory(&self.root)?;
        }
        Ok(())
    }

    fn ensure_staged_revisions_are_not_durable_locked(
        &self,
        index: &AssetIdentityIndex,
        staged: &[StagedAssetRevision],
    ) -> Result<()> {
        let durable = self.durable_definition_asset_refs_locked(index)?;
        let durable_latest = latest_asset_revisions(durable.iter());
        if let Some(entry) = staged.iter().find(|entry| {
            durable_latest
                .get(entry.successor.id.as_str())
                .is_some_and(|revision| *revision >= entry.successor.revision)
        }) {
            bail!(
                "durable definition references template {} revision {}; staged rollback is blocked",
                entry.successor.id,
                entry.successor.revision
            );
        }
        Ok(())
    }

    fn durable_definition_asset_refs_locked(
        &self,
        index: &AssetIdentityIndex,
    ) -> Result<HashSet<AssetRef>> {
        let mut durable = HashSet::new();
        let definitions_root = self
            .root
            .parent()
            .context("macro asset root has no store parent")?
            .join("definitions");
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
                        .and_then(|extension| extension.to_str())
                        != Some("json")
                {
                    continue;
                }
                let file_name = revision_file.file_name().to_string_lossy().into_owned();
                let bytes = fs::read(revision_file.path())?;
                let definition: MacroDefinition = serde_json::from_slice(&bytes)
                    .context("corrupt saved macro definition blocks staged asset recovery")?;
                if definition.schema_version != MACRO_SCHEMA_VERSION {
                    bail!(
                        "unsupported macro schema {} blocks staged asset recovery",
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
                    self.verify_hash_file_locked(&asset.content_hash)?;
                    durable.insert(asset.clone());
                }
            }
        }
        Ok(durable)
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

fn next_captured_asset_id(bindings: &[AssetRef], content_hash: &str) -> Result<String> {
    let prefix = content_hash
        .get(..16)
        .context("captured asset content hash is invalid")?;
    for ordinal in 1_u64.. {
        let id = format!("captured-{prefix}-{ordinal}");
        if !bindings.iter().any(|binding| binding.id == id) {
            return Ok(id);
        }
    }
    unreachable!("u64 asset identity space exhausted")
}

#[derive(Debug, Clone)]
pub struct MacroStore {
    root: PathBuf,
    assets: AssetStore,
    lock: Arc<Mutex<()>>,
    activity: Arc<StoreActivity>,
    #[cfg(test)]
    fail_import_after_assets: Arc<AtomicBool>,
    #[cfg(test)]
    fail_staged_finalize_cleanup: Arc<AtomicBool>,
}

impl MacroStore {
    pub fn open(root: &Path) -> Result<Self> {
        let root = root.join("macro_data");
        fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        let (lock, first_live_owner) = shared_store_lock(&root)?;
        let activity = shared_store_activity(&root)?;
        let assets = AssetStore {
            root: root.join("assets"),
            identity_index: root.join("asset_identities.json"),
            staged_revision_journal: root.join("staged_asset_revisions.json"),
            lock: Arc::clone(&lock),
        };
        fs::create_dir_all(root.join("definitions"))?;
        fs::create_dir_all(&assets.root)?;
        fs::create_dir_all(root.join("runs"))?;
        let store = Self {
            root,
            assets,
            lock,
            activity,
            #[cfg(test)]
            fail_import_after_assets: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            fail_staged_finalize_cleanup: Arc::new(AtomicBool::new(false)),
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
            store.assets.load_staged_revision_journal_locked()?;
            if first_live_owner {
                store.recover_staged_asset_revisions_locked()?;
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

    /// Validates and compiles the exact immutable candidate before publishing it.
    /// This is the application-facing save path; `save` remains the low-level
    /// persistence primitive used by migration-free fixture and recovery tests.
    pub fn save_validated(&self, definition: MacroDefinition) -> Result<SavedRevision> {
        let _guard = lock_store(&self.lock)?;
        self.save_validated_locked(definition)
    }

    fn save_validated_locked(&self, definition: MacroDefinition) -> Result<SavedRevision> {
        validate_macro_name(&definition.name)?;
        let directory = self.root.join("definitions").join(&definition.id);
        if directory.join("current.json").exists() {
            let current = self.load_current_locked(&definition.id)?;
            if definition.revision <= current.definition.revision {
                bail!(
                    "validated save revision {} must be newer than current revision {}",
                    definition.revision,
                    current.definition.revision
                );
            }
        } else if definition.revision == 0 {
            bail!("validated save revision must be positive");
        }
        let candidate = self.assemble_saved_locked(definition.clone())?;
        super::CompiledMacro::compile(candidate)
            .context("validated save candidate does not compile")?;
        self.save_locked(definition)
    }

    pub fn list_macros(&self) -> Result<Vec<SavedMacroSummary>> {
        let _guard = lock_store(&self.lock)?;
        let mut summaries = Vec::new();
        for entry in fs::read_dir(self.root.join("definitions"))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if id.starts_with(".deleting-") {
                continue;
            }
            validate_component("saved macro directory", &id)?;
            let saved = self.load_current_locked(&id)?;
            summaries.push(SavedMacroSummary {
                id,
                name: saved.definition.name,
                current_revision: saved.definition.revision,
                definition_hash: saved.definition_hash,
                enabled: self.load_enabled_locked(&saved.definition.id)?,
            });
        }
        summaries.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(summaries)
    }

    pub fn load_current(&self, macro_id: &str) -> Result<SavedRevision> {
        let _guard = lock_store(&self.lock)?;
        self.load_current_locked(macro_id)
    }

    fn load_current_locked(&self, macro_id: &str) -> Result<SavedRevision> {
        validate_component("macro ID", macro_id)?;
        let directory = self.root.join("definitions").join(macro_id);
        let current_path = directory.join("current.json");
        let current_bytes = fs::read(&current_path)
            .with_context(|| format!("current revision is unavailable for macro '{macro_id}'"))?;
        let definition: MacroDefinition =
            serde_json::from_slice(&current_bytes).context("current revision JSON is corrupt")?;
        if definition.id != macro_id {
            bail!("current revision macro ID does not match its directory");
        }
        let immutable_path = directory.join(format!("{}.json", definition.revision));
        let immutable_bytes =
            fs::read(&immutable_path).context("current immutable revision file is missing")?;
        if current_bytes != immutable_bytes {
            bail!("current revision does not match immutable revision bytes");
        }
        self.load_revision_bytes_locked(macro_id, definition.revision, immutable_bytes)
    }

    pub fn load_revision(&self, macro_id: &str, revision: u64) -> Result<SavedRevision> {
        let _guard = lock_store(&self.lock)?;
        self.load_revision_locked(macro_id, revision)
    }

    fn load_revision_locked(&self, macro_id: &str, revision: u64) -> Result<SavedRevision> {
        validate_component("macro ID", macro_id)?;
        if revision == 0 {
            bail!("revision must be positive");
        }
        let path = self
            .root
            .join("definitions")
            .join(macro_id)
            .join(format!("{revision}.json"));
        let bytes = fs::read(&path).context("immutable revision is unavailable")?;
        self.load_revision_bytes_locked(macro_id, revision, bytes)
    }

    fn load_revision_bytes_locked(
        &self,
        macro_id: &str,
        revision: u64,
        bytes: Vec<u8>,
    ) -> Result<SavedRevision> {
        let directory = self.root.join("definitions").join(macro_id);
        let path = directory.join(format!("{revision}.json"));
        let checksum = fs::read_to_string(revision_checksum_path(&path)?)
            .context("immutable revision checksum is unavailable")?;
        let actual_hash = sha256_hex(&bytes);
        if checksum.trim() != actual_hash {
            bail!("immutable revision checksum mismatch");
        }
        let definition: MacroDefinition =
            serde_json::from_slice(&bytes).context("immutable revision JSON is corrupt")?;
        if definition.schema_version != MACRO_SCHEMA_VERSION {
            bail!("unsupported macro schema {}", definition.schema_version);
        }
        if definition.id != macro_id || definition.revision != revision {
            bail!("immutable revision identity does not match its path");
        }
        let saved = self.assemble_saved_locked(definition)?;
        if saved.definition_hash != actual_hash {
            bail!("immutable revision canonical hash mismatch");
        }
        super::CompiledMacro::compile(saved.clone())
            .context("stored immutable revision does not compile")?;
        Ok(saved)
    }

    fn assemble_saved_locked(&self, definition: MacroDefinition) -> Result<SavedRevision> {
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
        Ok(SavedRevision {
            definition,
            definition_hash: sha256_hex(&bytes),
            pinned_assets,
        })
    }

    pub fn revision_history(&self, macro_id: &str) -> Result<Vec<SavedRevisionSummary>> {
        let _guard = lock_store(&self.lock)?;
        validate_component("macro ID", macro_id)?;
        let mut revisions = Vec::new();
        let directory = self.root.join("definitions").join(macro_id);
        for entry in fs::read_dir(&directory).context("macro is unavailable")? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Ok(revision) = stem.parse::<u64>() else {
                continue;
            };
            let saved = self.load_revision_locked(macro_id, revision)?;
            revisions.push(SavedRevisionSummary {
                revision,
                definition_hash: saved.definition_hash,
            });
        }
        revisions.sort_by_key(|entry| std::cmp::Reverse(entry.revision));
        Ok(revisions)
    }

    pub fn rename_macro(
        &self,
        macro_id: &str,
        expected_current_hash: &str,
        new_name: &str,
    ) -> Result<SavedRevision> {
        let _guard = lock_store(&self.lock)?;
        validate_macro_name(new_name)?;
        let current = self.load_current_locked(macro_id)?;
        if current.definition_hash != expected_current_hash {
            bail!("macro changed since it was loaded");
        }
        let mut definition = current.definition;
        definition.name = new_name.trim().to_string();
        definition.revision = definition
            .revision
            .checked_add(1)
            .context("macro revision overflow")?;
        self.save_validated_locked(definition)
    }

    pub fn duplicate_macro(
        &self,
        source_macro_id: &str,
        expected_current_hash: &str,
        new_macro_id: &str,
        new_name: &str,
    ) -> Result<SavedRevision> {
        let _guard = lock_store(&self.lock)?;
        validate_component("new macro ID", new_macro_id)?;
        validate_macro_name(new_name)?;
        if self.root.join("definitions").join(new_macro_id).exists() {
            bail!("macro ID already exists: {new_macro_id}");
        }
        let current = self.load_current_locked(source_macro_id)?;
        if current.definition_hash != expected_current_hash {
            bail!("macro changed since it was loaded");
        }
        let mut definition = current.definition;
        definition.id = new_macro_id.to_string();
        definition.name = new_name.trim().to_string();
        definition.revision = 1;
        self.save_validated_locked(definition)
    }

    pub fn set_macro_enabled(&self, macro_id: &str, enabled: bool) -> Result<()> {
        let _guard = lock_store(&self.lock)?;
        self.load_current_locked(macro_id)?;
        atomic_write(
            &self
                .root
                .join("definitions")
                .join(macro_id)
                .join("lifecycle.json"),
            &serde_json::to_vec_pretty(&MacroLifecycleMetadata { enabled })?,
        )
    }

    pub(crate) fn acquire_active_revision(
        &self,
        saved: &SavedRevision,
    ) -> Result<ActiveRevisionLease> {
        let identity = ActiveRevisionIdentity::from(saved);
        let mut active = self
            .activity
            .revisions
            .lock()
            .map_err(|_| anyhow::anyhow!("store activity poisoned"))?;
        if !active.insert(identity.clone()) {
            bail!("saved revision is already active");
        }
        drop(active);
        Ok(ActiveRevisionLease {
            activity: Arc::clone(&self.activity),
            identity,
        })
    }

    pub(crate) fn acquire_current_for_run(
        &self,
        macro_id: &str,
    ) -> Result<(SavedRevision, ActiveRevisionLease)> {
        let _guard = lock_store(&self.lock)?;
        validate_component("macro ID", macro_id)?;
        if !self.load_enabled_locked(macro_id)? {
            bail!("saved macro is disabled");
        }
        let saved = self.load_current_locked(macro_id)?;
        let lease = self.acquire_active_revision(&saved)?;
        Ok((saved, lease))
    }

    pub(crate) fn acquire_active_run(&self, run_id: &str) -> Result<ActiveRunLease> {
        let identity = ActiveRunIdentity::new(run_id)?;
        let mut active = self
            .activity
            .runs
            .lock()
            .map_err(|_| anyhow::anyhow!("store activity poisoned"))?;
        if !active.insert(identity.clone()) {
            bail!("run history is already active");
        }
        drop(active);
        Ok(ActiveRunLease {
            activity: Arc::clone(&self.activity),
            identity,
        })
    }

    fn load_enabled_locked(&self, macro_id: &str) -> Result<bool> {
        let path = self
            .root
            .join("definitions")
            .join(macro_id)
            .join("lifecycle.json");
        if !path.exists() {
            return Ok(true);
        }
        let metadata: MacroLifecycleMetadata = serde_json::from_slice(&fs::read(path)?)
            .context("macro lifecycle metadata is corrupt")?;
        Ok(metadata.enabled)
    }

    pub fn delete_macro(&self, macro_id: &str) -> Result<()> {
        let _guard = lock_store(&self.lock)?;
        validate_component("macro ID", macro_id)?;
        if self
            .activity
            .revisions
            .lock()
            .map_err(|_| anyhow::anyhow!("store activity poisoned"))?
            .iter()
            .any(|active| active.macro_id == macro_id)
        {
            bail!("cannot delete the active macro snapshot");
        }
        self.load_current_locked(macro_id)?;
        let definitions = self.root.join("definitions");
        let source = definitions.join(macro_id);
        let mut ordinal = 1_u64;
        let tombstone = loop {
            let candidate = definitions.join(format!(".deleting-{macro_id}-{ordinal}"));
            if !candidate.exists() {
                break candidate;
            }
            ordinal = ordinal
                .checked_add(1)
                .context("delete tombstone overflow")?;
        };
        fs::rename(&source, &tombstone).context("could not stage macro deletion")?;
        if let Err(error) = fs::remove_dir_all(&tombstone) {
            let _ = fs::rename(&tombstone, &source);
            return Err(error).context("could not delete macro directory");
        }
        sync_directory(&definitions)?;
        Ok(())
    }

    pub fn list_run_history(&self) -> Result<Vec<RunHistorySummary>> {
        let _guard = lock_store(&self.lock)?;
        let mut history = Vec::new();
        for entry in fs::read_dir(self.root.join("runs"))? {
            let entry = entry?;
            if !entry.file_type()?.is_file()
                || entry
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    != Some("jsonl")
            {
                continue;
            }
            let run_id = entry
                .path()
                .file_stem()
                .and_then(|stem| stem.to_str())
                .context("run history filename is invalid")?
                .to_string();
            validate_component("run ID", &run_id)?;
            history.push(RunHistorySummary {
                run_id,
                bytes: entry.metadata()?.len(),
            });
        }
        history.sort_by(|left, right| right.run_id.cmp(&left.run_id));
        Ok(history)
    }

    pub fn load_run_history(&self, run_id: &str) -> Result<Vec<JournalRecord>> {
        let _guard = lock_store(&self.lock)?;
        validate_component("run ID", run_id)?;
        let path = self.root.join("runs").join(format!("{run_id}.jsonl"));
        let file = File::open(path).context("run history is unavailable")?;
        let mut records: Vec<JournalRecord> = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            records.push(serde_json::from_str(&line).context("run history record is corrupt")?);
        }
        for pair in records.windows(2) {
            if pair[0].sequence >= pair[1].sequence {
                bail!("run history sequence is not strictly ordered");
            }
        }
        Ok(records)
    }

    pub fn delete_run_history(&self, run_id: &str) -> Result<()> {
        let _guard = lock_store(&self.lock)?;
        validate_component("run ID", run_id)?;
        if self
            .activity
            .runs
            .lock()
            .map_err(|_| anyhow::anyhow!("store activity poisoned"))?
            .contains(&ActiveRunIdentity::new(run_id)?)
        {
            bail!("cannot delete active run history");
        }
        fs::remove_file(self.root.join("runs").join(format!("{run_id}.jsonl")))
            .context("run history is unavailable")?;
        Ok(())
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
        let staged_after_save = self.prepare_staged_asset_finalization_locked(&definition)?;

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
        #[cfg(test)]
        let skip_staged_cleanup = self.fail_staged_finalize_cleanup.load(Ordering::SeqCst);
        #[cfg(not(test))]
        let skip_staged_cleanup = false;
        if !skip_staged_cleanup {
            let _ = self
                .assets
                .write_staged_revision_journal_locked(&staged_after_save);
        }
        Ok(saved)
    }

    fn recover_staged_asset_revisions_locked(&self) -> Result<()> {
        let mut journal = self.assets.load_staged_revision_journal_locked()?;
        if journal.entries.is_empty() {
            return Ok(());
        }
        let index = self.assets.load_index_locked()?;
        let durable = self.durable_definition_asset_refs_locked(&index)?;
        let durable_latest = latest_asset_revisions(durable.iter());
        journal.entries.sort_by(|left, right| {
            right
                .successor
                .revision
                .cmp(&left.successor.revision)
                .then_with(|| left.successor.id.cmp(&right.successor.id))
        });
        for staged in &journal.entries {
            let is_durable = durable_latest
                .get(staged.successor.id.as_str())
                .is_some_and(|revision| *revision >= staged.successor.revision);
            if is_durable {
                validate_identity_binding(&index.bindings, &staged.successor).context(
                    "durable definition references an unavailable staged template revision",
                )?;
                self.assets
                    .verify_hash_file_locked(&staged.successor.content_hash)
                    .context("durable staged template bytes are unavailable or corrupt")?;
            } else {
                self.assets.rollback_staged_revision_locked(staged)?;
            }
        }
        journal.entries.clear();
        self.assets.write_staged_revision_journal_locked(&journal)
    }

    fn prepare_staged_asset_finalization_locked(
        &self,
        definition: &MacroDefinition,
    ) -> Result<StagedAssetRevisionJournal> {
        let mut journal = self.assets.load_staged_revision_journal_locked()?;
        if journal.entries.is_empty() {
            return Ok(journal);
        }
        let referenced_latest = latest_asset_revisions(referenced_assets(definition));
        let index = self.assets.load_index_locked()?;
        for staged in &journal.entries {
            for asset in [&staged.previous, &staged.successor] {
                validate_identity_binding(&index.bindings, asset)
                    .context("staged asset finalization has an unavailable identity binding")?;
                self.assets
                    .verify_hash_file_locked(&asset.content_hash)
                    .context("staged asset finalization bytes are unavailable or corrupt")?;
            }
        }
        journal.entries.retain(|staged| {
            !referenced_latest
                .get(staged.successor.id.as_str())
                .is_some_and(|revision| *revision >= staged.successor.revision)
        });
        Ok(journal)
    }

    fn durable_definition_asset_refs_locked(
        &self,
        index: &AssetIdentityIndex,
    ) -> Result<HashSet<AssetRef>> {
        self.assets.durable_definition_asset_refs_locked(index)
    }

    pub fn validate_package(package_root: &Path) -> Result<MacroPackage> {
        let root = open_package_root(package_root)?;
        let manifest: PackageManifest = serde_json::from_slice(&read_bounded_package_file(
            &root,
            Path::new("manifest.json"),
            MAX_PACKAGE_MANIFEST_BYTES,
            "package manifest",
        )?)
        .context("corrupt package manifest JSON")?;
        if manifest.schema_version != PACKAGE_SCHEMA_VERSION {
            bail!("unsupported package schema {}", manifest.schema_version);
        }
        if manifest.assets.len() > MAX_PACKAGE_ASSETS {
            bail!(
                "package asset count {} exceeds maximum {MAX_PACKAGE_ASSETS}",
                manifest.assets.len()
            );
        }
        validate_package_relative(&manifest.definition)?;
        for asset in &manifest.assets {
            validate_package_relative(&asset.path)?;
        }

        let definition: MacroDefinition = serde_json::from_slice(&read_bounded_package_file(
            &root,
            &manifest.definition,
            MAX_PACKAGE_DEFINITION_BYTES,
            "package definition",
        )?)
        .context("corrupt macro JSON")?;
        if definition.schema_version != MACRO_SCHEMA_VERSION {
            bail!("unsupported macro schema {}", definition.schema_version);
        }
        validate_component("macro ID", &definition.id)?;
        validate_identity_set(referenced_assets(&definition))?;
        validate_identity_set(manifest.assets.iter().map(|entry| &entry.asset))?;

        let mut assets = Vec::with_capacity(manifest.assets.len());
        let mut manifest_refs = HashSet::new();
        let mut total_asset_bytes = 0_u64;
        for entry in manifest.assets {
            if !manifest_refs.insert(entry.asset.clone()) {
                bail!("duplicate asset entry in package manifest");
            }
            let bytes = read_bounded_package_file(
                &root,
                &entry.path,
                MAX_PACKAGE_ASSET_BYTES,
                "package asset",
            )?;
            total_asset_bytes = total_asset_bytes
                .checked_add(bytes.len() as u64)
                .context("package asset byte total overflow")?;
            if total_asset_bytes > MAX_PACKAGE_TOTAL_ASSET_BYTES {
                bail!("package asset bytes exceed maximum {MAX_PACKAGE_TOTAL_ASSET_BYTES}");
            }
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

    /// Exports only the checked current immutable revision owned by this store.
    /// Callers cannot substitute an editor draft or a forged `SavedRevision`.
    pub fn export_current_package(&self, macro_id: &str, package_root: &Path) -> Result<()> {
        let _guard = lock_store(&self.lock)?;
        let saved = self.load_current_locked(macro_id)?;
        self.export_package_locked(&saved, package_root)
    }

    /// Exports a saved identity only when it is still the current immutable revision.
    pub fn export_current_package_checked(
        &self,
        macro_id: &str,
        expected_revision: u64,
        expected_current_hash: &str,
        package_root: &Path,
    ) -> Result<()> {
        let _guard = lock_store(&self.lock)?;
        let saved = self.load_current_locked(macro_id)?;
        if saved.definition.revision != expected_revision
            || saved.definition_hash != expected_current_hash
        {
            bail!("macro changed since it was loaded");
        }
        self.export_package_locked(&saved, package_root)
    }

    #[cfg(test)]
    fn export_package(&self, saved: &SavedRevision, package_root: &Path) -> Result<()> {
        let _guard = lock_store(&self.lock)?;
        self.export_package_locked(saved, package_root)
    }

    fn export_package_locked(&self, saved: &SavedRevision, package_root: &Path) -> Result<()> {
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

    pub fn prepare_package_import(&self, package_root: &Path) -> Result<PreparedPackageImport> {
        let package = Self::validate_package(package_root)?;
        self.prepare_validated_package_import(package)
    }

    fn prepare_validated_package_import(
        &self,
        mut package: MacroPackage,
    ) -> Result<PreparedPackageImport> {
        validate_package_memory(&package)?;
        let fingerprint = package_fingerprint(&package)?;
        if package.definition.image_rules.is_empty() {
            validate_package_compiles(&package)?;
            return Ok(PreparedPackageImport::Text(PreparedTextImport {
                package,
                package_fingerprint: fingerprint,
            }));
        }
        if package.definition.image_rules.len() > MAX_PACKAGE_IMAGE_RULES {
            bail!(
                "package image rule count {} exceeds maximum {MAX_PACKAGE_IMAGE_RULES}",
                package.definition.image_rules.len()
            );
        }

        // Portable proof is intentionally discarded before any validation that
        // could treat it as executable/trusted verifier output.
        for rule in &mut package.definition.image_rules {
            rule.verification = None;
        }
        validate_image_import_structure(&package)?;
        let _guard = lock_store(&self.lock)?;
        let existing_macro_ids = self.existing_macro_ids_locked()?;
        let source_macro_id = package.definition.id.clone();
        let destination_macro_id = remap_id(&source_macro_id, &existing_macro_ids);
        package.definition.id = destination_macro_id.clone();
        let destination_state_fingerprint = self.import_destination_fingerprint_locked()?;
        let image_rule_ids = package
            .definition
            .image_rules
            .iter()
            .map(|rule| rule.id.clone())
            .collect::<Vec<_>>();
        let portable_assets = package
            .assets
            .iter()
            .map(|asset| asset.asset.clone())
            .collect::<HashSet<_>>();
        let plan_fingerprint = pending_image_plan_fingerprint(
            &fingerprint,
            &destination_state_fingerprint,
            &source_macro_id,
            &destination_macro_id,
            &package.definition,
            &portable_assets,
        )?;
        Ok(PreparedPackageImport::Image(PendingImageImport {
            package_fingerprint: fingerprint,
            plan_fingerprint,
            destination_state_fingerprint,
            source_macro_id,
            destination_macro_id,
            definition: package.definition,
            image_rule_ids,
            portable_assets,
        }))
    }

    pub fn commit_text_package_import(
        &self,
        prepared: PreparedTextImport,
    ) -> Result<SavedRevision> {
        if package_fingerprint(&prepared.package)? != prepared.package_fingerprint {
            bail!("prepared text package fingerprint changed");
        }
        self.import_validated_package(prepared.package)
    }

    pub fn import_package(&self, package_root: &Path) -> Result<SavedRevision> {
        let package = Self::validate_package(package_root)?;
        if !package.definition.image_rules.is_empty() {
            return Err(LocalReverificationRequired {
                image_rule_ids: package
                    .definition
                    .image_rules
                    .iter()
                    .map(|rule| rule.id.clone())
                    .collect(),
            }
            .into());
        }
        match self.prepare_validated_package_import(package)? {
            PreparedPackageImport::Text(prepared) => self.commit_text_package_import(prepared),
            PreparedPackageImport::Image(_) => unreachable!("image packages returned above"),
        }
    }

    pub fn complete_local_image_reverification(
        &self,
        pending: &PendingImageImport,
        input: LocalImageRuleVerificationInput<'_>,
    ) -> Result<LocalImageRuleReverification> {
        let _guard = lock_store(&self.lock)?;
        self.recheck_pending_image_import_locked(pending)?;
        let mut definition = pending.definition.clone();
        let rule_index = definition
            .image_rules
            .iter()
            .position(|rule| rule.id == input.rule_id)
            .with_context(|| format!("pending image rule '{}' does not exist", input.rule_id))?;
        if input.target.captured_client_width == 0
            || input.target.captured_client_height == 0
            || input.target.captured_dpi == 0
        {
            bail!("local target capture must provide non-zero client dimensions and DPI");
        }
        if input.region.id != definition.image_rules[rule_index].region_id {
            bail!("local image recapture must confirm the rule's region identity");
        }
        if !input.region.rect.x.is_finite()
            || !input.region.rect.y.is_finite()
            || !input.region.rect.width.is_finite()
            || !input.region.rect.height.is_finite()
            || input.region.rect.x < 0.0
            || input.region.rect.y < 0.0
            || input.region.rect.width <= 0.0
            || input.region.rect.height <= 0.0
            || input.region.rect.x + input.region.rect.width > 1.0
            || input.region.rect.y + input.region.rect.height > 1.0
        {
            bail!("local image recapture has an invalid region geometry");
        }
        if definition.image_rules[rule_index]
            .transparent_mask
            .is_some()
            != input.mask_png.is_some()
        {
            bail!("local image recapture must preserve transparent-mask presence");
        }
        let portable_identities = pending.portable_assets.iter().cloned().collect::<Vec<_>>();
        let index = self.assets.load_index_locked()?;
        let template = fresh_local_import_asset(
            &index.bindings,
            &portable_identities,
            &pending.package_fingerprint,
            input.rule_id,
            "template",
            input.template_png,
        )?;
        let mut reserved = portable_identities;
        reserved.push(template.asset.clone());
        let mask = input
            .mask_png
            .map(|bytes| {
                fresh_local_import_asset(
                    &index.bindings,
                    &reserved,
                    &pending.package_fingerprint,
                    input.rule_id,
                    "mask",
                    bytes,
                )
            })
            .transpose()?;

        definition.target = input.target.clone();
        let region = definition
            .regions
            .iter_mut()
            .find(|region| region.id == input.region.id)
            .context("pending image rule region is missing")?;
        *region = input.region.clone();
        {
            let rule = &mut definition.image_rules[rule_index];
            rule.template = template.asset.clone();
            rule.transparent_mask = mask.as_ref().map(|asset| asset.asset.clone());
            rule.verification = None;
        }
        let rule = &definition.image_rules[rule_index];
        let region = definition
            .regions
            .iter()
            .find(|region| region.id == rule.region_id)
            .context("pending image rule region is missing")?;
        let client = crate::engine::types::Rect::new(
            0,
            0,
            definition.target.captured_client_width,
            definition.target.captured_client_height,
        );
        let search = client.rect_from_ratio(region.rect);
        let template_image = ImageRuleVerification::decode_template_png(&template.bytes)?;
        let mask_image = mask
            .as_ref()
            .map(|asset| ImageRuleVerification::decode_mask_png(&asset.bytes))
            .transpose()?;
        let target_region = super::image_verification::decode_search_png(input.target_region_png)?;
        if target_region.dimensions() != (search.width, search.height) {
            bail!("local target capture dimensions do not match the confirmed region");
        }
        let matcher = ImageMatcher;
        let match_config = ImageMatchConfig {
            threshold: rule.threshold,
            scales_percent: rule.scales_percent.clone(),
        };
        let observed_clusters = cluster_peaks(
            matcher
                .match_template_masked(
                    &target_region,
                    &template_image,
                    mask_image.as_ref(),
                    &match_config,
                )?
                .candidates,
            ClusterPolicy::default(),
        )?;
        let negative_samples = input
            .negative_samples
            .iter()
            .map(|sample| -> Result<NegativeCorpusSample> {
                let negative = super::image_verification::decode_search_png(sample.png)?;
                let measured_score = matcher
                    .match_template_masked(
                        &negative,
                        &template_image,
                        mask_image.as_ref(),
                        &match_config,
                    )?
                    .best
                    .score;
                Ok(NegativeCorpusSample {
                    stable_id: sample.stable_id.clone(),
                    content_sha256: sha256_hex(sample.png),
                    measured_score,
                    evaluation: NegativeSampleEvaluationInputs::for_rule(
                        rule,
                        definition.target.captured_dpi,
                        region.revision,
                        (search.width, search.height),
                    ),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let verification = ImageRuleVerification::verify(ImageRuleVerificationInput {
            rule,
            template: &template_image,
            mask: mask_image.as_ref(),
            captured_dpi: definition.target.captured_dpi,
            current_dpi: definition.target.captured_dpi,
            region_revision: region.revision,
            search_dimensions: (search.width, search.height),
            negative_samples: &negative_samples,
            observed_clusters: &observed_clusters,
            maximum_score_cells: input.maximum_score_cells,
        })
        .map_err(anyhow::Error::new)?;
        let artifact = verification.into_artifact();
        super::image_verification::validate_candidate_binding(&definition, rule, &artifact)
            .map_err(|problem| {
                anyhow::anyhow!("local image verification binding is invalid: {problem:?}")
            })?;
        Ok(LocalImageRuleReverification {
            package_fingerprint: pending.package_fingerprint.clone(),
            plan_fingerprint: pending.plan_fingerprint.clone(),
            destination_state_fingerprint: pending.destination_state_fingerprint.clone(),
            rule_id: input.rule_id.to_string(),
            target: definition.target,
            region: region.clone(),
            template,
            mask,
            artifact,
        })
    }

    pub fn commit_image_package_import(
        &self,
        pending: PendingImageImport,
        completions: Vec<LocalImageRuleReverification>,
    ) -> Result<SavedRevision> {
        let _guard = lock_store(&self.lock)?;
        self.recheck_pending_image_import_locked(&pending)?;
        if completions.len() != pending.image_rule_ids.len() {
            bail!("every pending image rule requires one local re-verification");
        }
        let mut by_rule = HashMap::new();
        for completion in completions {
            if completion.package_fingerprint != pending.package_fingerprint
                || completion.plan_fingerprint != pending.plan_fingerprint
                || completion.destination_state_fingerprint != pending.destination_state_fingerprint
            {
                bail!("local image re-verification belongs to a stale package plan");
            }
            if by_rule
                .insert(completion.rule_id.clone(), completion)
                .is_some()
            {
                bail!("duplicate local image re-verification");
            }
        }
        let mut definition = pending.definition.clone();
        let mut local_assets = Vec::new();
        let mut local_target = None;
        let mut local_regions = HashMap::new();
        for rule in &mut definition.image_rules {
            let completion = by_rule
                .remove(&rule.id)
                .with_context(|| format!("image rule '{}' was not locally re-verified", rule.id))?;
            if pending.portable_assets.contains(&completion.template.asset)
                || completion
                    .mask
                    .as_ref()
                    .is_some_and(|mask| pending.portable_assets.contains(&mask.asset))
            {
                bail!("portable image asset identity cannot satisfy local re-verification");
            }
            if let Some(target) = &local_target {
                if target != &completion.target {
                    bail!("local image re-verifications must share one captured target");
                }
            } else {
                local_target = Some(completion.target.clone());
            }
            if let Some(existing) =
                local_regions.insert(completion.region.id.clone(), completion.region.clone())
            {
                if existing != completion.region {
                    bail!("local image re-verifications disagree on confirmed region geometry");
                }
            }
            rule.template = completion.template.asset.clone();
            rule.transparent_mask = completion.mask.as_ref().map(|mask| mask.asset.clone());
            rule.verification = Some(completion.artifact);
            local_assets.push(completion.template);
            if let Some(mask) = completion.mask {
                local_assets.push(mask);
            }
        }
        if !by_rule.is_empty() {
            bail!("local image re-verification references an unexpected rule");
        }
        definition.target = local_target.context("missing local target recapture")?;
        for region in &mut definition.regions {
            if let Some(local) = local_regions.remove(&region.id) {
                *region = local;
            }
        }
        if !local_regions.is_empty() {
            bail!("local image re-verification references an unexpected region");
        }
        validate_identity_set(local_assets.iter().map(|asset| &asset.asset))?;
        let mut installed = Vec::new();
        for asset in &local_assets {
            match self
                .assets
                .install_locked(asset.asset.clone(), &asset.bytes)
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
        match self.save_validated_locked(definition) {
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

    fn recheck_pending_image_import_locked(&self, pending: &PendingImageImport) -> Result<()> {
        if pending_image_plan_fingerprint(
            &pending.package_fingerprint,
            &pending.destination_state_fingerprint,
            &pending.source_macro_id,
            &pending.destination_macro_id,
            &pending.definition,
            &pending.portable_assets,
        )? != pending.plan_fingerprint
        {
            bail!("pending image import fingerprint changed; prepare again");
        }
        if self.import_destination_fingerprint_locked()? != pending.destination_state_fingerprint {
            bail!("pending image import destination changed; prepare again");
        }
        let expected = remap_id(&pending.source_macro_id, &self.existing_macro_ids_locked()?);
        if expected != pending.destination_macro_id || pending.definition.id != expected {
            bail!("pending image import identity changed; prepare again");
        }
        Ok(())
    }

    fn existing_macro_ids_locked(&self) -> Result<HashSet<String>> {
        Ok(fs::read_dir(self.root.join("definitions"))?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| {
                let id = entry.file_name().to_string_lossy().into_owned();
                (!id.starts_with(".deleting-")).then_some(id)
            })
            .collect())
    }

    fn import_destination_fingerprint_locked(&self) -> Result<String> {
        let mut macro_ids = self
            .existing_macro_ids_locked()?
            .into_iter()
            .collect::<Vec<_>>();
        macro_ids.sort();
        let mut bindings = self.assets.load_index_locked()?.bindings;
        bindings.sort_by(|left, right| {
            left.id
                .cmp(&right.id)
                .then_with(|| left.revision.cmp(&right.revision))
                .then_with(|| left.content_hash.cmp(&right.content_hash))
        });
        Ok(sha256_hex(&serde_json::to_vec(&(macro_ids, bindings))?))
    }

    fn import_validated_package(&self, mut package: MacroPackage) -> Result<SavedRevision> {
        reject_portable_image_rules(&package)?;
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
        super::image_verification::trusted_remap_definition_assets(
            &mut package.definition,
            &remaps,
        )?;
        let mut installed = Vec::new();
        for package_asset in &mut package.assets {
            if let Some(id) =
                remaps.get(&(package_asset.asset.id.clone(), package_asset.asset.revision))
            {
                package_asset.asset.id = id.clone();
            }
        }
        validate_package_memory(&package)?;
        validate_package_compiles(&package)?;
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
            let protected_runs = self
                .activity
                .runs
                .lock()
                .map_err(|_| anyhow::anyhow!("store activity poisoned"))?
                .iter()
                .map(|active| active.run_id.clone())
                .collect::<HashSet<_>>();
            if let Err(error) = prune_run_files(
                &runs,
                limits.max_runs.saturating_sub(1),
                Some(&path),
                &protected_runs,
            ) {
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
        let staged = self.assets.load_staged_revision_journal_locked()?;
        for entry in staged.entries {
            for asset in [&entry.previous, &entry.successor] {
                validate_identity_binding(&index.bindings, asset)?;
                self.assets.verify_hash_file_locked(&asset.content_hash)?;
                protected.insert(asset.clone());
            }
        }
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

fn prune_run_files(
    runs: &Path,
    keep: usize,
    exclude: Option<&Path>,
    protected_run_ids: &HashSet<String>,
) -> Result<()> {
    let mut files = Vec::new();
    let mut protected_count = 0_usize;
    for entry in fs::read_dir(runs)? {
        let entry = entry?;
        if exclude.is_some_and(|excluded| entry.path() == excluded) {
            continue;
        }
        if entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl")
            && entry.file_type()?.is_file()
        {
            let run_id = entry
                .path()
                .file_stem()
                .and_then(|stem| stem.to_str())
                .context("run history filename is invalid")?
                .to_string();
            if protected_run_ids.contains(&run_id) {
                protected_count = protected_count.saturating_add(1);
            } else {
                files.push(entry);
            }
        }
    }
    files.sort_by_key(|entry| {
        entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    let keep_unprotected = keep.saturating_sub(protected_count);
    let remove_count = files.len().saturating_sub(keep_unprotected);
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

fn latest_asset_revisions<'a>(
    assets: impl IntoIterator<Item = &'a AssetRef>,
) -> HashMap<&'a str, u64> {
    let mut latest: HashMap<&str, u64> = HashMap::new();
    for asset in assets {
        latest
            .entry(asset.id.as_str())
            .and_modify(|revision| *revision = (*revision).max(asset.revision))
            .or_insert(asset.revision);
    }
    latest
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
    if package.definition.image_rules.len() > MAX_PACKAGE_IMAGE_RULES {
        bail!(
            "package image rule count {} exceeds maximum {MAX_PACKAGE_IMAGE_RULES}",
            package.definition.image_rules.len()
        );
    }
    let definition_bytes = serde_json::to_vec(&package.definition)?;
    if u64::try_from(definition_bytes.len()).context("package definition length overflow")?
        > MAX_PACKAGE_DEFINITION_BYTES
    {
        bail!("package definition bytes exceed maximum {MAX_PACKAGE_DEFINITION_BYTES}");
    }
    if package.assets.len() > MAX_PACKAGE_ASSETS {
        bail!(
            "package asset count {} exceeds maximum {MAX_PACKAGE_ASSETS}",
            package.assets.len()
        );
    }
    let total_asset_bytes = package.assets.iter().try_fold(0_u64, |total, asset| {
        let bytes = u64::try_from(asset.bytes.len()).context("package asset length overflow")?;
        if bytes > MAX_PACKAGE_ASSET_BYTES {
            bail!("package asset bytes exceed maximum {MAX_PACKAGE_ASSET_BYTES}");
        }
        total
            .checked_add(bytes)
            .context("package asset byte total overflow")
    })?;
    if total_asset_bytes > MAX_PACKAGE_TOTAL_ASSET_BYTES {
        bail!("package asset bytes exceed maximum {MAX_PACKAGE_TOTAL_ASSET_BYTES}");
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

fn package_fingerprint(package: &MacroPackage) -> Result<String> {
    let assets = package
        .assets
        .iter()
        .map(|asset| (&asset.asset, &asset.relative_path, &asset.bytes))
        .collect::<Vec<_>>();
    Ok(sha256_hex(&serde_json::to_vec(&(
        package.schema_version,
        &package.definition,
        assets,
    ))?))
}

fn pending_image_plan_fingerprint(
    package_fingerprint: &str,
    destination_state_fingerprint: &str,
    source_macro_id: &str,
    destination_macro_id: &str,
    definition: &MacroDefinition,
    portable_assets: &HashSet<AssetRef>,
) -> Result<String> {
    let mut assets = portable_assets.iter().collect::<Vec<_>>();
    assets.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| left.revision.cmp(&right.revision))
            .then_with(|| left.content_hash.cmp(&right.content_hash))
    });
    Ok(sha256_hex(&serde_json::to_vec(&(
        package_fingerprint,
        destination_state_fingerprint,
        source_macro_id,
        destination_macro_id,
        definition,
        assets,
    ))?))
}

fn validate_image_import_structure(package: &MacroPackage) -> Result<()> {
    validate_macro_name(&package.definition.name)?;
    let problems = super::validate_macro(&package.definition)
        .into_iter()
        .filter(|problem| problem.code != "image_rule.missing_verification")
        .collect::<Vec<_>>();
    if !problems.is_empty() {
        bail!("image package definition is invalid: {problems:?}");
    }
    let assets = package
        .assets
        .iter()
        .map(|asset| (asset.asset.clone(), asset.bytes.as_slice()))
        .collect::<HashMap<_, _>>();
    for rule in &package.definition.image_rules {
        let template = assets
            .get(&rule.template)
            .context("image package template is missing from its frozen snapshot")?;
        ImageRuleVerification::decode_template_png(template)?;
        if let Some(mask) = &rule.transparent_mask {
            let mask = assets
                .get(mask)
                .context("image package mask is missing from its frozen snapshot")?;
            ImageRuleVerification::decode_mask_png(mask)?;
        }
    }
    Ok(())
}

fn fresh_local_import_asset(
    existing: &[AssetRef],
    reserved: &[AssetRef],
    package_fingerprint: &str,
    rule_id: &str,
    kind: &str,
    bytes: &[u8],
) -> Result<PackageAsset> {
    if bytes.is_empty() {
        bail!("local {kind} recapture is empty");
    }
    let package_prefix = package_fingerprint
        .get(..16)
        .context("pending package fingerprint is invalid")?;
    let rule_hash = sha256_hex(rule_id.as_bytes());
    let rule_prefix = rule_hash
        .get(..16)
        .context("local rule fingerprint is invalid")?;
    let base = format!("local-{package_prefix}-{rule_prefix}-{kind}");
    let occupied = existing
        .iter()
        .chain(reserved)
        .map(|asset| asset.id.clone())
        .collect::<HashSet<_>>();
    let id = remap_id(&base, &occupied);
    let asset = AssetRef {
        id,
        revision: 1,
        content_hash: sha256_hex(bytes),
    };
    validate_asset_ref(&asset)?;
    Ok(PackageAsset {
        relative_path: PathBuf::from(format!("local/{kind}.png")),
        asset,
        bytes: bytes.to_vec(),
    })
}

fn reject_portable_image_rules(package: &MacroPackage) -> Result<()> {
    let image_rule_ids = package
        .definition
        .image_rules
        .iter()
        .map(|rule| rule.id.clone())
        .collect::<Vec<_>>();
    if image_rule_ids.is_empty() {
        return Ok(());
    }
    Err(LocalReverificationRequired { image_rule_ids }.into())
}

fn validate_package_compiles(package: &MacroPackage) -> Result<()> {
    let definition_hash = sha256_hex(&serde_json::to_vec_pretty(&package.definition)?);
    let saved = SavedRevision {
        definition: package.definition.clone(),
        definition_hash,
        pinned_assets: package
            .assets
            .iter()
            .map(|asset| PinnedAsset {
                asset: asset.asset.clone(),
                bytes: asset.bytes.clone(),
            })
            .collect(),
    };
    if let Err(error) = super::runtime::CompiledMacro::compile(saved) {
        bail!("package is not compilable: {error:#}");
    }
    Ok(())
}

fn lock_store(lock: &Mutex<()>) -> Result<MutexGuard<'_, ()>> {
    lock.lock()
        .map_err(|_| anyhow::anyhow!("macro store lock poisoned"))
}

fn shared_store_lock(root: &Path) -> Result<(Arc<Mutex<()>>, bool)> {
    let registry = STORE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = registry
        .lock()
        .map_err(|_| anyhow::anyhow!("macro store lock registry poisoned"))?;
    registry.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = registry.get(root).and_then(Weak::upgrade) {
        return Ok((lock, false));
    }
    let lock = Arc::new(Mutex::new(()));
    registry.insert(root.to_path_buf(), Arc::downgrade(&lock));
    Ok((lock, true))
}

fn shared_store_activity(root: &Path) -> Result<Arc<StoreActivity>> {
    let registry = STORE_ACTIVITY.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = registry
        .lock()
        .map_err(|_| anyhow::anyhow!("macro store activity registry poisoned"))?;
    registry.retain(|_, activity| activity.strong_count() > 0);
    if let Some(activity) = registry.get(root).and_then(Weak::upgrade) {
        return Ok(activity);
    }
    let activity = Arc::new(StoreActivity::default());
    registry.insert(root.to_path_buf(), Arc::downgrade(&activity));
    Ok(activity)
}

struct OpenedPackageRoot {
    #[cfg(windows)]
    handle: File,
    #[cfg(windows)]
    identity: (u32, u32, u32),
    #[cfg(not(windows))]
    path: PathBuf,
}

fn open_package_root(package_root: &Path) -> Result<OpenedPackageRoot> {
    let file = open_nofollow(package_root)
        .with_context(|| format!("package folder does not exist: {}", package_root.display()))?;
    let metadata = file
        .metadata()
        .context("could not inspect package folder handle")?;
    reject_reparse_point(&metadata, "package folder")?;
    if !metadata.is_dir() {
        bail!("package path is not a folder");
    }
    #[cfg(windows)]
    {
        return Ok(OpenedPackageRoot {
            identity: file_identity(&file)?,
            handle: file,
        });
    }
    #[cfg(not(windows))]
    {
        let canonical = package_root.canonicalize()?;
        if !canonical.is_dir() || !opened_file_matches_path(&file, &canonical)? {
            bail!("package folder changed while opening");
        }
        Ok(OpenedPackageRoot { path: canonical })
    }
}

fn read_bounded_package_file(
    root: &OpenedPackageRoot,
    relative: &Path,
    maximum: u64,
    label: &str,
) -> Result<Vec<u8>> {
    validate_package_relative(relative)?;
    #[cfg(windows)]
    let file = match open_package_relative_nofollow(root, relative) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            bail!("missing package file: {}", relative.display())
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "could not open package file from verified root: {}",
                    relative.display()
                )
            });
        }
    };
    #[cfg(not(windows))]
    let file = {
        let path = root.path.join(relative);
        match open_nofollow(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                bail!("missing package file: {}", relative.display())
            }
            Err(error) => return Err(error).with_context(|| format!("could not open {label}")),
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("could not inspect {label} handle"))?;
    reject_reparse_point(&metadata, label)?;
    if !metadata.is_file() {
        bail!("{label} is not a regular file");
    }
    let mut bytes = Vec::new();
    file.take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("could not read {label}"))?;
    if u64::try_from(bytes.len()).context("bounded file length overflow")? > maximum {
        bail!("{label} bytes exceed maximum {maximum}");
    }
    Ok(bytes)
}

#[cfg(windows)]
fn open_package_relative_nofollow(root: &OpenedPackageRoot, relative: &Path) -> io::Result<File> {
    if root.identity != file_identity(&root.handle)? {
        return Err(io::Error::other(
            "verified package root handle identity changed",
        ));
    }
    let components = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut parent = root.handle.try_clone()?;
    for (index, component) in components.iter().enumerate() {
        let is_leaf = index + 1 == components.len();
        let opened = open_relative_component_nofollow(&parent, component, !is_leaf)?;
        let metadata = opened.metadata()?;
        if metadata.file_attributes() & 0x0000_0400 != 0 {
            return Err(io::Error::other(
                "package path component is a reparse point",
            ));
        }
        if !is_leaf && !metadata.is_dir() {
            return Err(io::Error::other(
                "package path component is not a directory",
            ));
        }
        parent = opened;
    }
    Ok(parent)
}

#[cfg(windows)]
fn open_relative_component_nofollow(
    parent: &File,
    component: &std::ffi::OsStr,
    directory: bool,
) -> io::Result<File> {
    let mut wide = component.encode_wide().collect::<Vec<_>>();
    let byte_len = wide
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "package component is too long")
        })?;
    let object_name = UNICODE_STRING {
        Length: byte_len,
        MaximumLength: byte_len,
        Buffer: windows::core::PWSTR(wide.as_mut_ptr()),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(std::mem::size_of::<OBJECT_ATTRIBUTES>())
            .expect("object attributes size fits u32"),
        RootDirectory: HANDLE(parent.as_raw_handle() as _),
        ObjectName: &object_name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let create_options = NTCREATEFILE_CREATE_OPTIONS(
        FILE_OPEN_REPARSE_POINT.0
            | FILE_SYNCHRONOUS_IO_NONALERT.0
            | if directory {
                FILE_DIRECTORY_FILE.0
            } else {
                FILE_NON_DIRECTORY_FILE.0
            },
    );
    let mut handle = HANDLE::default();
    let mut status = IO_STATUS_BLOCK::default();
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            FILE_GENERIC_READ,
            &attributes,
            &mut status,
            None,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN,
            create_options,
            None,
            0,
        )
    };
    if result.0 < 0 {
        let kind = match result.0 as u32 {
            0xC000_0034 | 0xC000_003A => io::ErrorKind::NotFound,
            _ => io::ErrorKind::Other,
        };
        return Err(io::Error::new(
            kind,
            format!(
                "relative package component open failed with NTSTATUS {:#x}",
                result.0
            ),
        ));
    }
    Ok(unsafe { File::from_raw_handle(handle.0 as _) })
}

fn open_nofollow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

fn reject_reparse_point(metadata: &fs::Metadata, label: &str) -> Result<()> {
    #[cfg(windows)]
    if metadata.file_attributes() & 0x0000_0400 != 0 {
        bail!("{label} is a reparse point");
    }
    let _ = (metadata, label);
    Ok(())
}

#[cfg(not(windows))]
fn opened_file_matches_path(opened: &File, path: &Path) -> Result<bool> {
    let current = fs::metadata(path)?;
    let opened = opened.metadata()?;
    Ok(opened.len() == current.len() && opened.is_file() == current.is_file())
}

#[cfg(windows)]
fn file_identity(file: &File) -> io::Result<(u32, u32, u32)> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(HANDLE(file.as_raw_handle() as _), &mut information)
            .map_err(|error| io::Error::other(error.to_string()))?;
    }
    Ok((
        information.dwVolumeSerialNumber,
        information.nFileIndexHigh,
        information.nFileIndexLow,
    ))
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

fn validate_macro_name(name: &str) -> Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("macro name must not be empty");
    }
    if trimmed.chars().count() > 200 {
        bail!("macro name is too long");
    }
    if trimmed.chars().any(char::is_control) {
        bail!("macro name contains control characters");
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
        Block, BlockKind, CompiledMacro, Condition, DEFAULT_MAX_SCORE_CELLS, FocusLossPolicy,
        ImageRule, ImageRuleVerification, ImageRuleVerificationInput, Limit, MatchSelectionPolicy,
        NegativeCorpusSample, NegativeSampleEvaluationInputs, ObserveMode, PreprocessProfile,
        RegionDefinition, SafetyPolicy, TargetProfile, TextMatchMode, TextRule,
    };
    use crate::engine::types::RectRatio;
    use image::{DynamicImage, GrayImage, ImageFormat, Luma};
    use std::{
        io::Cursor,
        sync::{Arc, Barrier},
    };

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

    fn fixture_png(seed: u8) -> (GrayImage, Vec<u8>) {
        let image = GrayImage::from_fn(7, 5, |x, y| {
            Luma([seed.wrapping_add((x * 31 + y * 47) as u8)])
        });
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageLuma8(image.clone())
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        (image, bytes.into_inner())
    }

    fn fixture_capture_png(width: u32, height: u32, seed: u8) -> Vec<u8> {
        let image = GrayImage::from_fn(width, height, |x, y| {
            Luma([seed
                .wrapping_add((x * 17) as u8)
                .wrapping_sub((y * 29) as u8)])
        });
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageLuma8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    fn local_target() -> TargetProfile {
        TargetProfile {
            process_path: "local-game.exe".to_string(),
            window_class: "local-game".to_string(),
            title_contains: "Local Diablo".to_string(),
            captured_client_width: 64,
            captured_client_height: 48,
            captured_dpi: 96,
        }
    }

    fn local_region() -> RegionDefinition {
        RegionDefinition {
            id: "region-one".to_string(),
            revision: 99,
            rect: RectRatio {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
        }
    }

    fn compilable_image_definition(
        asset: AssetRef,
        template: &GrayImage,
        mask: Option<(AssetRef, &GrayImage)>,
    ) -> MacroDefinition {
        let mut definition = fixture_definition(asset);
        definition.target.captured_client_width = 64;
        definition.target.captured_client_height = 48;
        definition.regions = vec![RegionDefinition {
            id: "region-one".to_string(),
            revision: 13,
            rect: RectRatio {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
        }];
        definition.blocks = vec![Block {
            id: "observe".to_string(),
            enabled: true,
            kind: BlockKind::Observe {
                condition: Condition::Image {
                    source_block_id: "observe".to_string(),
                    rule_id: "image-one".to_string(),
                    mode: ObserveMode::CheckNow,
                },
            },
        }];
        definition.image_rules[0].transparent_mask = mask.as_ref().map(|(asset, _)| asset.clone());
        let rule = &definition.image_rules[0];
        let samples = vec![NegativeCorpusSample {
            stable_id: "negative/a".to_string(),
            content_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_string(),
            measured_score: 0.80,
            evaluation: NegativeSampleEvaluationInputs::for_rule(rule, 96, 13, (64, 48)),
        }];
        definition.image_rules[0].verification = Some(
            ImageRuleVerification::verify(ImageRuleVerificationInput {
                rule,
                template,
                mask: mask.map(|(_, image)| image),
                captured_dpi: 96,
                current_dpi: 96,
                region_revision: 13,
                search_dimensions: (64, 48),
                negative_samples: &samples,
                observed_clusters: &[],
                maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
            })
            .unwrap()
            .into_artifact(),
        );
        definition
    }

    fn compilable_text_definition() -> MacroDefinition {
        let placeholder = AssetRef {
            id: "unused".to_string(),
            revision: 1,
            content_hash: "0".repeat(64),
        };
        let mut definition = fixture_definition(placeholder);
        definition.regions = vec![RegionDefinition {
            id: "region-one".to_string(),
            revision: 1,
            rect: RectRatio {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
        }];
        definition.image_rules.clear();
        definition.text_rules = vec![TextRule {
            id: "text-one".to_string(),
            revision: 1,
            region_id: "region-one".to_string(),
            language: "en-US".to_string(),
            preprocess: PreprocessProfile::Original,
            expected: "ready".to_string(),
            match_mode: TextMatchMode::Contains,
            threshold: 0.9,
            case_sensitive: false,
            allow_cross_line: false,
            match_policy: MatchSelectionPolicy::HighestScore,
            poll_interval_ms: 100,
            timeout_ms: Limit::Finite(5_000),
            stable_frames: 1,
        }];
        definition.blocks = vec![Block {
            id: "observe".to_string(),
            enabled: true,
            kind: BlockKind::Observe {
                condition: Condition::Text {
                    source_block_id: "observe".to_string(),
                    rule_id: "text-one".to_string(),
                    mode: ObserveMode::CheckNow,
                },
            },
        }];
        definition
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
    fn image_package_collision_is_rejected_without_remapping_or_mutation() {
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
            .save(fixture_definition(existing_asset.clone()))
            .unwrap();

        let error = destination.import_package(&package_folder).unwrap_err();
        fs::remove_dir_all(package_folder).unwrap();

        assert!(
            error
                .downcast_ref::<LocalReverificationRequired>()
                .is_some()
        );
        assert_eq!(
            destination.assets().read(&existing_asset).unwrap(),
            existing_bytes
        );
        assert_eq!(
            fs::read_dir(destination.root.join("definitions"))
                .unwrap()
                .count(),
            1
        );
        assert!(
            destination
                .assets()
                .read(&AssetRef {
                    id: "shared-template-imported-1".to_string(),
                    ..source_asset
                })
                .is_err()
        );
    }

    #[test]
    fn image_package_without_local_verification_is_rejected_before_mutation() {
        let bytes = b"portable-image-bytes".to_vec();
        let asset = AssetRef {
            id: "portable-template".to_string(),
            revision: 1,
            content_hash: sha256_hex(&bytes),
        };
        let package = MacroPackage {
            schema_version: PACKAGE_SCHEMA_VERSION,
            definition: fixture_definition(asset.clone()),
            assets: vec![PackageAsset {
                asset: asset.clone(),
                relative_path: PathBuf::from("assets/portable.png"),
                bytes,
            }],
        };
        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();

        let error = destination.import_validated_package(package).unwrap_err();

        let local = error
            .downcast_ref::<LocalReverificationRequired>()
            .expect("image imports require a typed local re-verification outcome");
        assert_eq!(local.image_rule_ids, vec!["image-one"]);
        assert!(destination.assets().read(&asset).is_err());
        assert!(!destination.root.join("definitions/macro-one").exists());
    }

    #[test]
    fn valid_text_only_package_compiles_and_imports() {
        let source_temp = tempfile::tempdir().unwrap();
        let source = MacroStore::open(source_temp.path()).unwrap();
        let saved = source.save(compilable_text_definition()).unwrap();
        let package_path = source_temp.path().join("text-package");
        source.export_package(&saved, &package_path).unwrap();
        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();

        let imported = destination.import_package(&package_path).unwrap();

        assert_eq!(imported.definition.id, "macro-one");
        assert_eq!(imported.definition.text_rules.len(), 1);
        assert!(imported.definition.image_rules.is_empty());
        assert!(CompiledMacro::compile(imported).is_ok());
    }

    #[test]
    fn invalid_text_only_package_is_compiled_before_install_and_mutates_nothing() {
        let mut definition = compilable_text_definition();
        definition.text_rules[0].region_id = "missing-region".to_string();
        let package = MacroPackage {
            schema_version: PACKAGE_SCHEMA_VERSION,
            definition,
            assets: vec![],
        };
        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();

        let error = destination.import_validated_package(package).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("saved macro revision is invalid")
        );
        assert_eq!(
            fs::read_dir(destination.root.join("definitions"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn verified_image_package_collision_still_requires_local_reverification() {
        let source_temp = tempfile::tempdir().unwrap();
        let source = MacroStore::open(source_temp.path()).unwrap();
        let (source_template, source_bytes) = fixture_png(7);
        let source_asset = AssetRef {
            id: "shared-template".to_string(),
            revision: 1,
            content_hash: sha256_hex(&source_bytes),
        };
        source
            .assets()
            .put_png_revision(source_asset.clone(), &source_bytes)
            .unwrap();
        let (source_mask, source_mask_bytes) = fixture_png(211);
        let source_mask_asset = AssetRef {
            id: "shared-mask".to_string(),
            revision: 1,
            content_hash: sha256_hex(&source_mask_bytes),
        };
        source
            .assets()
            .put_png_revision(source_mask_asset.clone(), &source_mask_bytes)
            .unwrap();
        let source_saved = source
            .save(compilable_image_definition(
                source_asset.clone(),
                &source_template,
                Some((source_mask_asset.clone(), &source_mask)),
            ))
            .unwrap();
        let package_folder = source_temp.path().join("verified-package");
        source
            .export_package(&source_saved, &package_folder)
            .unwrap();

        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();
        let (_, existing_bytes) = fixture_png(119);
        destination
            .assets()
            .put_png_revision(
                AssetRef {
                    id: source_asset.id.clone(),
                    revision: source_asset.revision,
                    content_hash: sha256_hex(&existing_bytes),
                },
                &existing_bytes,
            )
            .unwrap();
        let (_, existing_mask_bytes) = fixture_png(31);
        destination
            .assets()
            .put_png_revision(
                AssetRef {
                    id: source_mask_asset.id.clone(),
                    revision: source_mask_asset.revision,
                    content_hash: sha256_hex(&existing_mask_bytes),
                },
                &existing_mask_bytes,
            )
            .unwrap();

        let error = destination.import_package(&package_folder).unwrap_err();

        assert!(
            error
                .downcast_ref::<LocalReverificationRequired>()
                .is_some()
        );
        for id in ["shared-template-imported-1", "shared-mask-imported-1"] {
            assert!(!destination.assets.root.join(id).exists());
        }
    }

    #[test]
    fn stale_portable_verification_is_rejected_by_local_trust_boundary_first() {
        let source_temp = tempfile::tempdir().unwrap();
        let source = MacroStore::open(source_temp.path()).unwrap();
        let (source_template, source_bytes) = fixture_png(7);
        let source_asset = AssetRef {
            id: "shared-template".to_string(),
            revision: 1,
            content_hash: sha256_hex(&source_bytes),
        };
        source
            .assets()
            .put_png_revision(source_asset.clone(), &source_bytes)
            .unwrap();
        let source_saved = source
            .save(compilable_image_definition(
                source_asset.clone(),
                &source_template,
                None,
            ))
            .unwrap();
        let package_folder = source_temp.path().join("stale-package");
        source
            .export_package(&source_saved, &package_folder)
            .unwrap();
        let definition_path = package_folder.join("macro.json");
        let mut stale: MacroDefinition =
            serde_json::from_slice(&fs::read(&definition_path).unwrap()).unwrap();
        stale.image_rules[0]
            .verification
            .as_mut()
            .unwrap()
            .verification_fingerprint_sha256 =
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        fs::write(&definition_path, serde_json::to_vec_pretty(&stale).unwrap()).unwrap();

        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();
        let (_, existing_bytes) = fixture_png(119);
        destination
            .assets()
            .put_png_revision(
                AssetRef {
                    id: source_asset.id.clone(),
                    revision: source_asset.revision,
                    content_hash: sha256_hex(&existing_bytes),
                },
                &existing_bytes,
            )
            .unwrap();

        let error = destination.import_package(&package_folder).unwrap_err();

        assert!(
            error
                .downcast_ref::<LocalReverificationRequired>()
                .is_some()
        );
        let would_be_remap = AssetRef {
            id: "shared-template-imported-1".to_string(),
            ..source_asset
        };
        assert!(destination.assets().read(&would_be_remap).is_err());
    }

    #[test]
    fn invalid_portable_image_package_hits_local_reverification_boundary_first() {
        let (varied_template, _) = fixture_png(7);
        let flat = GrayImage::from_pixel(7, 5, Luma([80]));
        let mut flat_bytes = Cursor::new(Vec::new());
        DynamicImage::ImageLuma8(flat)
            .write_to(&mut flat_bytes, ImageFormat::Png)
            .unwrap();
        let flat_bytes = flat_bytes.into_inner();
        let flat_asset = AssetRef {
            id: "flat-template".to_string(),
            revision: 1,
            content_hash: sha256_hex(&flat_bytes),
        };
        let definition = compilable_image_definition(flat_asset.clone(), &varied_template, None);
        let package = MacroPackage {
            schema_version: PACKAGE_SCHEMA_VERSION,
            definition,
            assets: vec![PackageAsset {
                asset: flat_asset.clone(),
                relative_path: PathBuf::from("assets/flat.png"),
                bytes: flat_bytes,
            }],
        };
        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();

        let error = destination.import_validated_package(package).unwrap_err();

        assert!(
            error
                .downcast_ref::<LocalReverificationRequired>()
                .is_some()
        );
        assert!(destination.assets().read(&flat_asset).is_err());
        assert!(!destination.root.join("definitions/macro-one").exists());
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
    fn captured_template_recapture_keeps_id_and_preserves_old_revision_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let original = AssetRef {
            id: "logical-template".into(),
            revision: 4,
            content_hash: sha256_hex(b"old png"),
        };
        store
            .assets()
            .put_png_revision(original.clone(), b"old png")
            .unwrap();

        let recaptured = store
            .assets()
            .put_next_png_revision(&original, b"new png")
            .unwrap();

        assert_eq!(recaptured.id, original.id);
        assert_eq!(recaptured.revision, 5);
        assert_ne!(recaptured.content_hash, original.content_hash);
        assert_eq!(store.assets().read(&original).unwrap(), b"old png");
        assert_eq!(store.assets().read(&recaptured).unwrap(), b"new png");
    }

    #[test]
    fn identical_captured_templates_get_independent_logical_identities() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();

        let first = store.assets().put_png(b"same png").unwrap();
        let second = store.assets().put_png(b"same png").unwrap();

        assert_ne!(first.id, second.id);
        assert_eq!(first.revision, 1);
        assert_eq!(second.revision, 1);
        assert_eq!(first.content_hash, second.content_hash);
        assert_eq!(
            fs::read_dir(temp.path().join("macro_data/assets"))
                .unwrap()
                .count(),
            1,
            "content-addressed bytes remain deduplicated"
        );
    }

    #[test]
    fn stale_recap_fails_without_blocking_independent_template() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let first = store.assets().put_png(b"first").unwrap();
        let independent = store.assets().put_png(b"independent").unwrap();
        let next = store
            .assets()
            .put_next_png_revision(&first, b"first next")
            .unwrap();

        let stale = store
            .assets()
            .put_next_png_revision(&first, b"stale")
            .unwrap_err();
        assert!(stale.to_string().contains("stale template revision"));
        let independent_next = store
            .assets()
            .put_next_png_revision(&independent, b"independent next")
            .unwrap();

        assert_eq!(next.revision, 2);
        assert_eq!(independent_next.revision, 2);
        assert_eq!(store.assets().read(&next).unwrap(), b"first next");
    }

    #[test]
    fn recap_rejects_a_binding_whose_previous_bytes_are_missing() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let missing = store.assets().put_png(b"will disappear").unwrap();
        fs::remove_file(store.assets().path_for_hash(&missing.content_hash).unwrap()).unwrap();

        let error = store
            .assets()
            .put_next_png_revision(&missing, b"must not publish")
            .unwrap_err();

        assert!(error.to_string().contains("previous template bytes"));
        let index = store.assets().load_index_locked().unwrap();
        assert!(
            !index
                .bindings
                .iter()
                .any(|binding| binding.id == missing.id && binding.revision == 2)
        );
    }

    #[test]
    fn concurrent_recap_from_same_revision_publishes_exactly_one_successor() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let original = store.assets().put_png(b"original").unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for bytes in [b"winner one".as_slice(), b"winner two".as_slice()] {
            let store = store.clone();
            let original = original.clone();
            let barrier = Arc::clone(&barrier);
            let bytes = bytes.to_vec();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                store.assets().put_next_png_revision(&original, &bytes)
            }));
        }
        barrier.wait();
        let results = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
        assert!(
            results
                .iter()
                .filter_map(|result| result.as_ref().err())
                .any(|error| error.to_string().contains("stale template revision"))
        );
    }

    #[test]
    fn rejected_staged_recapture_rolls_back_and_allows_retry() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let original = store.assets().put_png(b"original").unwrap();
        let rejected = store
            .assets()
            .stage_next_png_revision(&original, b"rejected successor")
            .unwrap();

        store
            .assets()
            .discard_staged_png_revision(&rejected)
            .unwrap();

        assert!(store.assets().read(&rejected).is_err());
        let retry = store
            .assets()
            .stage_next_png_revision(&original, b"retry successor")
            .unwrap();
        assert_eq!(retry.revision, original.revision + 1);
        assert_eq!(store.assets().read(&retry).unwrap(), b"retry successor");
    }

    #[test]
    fn serialized_recapture_replaces_staged_successor_after_draft_undo() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let original = store.assets().put_png(b"original").unwrap();
        let undone = store
            .assets()
            .stage_next_png_revision(&original, b"successor later undone")
            .unwrap();

        let replacement = store
            .assets()
            .replace_staged_png_revision(&original, b"replacement after undo")
            .unwrap();

        assert_eq!(replacement.id, original.id);
        assert_eq!(replacement.revision, undone.revision);
        assert_ne!(replacement.content_hash, undone.content_hash);
        assert!(store.assets().read(&undone).is_err());
        assert_eq!(
            store.assets().read(&replacement).unwrap(),
            b"replacement after undo"
        );
    }

    #[test]
    fn orphan_cleanup_preserves_active_staged_recapture_and_predecessor() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let original = store.assets().put_png(b"original").unwrap();
        let staged = store
            .assets()
            .stage_next_png_revision(&original, b"active successor")
            .unwrap();

        assert_eq!(store.cleanup_orphan_assets(&HashSet::new()).unwrap(), 0);
        assert_eq!(store.assets().read(&original).unwrap(), b"original");
        assert_eq!(store.assets().read(&staged).unwrap(), b"active successor");
    }

    #[test]
    fn reopening_recovers_unreferenced_staged_recapture_after_crash() {
        let temp = tempfile::tempdir().unwrap();
        let original;
        let abandoned;
        {
            let store = MacroStore::open(temp.path()).unwrap();
            original = store.assets().put_png(b"original").unwrap();
            abandoned = store
                .assets()
                .stage_next_png_revision(&original, b"published before crash")
                .unwrap();
            assert_eq!(
                store.assets().read(&abandoned).unwrap(),
                b"published before crash"
            );
        }

        let reopened = MacroStore::open(temp.path()).unwrap();
        assert!(reopened.assets().read(&abandoned).is_err());
        let retry = reopened
            .assets()
            .stage_next_png_revision(&original, b"retry after restart")
            .unwrap();
        assert_eq!(retry.revision, abandoned.revision);
        assert_eq!(
            reopened.assets().read(&retry).unwrap(),
            b"retry after restart"
        );
    }

    #[test]
    fn additional_live_store_open_does_not_recover_an_active_staged_recapture() {
        let temp = tempfile::tempdir().unwrap();
        let staged;
        {
            let first = MacroStore::open(temp.path()).unwrap();
            let original = first.assets().put_png(b"original").unwrap();
            staged = first
                .assets()
                .stage_next_png_revision(&original, b"active staged successor")
                .unwrap();

            let second = MacroStore::open(temp.path()).unwrap();
            assert_eq!(
                second.assets().read(&staged).unwrap(),
                b"active staged successor"
            );
        }

        let restarted = MacroStore::open(temp.path()).unwrap();
        assert!(restarted.assets().read(&staged).is_err());
    }

    #[test]
    fn additional_live_store_open_still_rejects_a_corrupt_staged_journal() {
        let temp = tempfile::tempdir().unwrap();
        let first = MacroStore::open(temp.path()).unwrap();
        fs::write(&first.assets.staged_revision_journal, b"{not valid json").unwrap();

        let error = MacroStore::open(temp.path()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("corrupt staged asset revision journal")
        );
    }

    #[test]
    fn corrupt_staged_journal_blocks_save_before_any_definition_publication() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let original = store.assets().put_png(b"original").unwrap();
        let staged = store
            .assets()
            .stage_next_png_revision(&original, b"successor")
            .unwrap();
        fs::write(&store.assets.staged_revision_journal, b"{not valid json").unwrap();

        let error = store.save(fixture_definition(staged)).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("corrupt staged asset revision journal")
        );
        assert!(
            !temp
                .path()
                .join("macro_data/definitions/macro-one")
                .exists()
        );
    }

    #[test]
    fn staged_cleanup_failure_after_publication_returns_certain_success_and_recovers() {
        let temp = tempfile::tempdir().unwrap();
        let staged;
        {
            let store = MacroStore::open(temp.path()).unwrap();
            let original = store.assets().put_png(b"original").unwrap();
            staged = store
                .assets()
                .stage_next_png_revision(&original, b"durable successor")
                .unwrap();
            store
                .fail_staged_finalize_cleanup
                .store(true, Ordering::SeqCst);

            let saved = store
                .save(fixture_definition(staged.clone()))
                .expect("durable publication remains a certain success");

            assert_eq!(saved.definition.image_rules[0].template, staged);
            assert!(store.assets.staged_revision_journal.exists());
            let current: MacroDefinition = serde_json::from_slice(
                &fs::read(
                    temp.path()
                        .join("macro_data/definitions/macro-one/current.json"),
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(current.image_rules[0].template, staged);
        }

        let reopened = MacroStore::open(temp.path()).unwrap();
        assert_eq!(
            reopened.assets().read(&staged).unwrap(),
            b"durable successor"
        );
        assert!(!reopened.assets.staged_revision_journal.exists());
    }

    #[test]
    fn same_process_replace_cannot_rollback_durable_successor_after_cleanup_failure() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let original = store.assets().put_png(b"original").unwrap();
        let staged = store
            .assets()
            .stage_next_png_revision(&original, b"durable successor")
            .unwrap();
        store
            .fail_staged_finalize_cleanup
            .store(true, Ordering::SeqCst);
        store.save(fixture_definition(staged.clone())).unwrap();

        let error = store
            .assets()
            .replace_staged_png_revision(&original, b"must not replace durable bytes")
            .unwrap_err();

        assert!(error.to_string().contains("durable"));
        assert_eq!(store.assets().read(&staged).unwrap(), b"durable successor");
        let current: MacroDefinition = serde_json::from_slice(
            &fs::read(
                temp.path()
                    .join("macro_data/definitions/macro-one/current.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(current.image_rules[0].template, staged);
    }

    #[test]
    fn same_process_discard_cannot_rollback_durable_successor_after_cleanup_failure() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let original = store.assets().put_png(b"original").unwrap();
        let staged = store
            .assets()
            .stage_next_png_revision(&original, b"durable successor")
            .unwrap();
        store
            .fail_staged_finalize_cleanup
            .store(true, Ordering::SeqCst);
        store.save(fixture_definition(staged.clone())).unwrap();

        let error = store
            .assets()
            .discard_staged_png_revision(&staged)
            .unwrap_err();

        assert!(error.to_string().contains("durable"));
        assert_eq!(store.assets().read(&staged).unwrap(), b"durable successor");
        let current: MacroDefinition = serde_json::from_slice(
            &fs::read(
                temp.path()
                    .join("macro_data/definitions/macro-one/current.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(current.image_rules[0].template, staged);
    }

    #[test]
    fn staged_discard_and_replace_fail_closed_when_durable_definition_scan_is_corrupt() {
        for replace in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let store = MacroStore::open(temp.path()).unwrap();
            let original = store.assets().put_png(b"original").unwrap();
            let staged = store
                .assets()
                .stage_next_png_revision(&original, b"staged successor")
                .unwrap();
            let definition_dir = temp.path().join("macro_data/definitions/corrupt-macro");
            fs::create_dir_all(&definition_dir).unwrap();
            fs::write(definition_dir.join("current.json"), b"{not valid json").unwrap();

            let error = if replace {
                store
                    .assets()
                    .replace_staged_png_revision(&original, b"replacement")
                    .map(|_| ())
                    .unwrap_err()
            } else {
                store
                    .assets()
                    .discard_staged_png_revision(&staged)
                    .unwrap_err()
            };

            assert!(error.to_string().contains("corrupt saved macro definition"));
            assert_eq!(store.assets().read(&staged).unwrap(), b"staged successor");
        }
    }

    #[test]
    fn durable_definition_reference_finalizes_staged_recapture_on_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let original;
        let staged;
        {
            let store = MacroStore::open(temp.path()).unwrap();
            original = store.assets().put_png(b"original").unwrap();
            staged = store
                .assets()
                .stage_next_png_revision(&original, b"durably accepted")
                .unwrap();
            store.save(fixture_definition(staged.clone())).unwrap();
        }

        let reopened = MacroStore::open(temp.path()).unwrap();
        assert_eq!(
            reopened.assets().read(&staged).unwrap(),
            b"durably accepted"
        );
        let stale = reopened
            .assets()
            .replace_staged_png_revision(&original, b"must remain stale")
            .unwrap_err();
        assert!(
            stale.to_string().contains("stale template revision")
                || stale.to_string().contains("durable successor")
        );
    }

    #[test]
    fn concurrent_staged_recaptures_from_same_revision_reserve_exactly_one_successor() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let original = store.assets().put_png(b"original").unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for bytes in [b"winner one".as_slice(), b"winner two".as_slice()] {
            let store = store.clone();
            let original = original.clone();
            let barrier = Arc::clone(&barrier);
            let bytes = bytes.to_vec();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                store.assets().stage_next_png_revision(&original, &bytes)
            }));
        }
        barrier.wait();
        let results = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
        assert!(
            results
                .iter()
                .filter_map(|result| result.as_ref().err())
                .any(
                    |error| error.to_string().contains("stale template revision")
                        || error.to_string().contains("staged template revision")
                )
        );
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
    fn image_import_rejection_precedes_asset_install_failure_injection() {
        let source_temp = tempfile::tempdir().unwrap();
        let source = MacroStore::open(source_temp.path()).unwrap();
        let asset = source.assets().put_png(b"source").unwrap();
        let saved = source.save(fixture_definition(asset.clone())).unwrap();
        let package_path = source_temp.path().join("package");
        source.export_package(&saved, &package_path).unwrap();

        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();
        destination.fail_next_import_after_asset_installs();
        let error = destination.import_package(&package_path).unwrap_err();
        assert!(
            error
                .downcast_ref::<LocalReverificationRequired>()
                .is_some()
        );
        assert!(destination.assets().read(&asset).is_err());
        assert_eq!(
            fs::read_dir(destination_temp.path().join("macro_data/definitions"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn validated_image_package_source_substitution_still_requires_local_reverification() {
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
        let error = destination.import_validated_package(package).unwrap_err();
        assert!(
            error
                .downcast_ref::<LocalReverificationRequired>()
                .is_some()
        );
        assert!(destination.assets().read(&asset).is_err());
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
    fn image_import_rejection_precedes_collision_remap_reservations() {
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

        let error = destination.import_package(&package_path).unwrap_err();
        assert!(
            error
                .downcast_ref::<LocalReverificationRequired>()
                .is_some()
        );
        for id in ["x-imported-1", "x-imported-2"] {
            assert!(!destination.assets.root.join(id).exists());
        }
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

    #[test]
    fn checked_lifecycle_loads_current_revision_and_history() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let first = store.save_validated(compilable_text_definition()).unwrap();
        let second = store
            .rename_macro(
                &first.definition.id,
                &first.definition_hash,
                "Renamed macro",
            )
            .unwrap();

        let summaries = store.list_macros().unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "macro-one");
        assert_eq!(summaries[0].name, "Renamed macro");
        assert_eq!(summaries[0].current_revision, 2);
        assert!(summaries[0].enabled);
        assert_eq!(store.load_current("macro-one").unwrap(), second);
        assert_eq!(store.load_revision("macro-one", 1).unwrap(), first);
        assert_eq!(
            store
                .revision_history("macro-one")
                .unwrap()
                .iter()
                .map(|revision| revision.revision)
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
    }

    #[test]
    fn duplicate_disable_and_typed_active_delete_are_transactional() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let original = store.save_validated(compilable_text_definition()).unwrap();
        let duplicate = store
            .duplicate_macro(
                &original.definition.id,
                &original.definition_hash,
                "macro-copy",
                "Macro copy",
            )
            .unwrap();
        assert_eq!(duplicate.definition.id, "macro-copy");
        assert_eq!(duplicate.definition.revision, 1);
        assert_eq!(duplicate.definition.name, "Macro copy");
        store.set_macro_enabled("macro-copy", false).unwrap();
        assert!(
            !store
                .list_macros()
                .unwrap()
                .iter()
                .find(|summary| summary.id == "macro-copy")
                .unwrap()
                .enabled
        );

        let active = store.acquire_active_revision(&duplicate).unwrap();
        assert!(
            store
                .delete_macro("macro-copy")
                .unwrap_err()
                .to_string()
                .contains("active")
        );
        assert!(store.load_current("macro-copy").is_ok());
        drop(active);
        store.delete_macro("macro-copy").unwrap();
        assert!(store.load_current("macro-copy").is_err());
        assert!(store.load_current("macro-one").is_ok());
    }

    #[test]
    fn duplicate_rejects_a_source_hash_that_changed_after_enqueue() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let first = store.save_validated(compilable_text_definition()).unwrap();
        let mut revised = first.definition.clone();
        revised.name = "Revised macro".into();
        revised.revision = 2;
        store.save_validated(revised).unwrap();

        assert!(
            store
                .duplicate_macro(
                    &first.definition.id,
                    &first.definition_hash,
                    "macro-copy",
                    "Macro copy",
                )
                .unwrap_err()
                .to_string()
                .contains("changed since it was loaded")
        );
        assert!(store.load_current("macro-copy").is_err());
    }

    #[test]
    fn checked_load_fails_closed_when_current_or_checksum_is_corrupt() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        store.save_validated(compilable_text_definition()).unwrap();
        let current = temp
            .path()
            .join("macro_data/definitions/macro-one/current.json");
        let mut definition: MacroDefinition =
            serde_json::from_slice(&fs::read(&current).unwrap()).unwrap();
        definition.name = "tampered".to_string();
        fs::write(&current, serde_json::to_vec_pretty(&definition).unwrap()).unwrap();

        assert!(
            store
                .load_current("macro-one")
                .unwrap_err()
                .to_string()
                .contains("current")
        );

        fs::write(
            temp.path()
                .join("macro_data/definitions/macro-one/current.json"),
            fs::read(temp.path().join("macro_data/definitions/macro-one/1.json")).unwrap(),
        )
        .unwrap();
        fs::write(
            temp.path()
                .join("macro_data/definitions/macro-one/1.json.sha256"),
            "0".repeat(64),
        )
        .unwrap();
        assert!(store.load_current("macro-one").is_err());
    }

    #[test]
    fn run_history_is_checked_and_active_identity_blocks_delete() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let mut journal =
            ready_journal(store.open_journal("run-one", JournalLimits::new(1_024, 4)));
        journal.append(&JournalRecord {
            sequence: 1,
            elapsed_ms: 2,
            kind: JournalKind::StateChange,
            message: "started".to_string(),
            fields: serde_json::json!({}),
        });
        drop(journal);

        assert_eq!(store.list_run_history().unwrap()[0].run_id, "run-one");
        assert_eq!(store.load_run_history("run-one").unwrap().len(), 1);
        let active = store.acquire_active_run("run-one").unwrap();
        assert!(
            store
                .delete_run_history("run-one")
                .unwrap_err()
                .to_string()
                .contains("active")
        );
        drop(active);
        store.delete_run_history("run-one").unwrap();
        assert!(store.list_run_history().unwrap().is_empty());
    }

    #[test]
    fn cross_store_journal_pruning_never_removes_an_active_run() {
        let temp = tempfile::tempdir().unwrap();
        let first = MacroStore::open(temp.path()).unwrap();
        let second = MacroStore::open(temp.path()).unwrap();
        let active = first.acquire_active_run("active-run").unwrap();
        let active_journal =
            ready_journal(first.open_journal("active-run", JournalLimits::new(1_024, 2)));
        let active_path = active_journal.path().to_path_buf();

        let _new = ready_journal(second.open_journal("new-run", JournalLimits::new(1_024, 1)));

        assert!(active_path.exists());
        drop(active_journal);
        drop(active);
    }

    #[test]
    fn export_current_rechecks_the_checked_immutable_revision() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        store.save_validated(compilable_text_definition()).unwrap();
        let package = temp.path().join("checked-export");

        store.export_current_package("macro-one", &package).unwrap();
        let exported: MacroDefinition =
            serde_json::from_slice(&fs::read(package.join("macro.json")).unwrap()).unwrap();
        assert_eq!(exported.name, "Macro One");

        fs::write(
            temp.path()
                .join("macro_data/definitions/macro-one/current.json"),
            b"{}",
        )
        .unwrap();
        assert!(
            store
                .export_current_package("macro-one", &temp.path().join("rejected-export"))
                .is_err()
        );
        assert!(!temp.path().join("rejected-export").exists());
    }

    #[test]
    fn checked_export_rejects_a_revision_that_changed_after_enqueue() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let first = store.save_validated(compilable_text_definition()).unwrap();
        let mut revised = first.definition.clone();
        revised.name = "Revised macro".into();
        revised.revision = 2;
        store.save_validated(revised).unwrap();
        let package = temp.path().join("stale-export");

        assert!(
            store
                .export_current_package_checked(
                    &first.definition.id,
                    first.definition.revision,
                    &first.definition_hash,
                    &package,
                )
                .is_err()
        );
        assert!(!package.exists());
    }

    #[test]
    fn text_prepare_is_non_mutating_and_survives_source_deletion() {
        let source_temp = tempfile::tempdir().unwrap();
        let source = MacroStore::open(source_temp.path()).unwrap();
        source.save_validated(compilable_text_definition()).unwrap();
        let package_path = source_temp.path().join("text-package");
        source
            .export_current_package("macro-one", &package_path)
            .unwrap();
        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();

        let prepared = match destination.prepare_package_import(&package_path).unwrap() {
            PreparedPackageImport::Text(prepared) => prepared,
            PreparedPackageImport::Image(_) => panic!("text package was misclassified"),
        };
        assert_eq!(
            fs::read_dir(destination.root.join("definitions"))
                .unwrap()
                .count(),
            0
        );
        fs::remove_dir_all(&package_path).unwrap();

        let imported = destination.commit_text_package_import(prepared).unwrap();
        assert_eq!(imported.definition.id, "macro-one");
    }

    #[test]
    fn image_prepare_is_non_mutating_and_strips_portable_proofs() {
        let source_temp = tempfile::tempdir().unwrap();
        let source = MacroStore::open(source_temp.path()).unwrap();
        let (template, bytes) = fixture_png(7);
        let asset = source.assets().put_png(&bytes).unwrap();
        source
            .save_validated(compilable_image_definition(asset, &template, None))
            .unwrap();
        let package_path = source_temp.path().join("image-package");
        source
            .export_current_package("macro-one", &package_path)
            .unwrap();
        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();

        let pending = match destination.prepare_package_import(&package_path).unwrap() {
            PreparedPackageImport::Image(pending) => pending,
            PreparedPackageImport::Text(_) => panic!("image package was misclassified"),
        };
        assert_eq!(pending.image_rule_ids(), &["image-one"]);
        assert!(
            pending
                .definition()
                .image_rules
                .iter()
                .all(|rule| rule.verification.is_none())
        );
        assert_eq!(
            fs::read_dir(destination.root.join("definitions"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            fs::read_dir(destination.root.join("assets"))
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "png"))
                .count(),
            0
        );
        fs::remove_dir_all(package_path).unwrap();
        assert_eq!(pending.definition().id, "macro-one");
    }

    #[test]
    fn image_commit_accepts_only_fresh_local_verifier_completions() {
        let source_temp = tempfile::tempdir().unwrap();
        let source = MacroStore::open(source_temp.path()).unwrap();
        let (portable_template, portable_bytes) = fixture_png(7);
        let portable_asset = source.assets().put_png(&portable_bytes).unwrap();
        source
            .save_validated(compilable_image_definition(
                portable_asset.clone(),
                &portable_template,
                None,
            ))
            .unwrap();
        let package_path = source_temp.path().join("image-package");
        source
            .export_current_package("macro-one", &package_path)
            .unwrap();
        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();
        let pending = match destination.prepare_package_import(&package_path).unwrap() {
            PreparedPackageImport::Image(pending) => pending,
            PreparedPackageImport::Text(_) => panic!("image package was misclassified"),
        };
        fs::remove_dir_all(package_path).unwrap();
        let (_local_template, local_bytes) = fixture_png(91);
        let target_region = fixture_capture_png(40, 48, 22);
        let negative_capture = fixture_capture_png(64, 48, 200);
        let mut local_target = local_target();
        local_target.captured_client_width = 80;
        let mut local_region = local_region();
        local_region.rect.width = 0.5;
        let negatives = vec![LocalNegativeImageSample {
            stable_id: "local-negative/a".to_string(),
            png: &negative_capture,
        }];
        let completion = destination
            .complete_local_image_reverification(
                &pending,
                LocalImageRuleVerificationInput {
                    rule_id: "image-one",
                    template_png: &local_bytes,
                    mask_png: None,
                    target: local_target,
                    region: local_region,
                    target_region_png: &target_region,
                    negative_samples: &negatives,
                    maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
                },
            )
            .unwrap();

        let saved = destination
            .commit_image_package_import(pending, vec![completion])
            .unwrap();

        let local = &saved.definition.image_rules[0].template;
        assert!(local.id.starts_with("local-"));
        assert_ne!(local, &portable_asset);
        assert_eq!(destination.assets().read(local).unwrap(), local_bytes);
        assert!(CompiledMacro::compile(saved).is_ok());
    }

    #[test]
    fn image_package_import_replaces_portable_target_and_region_with_local_capture() {
        let source_temp = tempfile::tempdir().unwrap();
        let source = MacroStore::open(source_temp.path()).unwrap();
        let (portable_template, portable_bytes) = fixture_png(7);
        let portable_asset = source.assets().put_png(&portable_bytes).unwrap();
        source
            .save_validated(compilable_image_definition(
                portable_asset,
                &portable_template,
                None,
            ))
            .unwrap();
        let package_path = source_temp.path().join("image-package");
        source
            .export_current_package("macro-one", &package_path)
            .unwrap();

        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();
        let pending = match destination.prepare_package_import(&package_path).unwrap() {
            PreparedPackageImport::Image(pending) => pending,
            PreparedPackageImport::Text(_) => panic!("image package was misclassified"),
        };
        let (_local_template, local_bytes) = fixture_png(91);
        let target_region = fixture_capture_png(40, 48, 22);
        let negative_capture = fixture_capture_png(64, 48, 200);
        let mut local_target = local_target();
        local_target.captured_client_width = 80;
        let mut local_region = local_region();
        local_region.rect.width = 0.5;
        let negatives = vec![LocalNegativeImageSample {
            stable_id: "local-negative/a".to_string(),
            png: &negative_capture,
        }];
        let completion = destination
            .complete_local_image_reverification(
                &pending,
                LocalImageRuleVerificationInput {
                    rule_id: "image-one",
                    template_png: &local_bytes,
                    mask_png: None,
                    target: local_target,
                    region: local_region,
                    target_region_png: &target_region,
                    negative_samples: &negatives,
                    maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
                },
            )
            .unwrap();
        let saved = destination
            .commit_image_package_import(pending, vec![completion])
            .unwrap();

        assert_eq!(saved.definition.target.process_path, "local-game.exe");
        assert_eq!(saved.definition.target.captured_client_width, 80);
        assert_eq!(saved.definition.regions[0].revision, 99);
        assert_eq!(saved.definition.regions[0].rect.width, 0.5);
    }

    #[test]
    fn image_reverification_rejects_caller_asserted_negative_score_without_local_image_evidence() {
        let source_temp = tempfile::tempdir().unwrap();
        let source = MacroStore::open(source_temp.path()).unwrap();
        let (portable_template, portable_bytes) = fixture_png(7);
        let portable_asset = source.assets().put_png(&portable_bytes).unwrap();
        source
            .save_validated(compilable_image_definition(
                portable_asset,
                &portable_template,
                None,
            ))
            .unwrap();
        let package_path = source_temp.path().join("image-package");
        source
            .export_current_package("macro-one", &package_path)
            .unwrap();
        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();
        let pending = match destination.prepare_package_import(&package_path).unwrap() {
            PreparedPackageImport::Image(pending) => pending,
            PreparedPackageImport::Text(_) => panic!("image package was misclassified"),
        };
        let (_local_template, local_bytes) = fixture_png(91);
        let target_region = fixture_capture_png(64, 48, 22);
        let positive_as_negative = local_bytes.clone();
        let asserted_negatives = vec![LocalNegativeImageSample::from_local_capture(
            "caller-asserted".to_string(),
            &positive_as_negative,
        )];

        let result = destination.complete_local_image_reverification(
            &pending,
            LocalImageRuleVerificationInput::from_local_capture(
                "image-one",
                &local_bytes,
                None,
                local_target(),
                local_region(),
                &target_region,
                &asserted_negatives,
                DEFAULT_MAX_SCORE_CELLS,
            ),
        );

        assert!(result.is_err());
    }

    #[test]
    fn image_commit_rechecks_destination_and_rolls_back_local_assets() {
        let source_temp = tempfile::tempdir().unwrap();
        let source = MacroStore::open(source_temp.path()).unwrap();
        let (portable_template, portable_bytes) = fixture_png(7);
        let portable_asset = source.assets().put_png(&portable_bytes).unwrap();
        source
            .save_validated(compilable_image_definition(
                portable_asset,
                &portable_template,
                None,
            ))
            .unwrap();
        let package_path = source_temp.path().join("image-package");
        source
            .export_current_package("macro-one", &package_path)
            .unwrap();
        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();
        let pending = match destination.prepare_package_import(&package_path).unwrap() {
            PreparedPackageImport::Image(pending) => pending,
            PreparedPackageImport::Text(_) => panic!("image package was misclassified"),
        };
        let (_local_template, local_bytes) = fixture_png(91);
        let target_region = fixture_capture_png(64, 48, 22);
        let negative_capture = fixture_capture_png(64, 48, 200);
        let negatives = vec![LocalNegativeImageSample {
            stable_id: "local-negative/a".to_string(),
            png: &negative_capture,
        }];
        let completion = destination
            .complete_local_image_reverification(
                &pending,
                LocalImageRuleVerificationInput {
                    rule_id: "image-one",
                    template_png: &local_bytes,
                    mask_png: None,
                    target: local_target(),
                    region: local_region(),
                    target_region_png: &target_region,
                    negative_samples: &negatives,
                    maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
                },
            )
            .unwrap();
        let mut other = compilable_text_definition();
        other.id = "other".to_string();
        other.name = "Other".to_string();
        destination.save_validated(other).unwrap();

        let error = destination
            .commit_image_package_import(pending, vec![completion])
            .unwrap_err();
        assert!(error.to_string().contains("destination changed"));
        assert!(!destination.root.join("definitions/macro-one").exists());
        assert_eq!(
            fs::read_dir(destination.root.join("assets"))
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "png"))
                .count(),
            0
        );
    }

    #[test]
    fn image_commit_failure_after_asset_install_is_transactional() {
        let source_temp = tempfile::tempdir().unwrap();
        let source = MacroStore::open(source_temp.path()).unwrap();
        let (portable_template, portable_bytes) = fixture_png(7);
        let portable_asset = source.assets().put_png(&portable_bytes).unwrap();
        source
            .save_validated(compilable_image_definition(
                portable_asset,
                &portable_template,
                None,
            ))
            .unwrap();
        let package_path = source_temp.path().join("image-package");
        source
            .export_current_package("macro-one", &package_path)
            .unwrap();
        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();
        let pending = match destination.prepare_package_import(&package_path).unwrap() {
            PreparedPackageImport::Image(pending) => pending,
            PreparedPackageImport::Text(_) => panic!("image package was misclassified"),
        };
        let (_local_template, local_bytes) = fixture_png(91);
        let target_region = fixture_capture_png(64, 48, 22);
        let negative_capture = fixture_capture_png(64, 48, 200);
        let negatives = vec![LocalNegativeImageSample {
            stable_id: "local-negative/a".to_string(),
            png: &negative_capture,
        }];
        let completion = destination
            .complete_local_image_reverification(
                &pending,
                LocalImageRuleVerificationInput {
                    rule_id: "image-one",
                    template_png: &local_bytes,
                    mask_png: None,
                    target: local_target(),
                    region: local_region(),
                    target_region_png: &target_region,
                    negative_samples: &negatives,
                    maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
                },
            )
            .unwrap();
        destination.fail_next_import_after_asset_installs();

        assert!(
            destination
                .commit_image_package_import(pending, vec![completion])
                .is_err()
        );
        assert!(!destination.root.join("definitions/macro-one").exists());
        assert_eq!(
            fs::read_dir(destination.root.join("assets"))
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "png"))
                .count(),
            0
        );
    }

    #[test]
    fn oversized_package_definition_is_rejected_before_read_and_mutates_nothing() {
        let package_temp = tempfile::tempdir().unwrap();
        let package = package_temp.path().join("oversized");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("manifest.json"),
            serde_json::to_vec(&PackageManifest {
                schema_version: PACKAGE_SCHEMA_VERSION,
                definition: PathBuf::from("macro.json"),
                assets: vec![],
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(
            package.join("macro.json"),
            vec![b' '; (MAX_PACKAGE_DEFINITION_BYTES + 1) as usize],
        )
        .unwrap();
        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();

        let error = destination.prepare_package_import(&package).unwrap_err();

        assert!(error.to_string().contains("exceed maximum"));
        assert_eq!(
            fs::read_dir(destination.root.join("definitions"))
                .unwrap()
                .count(),
            0
        );
    }

    #[cfg(windows)]
    #[test]
    fn package_rejects_reparse_point_even_when_its_target_stays_inside_package() {
        let source_temp = tempfile::tempdir().unwrap();
        let source = MacroStore::open(source_temp.path()).unwrap();
        source.save_validated(compilable_text_definition()).unwrap();
        let package = source_temp.path().join("text-package");
        source
            .export_current_package("macro-one", &package)
            .unwrap();
        fs::rename(package.join("macro.json"), package.join("real-macro.json")).unwrap();
        match std::os::windows::fs::symlink_file("real-macro.json", package.join("macro.json")) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("could not create package reparse-point fixture: {error}"),
        }

        let error = MacroStore::validate_package(&package).unwrap_err();

        assert!(format!("{error:#}").contains("reparse"));
    }

    #[cfg(windows)]
    #[test]
    fn package_reads_stay_bound_to_verified_root_after_root_path_is_replaced() {
        let source_temp = tempfile::tempdir().unwrap();
        let source = MacroStore::open(source_temp.path()).unwrap();
        let (template, bytes) = fixture_png(7);
        let asset = source.assets().put_png(&bytes).unwrap();
        source
            .save_validated(compilable_image_definition(asset, &template, None))
            .unwrap();
        let package = source_temp.path().join("package");
        source
            .export_current_package("macro-one", &package)
            .unwrap();
        let root = open_package_root(&package).unwrap();
        let manifest: PackageManifest = serde_json::from_slice(
            &read_bounded_package_file(
                &root,
                Path::new("manifest.json"),
                MAX_PACKAGE_MANIFEST_BYTES,
                "package manifest",
            )
            .unwrap(),
        )
        .unwrap();
        let relative_asset = manifest.assets[0].path.clone();

        let original = source_temp.path().join("original-package");
        fs::rename(&package, &original).unwrap();
        fs::create_dir_all(package.join(relative_asset.parent().unwrap())).unwrap();
        fs::write(package.join(&relative_asset), b"replacement-package-asset").unwrap();

        let read = read_bounded_package_file(
            &root,
            &relative_asset,
            MAX_PACKAGE_ASSET_BYTES,
            "package asset",
        )
        .unwrap();

        assert_eq!(read, bytes);
    }

    #[cfg(windows)]
    #[test]
    fn package_rejects_intermediate_assets_junction_inside_verified_root() {
        let source_temp = tempfile::tempdir().unwrap();
        let source = MacroStore::open(source_temp.path()).unwrap();
        let (template, bytes) = fixture_png(7);
        let asset = source.assets().put_png(&bytes).unwrap();
        source
            .save_validated(compilable_image_definition(asset, &template, None))
            .unwrap();
        let package = source_temp.path().join("package");
        source
            .export_current_package("macro-one", &package)
            .unwrap();
        let assets = package.join("assets");
        let real_assets = package.join("real-assets");
        fs::rename(&assets, &real_assets).unwrap();
        let command = format!(
            "New-Item -ItemType Junction -Path '{}' -Target '{}' | Out-Null",
            assets.display(),
            real_assets.display(),
        );
        let status = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &command])
            .status()
            .unwrap();
        assert!(status.success(), "could not create assets junction fixture");

        let error = MacroStore::validate_package(&package).unwrap_err();

        assert!(format!("{error:#}").contains("reparse"));
    }

    #[test]
    fn oversized_package_asset_count_is_rejected_before_asset_reads() {
        let package_temp = tempfile::tempdir().unwrap();
        let package = package_temp.path().join("too-many-assets");
        fs::create_dir_all(&package).unwrap();
        let placeholder = AssetRef {
            id: "not-read".to_string(),
            revision: 1,
            content_hash: "0".repeat(64),
        };
        let assets = (0..=MAX_PACKAGE_ASSETS)
            .map(|index| PackageManifestAsset {
                asset: AssetRef {
                    id: format!("not-read-{index}"),
                    ..placeholder.clone()
                },
                path: PathBuf::from(format!("missing-{index}.png")),
            })
            .collect();
        fs::write(
            package.join("manifest.json"),
            serde_json::to_vec(&PackageManifest {
                schema_version: PACKAGE_SCHEMA_VERSION,
                definition: PathBuf::from("macro.json"),
                assets,
            })
            .unwrap(),
        )
        .unwrap();
        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();

        let error = destination.prepare_package_import(&package).unwrap_err();

        assert!(error.to_string().contains("asset count"));
        assert_eq!(
            fs::read_dir(destination.root.join("definitions"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn oversized_package_asset_is_rejected_before_read_and_mutates_nothing() {
        let source_temp = tempfile::tempdir().unwrap();
        let source = MacroStore::open(source_temp.path()).unwrap();
        let (template, bytes) = fixture_png(7);
        let asset = source.assets().put_png(&bytes).unwrap();
        source
            .save_validated(compilable_image_definition(asset.clone(), &template, None))
            .unwrap();
        let package = source_temp.path().join("oversized-asset");
        source
            .export_current_package("macro-one", &package)
            .unwrap();
        fs::write(
            package
                .join("assets")
                .join(format!("{}.png", asset.content_hash)),
            vec![0_u8; (MAX_PACKAGE_ASSET_BYTES + 1) as usize],
        )
        .unwrap();
        let destination_temp = tempfile::tempdir().unwrap();
        let destination = MacroStore::open(destination_temp.path()).unwrap();

        let error = destination.prepare_package_import(&package).unwrap_err();

        assert!(error.to_string().contains("exceed maximum"));
        assert_eq!(
            fs::read_dir(destination.root.join("definitions"))
                .unwrap()
                .count(),
            0
        );
    }
}
