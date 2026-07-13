use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{AssetRef, MACRO_SCHEMA_VERSION, MacroDefinition};

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

#[derive(Debug, Clone)]
pub struct AssetStore {
    root: PathBuf,
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
        if sha256_hex(bytes) != asset.content_hash {
            bail!("asset content hash does not match bytes");
        }
        fs::create_dir_all(&self.root)?;
        let path = self.path_for_hash(&asset.content_hash)?;
        if path.exists() {
            let existing = fs::read(&path)?;
            if sha256_hex(&existing) != asset.content_hash {
                bail!("existing asset is corrupt: {}", asset.content_hash);
            }
            return Ok(());
        }
        atomic_write(&path, bytes)
    }

    pub fn read(&self, asset: &AssetRef) -> Result<Vec<u8>> {
        let path = self.path_for_hash(&asset.content_hash)?;
        let bytes =
            fs::read(&path).with_context(|| format!("missing asset {}", asset.content_hash))?;
        if sha256_hex(&bytes) != asset.content_hash {
            bail!("corrupt asset {}", asset.content_hash);
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
}

impl MacroStore {
    pub fn open(root: &Path) -> Result<Self> {
        let root = root.join("macro_data");
        let assets = AssetStore {
            root: root.join("assets"),
        };
        fs::create_dir_all(root.join("definitions"))?;
        fs::create_dir_all(&assets.root)?;
        fs::create_dir_all(root.join("runs"))?;
        Ok(Self { root, assets })
    }

    pub fn assets(&self) -> &AssetStore {
        &self.assets
    }

    pub fn save(&self, definition: MacroDefinition) -> Result<SavedRevision> {
        if definition.schema_version != MACRO_SCHEMA_VERSION {
            bail!("unsupported macro schema {}", definition.schema_version);
        }
        validate_component("macro ID", &definition.id)?;
        let mut pinned_assets = Vec::new();
        let mut pinned_hashes = HashSet::new();
        for asset in referenced_assets(&definition) {
            let bytes = self.assets.read(asset)?;
            if pinned_hashes.insert(asset.content_hash.clone()) {
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
        if revision_path.exists() {
            if fs::read(&revision_path)? != bytes {
                bail!(
                    "immutable revision {} already exists with different content",
                    saved.definition.revision
                );
            }
        } else {
            atomic_write(&revision_path, &bytes)?;
        }
        atomic_write(&directory.join("current.json"), &bytes)?;
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

        // Validate every package-controlled path before parsing payloads so a
        // malformed definition cannot hide an outside-package reference.
        for asset in &manifest.assets {
            package_file(&root, &asset.path)?;
        }

        let definition_path = package_file(&root, &manifest.definition)?;
        let definition: MacroDefinition = serde_json::from_slice(
            &fs::read(&definition_path).context("could not read package definition")?,
        )
        .context("corrupt macro JSON")?;
        if definition.schema_version != MACRO_SCHEMA_VERSION {
            bail!("unsupported macro schema {}", definition.schema_version);
        }

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

        Ok(MacroPackage {
            schema_version: manifest.schema_version,
            definition,
            assets,
        })
    }

    pub fn export_package(&self, saved: &SavedRevision, package_root: &Path) -> Result<()> {
        fs::create_dir_all(package_root)?;
        let canonical_root = package_root.canonicalize()?;
        let definition_path = package_output_file(&canonical_root, Path::new("macro.json"))?;
        let definition_bytes = serde_json::to_vec_pretty(&saved.definition)?;
        atomic_write(&definition_path, &definition_bytes)?;

        let mut manifest_assets = Vec::new();
        let mut seen = HashSet::new();
        for asset in referenced_assets(&saved.definition) {
            if !seen.insert(asset.clone()) {
                continue;
            }
            let bytes = self.assets.read(asset)?;
            let relative_path = PathBuf::from("assets").join(format!("{}.png", asset.content_hash));
            let output_path = package_output_file(&canonical_root, &relative_path)?;
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
        let manifest_path = package_output_file(&canonical_root, Path::new("manifest.json"))?;
        atomic_write(&manifest_path, &serde_json::to_vec_pretty(&manifest)?)
    }

    pub fn import_package(&self, package_root: &Path) -> Result<SavedRevision> {
        let mut package = Self::validate_package(package_root)?;
        let saved_definitions = self.saved_definitions()?;
        let existing_macro_ids = fs::read_dir(self.root.join("definitions"))?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        package.definition.id = remap_id(&package.definition.id, &existing_macro_ids);

        let existing_assets: HashMap<(String, u64), String> = saved_definitions
            .iter()
            .flat_map(referenced_assets)
            .map(|asset| {
                (
                    (asset.id.clone(), asset.revision),
                    asset.content_hash.clone(),
                )
            })
            .collect();
        let mut remaps = HashMap::new();
        let mut reserved_asset_ids: HashSet<String> =
            existing_assets.keys().map(|(id, _)| id.clone()).collect();
        for package_asset in &package.assets {
            let key = (package_asset.asset.id.clone(), package_asset.asset.revision);
            if existing_assets
                .get(&key)
                .is_some_and(|hash| hash != &package_asset.asset.content_hash)
            {
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
        for package_asset in &mut package.assets {
            if let Some(id) =
                remaps.get(&(package_asset.asset.id.clone(), package_asset.asset.revision))
            {
                package_asset.asset.id = id.clone();
            }
            self.assets
                .put_png_revision(package_asset.asset.clone(), &package_asset.bytes)?;
        }
        self.save(package.definition)
    }

    pub fn open_journal(&self, run_id: &str, limits: JournalLimits) -> Result<RunJournal> {
        validate_component("run ID", run_id)?;
        if limits.max_bytes_per_run == 0 || limits.max_runs == 0 {
            bail!("journal limits must be non-zero");
        }
        let runs = self.root.join("runs");
        fs::create_dir_all(&runs)?;
        prune_run_files(&runs, limits.max_runs.saturating_sub(1))?;
        let path = runs.join(format!("{run_id}.jsonl"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)?;
        Ok(RunJournal {
            path,
            file,
            max_bytes: limits.max_bytes_per_run,
            bytes_written: 0,
            failed: false,
        })
    }

    pub fn cleanup_orphan_assets(&self, running_assets: &HashSet<AssetRef>) -> Result<usize> {
        let mut referenced_hashes: HashSet<String> = running_assets
            .iter()
            .map(|asset| asset.content_hash.clone())
            .collect();
        let definitions_root = self.root.join("definitions");
        for macro_directory in fs::read_dir(definitions_root)? {
            let macro_directory = macro_directory?;
            if !macro_directory.file_type()?.is_dir() {
                continue;
            }
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
                let definition: MacroDefinition =
                    serde_json::from_slice(&fs::read(revision_file.path())?)
                        .context("corrupt saved macro definition blocks orphan cleanup")?;
                referenced_hashes
                    .extend(referenced_assets(&definition).map(|asset| asset.content_hash.clone()));
            }
        }

        let mut removed = 0;
        for entry in fs::read_dir(&self.assets.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file()
                || entry.path().extension().and_then(|ext| ext.to_str()) != Some("png")
            {
                continue;
            }
            let Some(hash) = entry
                .path()
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            if hash.len() == 64
                && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                && !referenced_hashes.contains(&hash)
            {
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn saved_definitions(&self) -> Result<Vec<MacroDefinition>> {
        let root = self.root.join("definitions");
        let mut definitions = Vec::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            for revision in fs::read_dir(entry.path())? {
                let revision = revision?;
                if !revision.file_type()?.is_file()
                    || revision.file_name() == "current.json"
                    || revision.path().extension().and_then(|ext| ext.to_str()) != Some("json")
                {
                    continue;
                }
                definitions.push(
                    serde_json::from_slice(&fs::read(revision.path())?)
                        .context("corrupt saved macro definition")?,
                );
            }
        }
        Ok(definitions)
    }
}

fn prune_run_files(runs: &Path, keep: usize) -> Result<()> {
    let mut files = fs::read_dir(runs)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl")
                && entry.file_type().is_ok_and(|kind| kind.is_file())
        })
        .collect::<Vec<_>>();
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

fn package_file(root: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("reference points outside package");
    }
    let joined = root.join(relative);
    let canonical = joined
        .canonicalize()
        .with_context(|| format!("missing package file: {}", relative.display()))?;
    if !canonical.starts_with(root) {
        bail!("reference points outside package");
    }
    Ok(canonical)
}

fn package_output_file(root: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("output points outside package");
    }
    let output = root.join(relative);
    let parent = output.parent().context("package output has no parent")?;
    fs::create_dir_all(parent)?;
    let canonical_parent = parent.canonicalize()?;
    if !canonical_parent.starts_with(root) {
        bail!("output points outside package");
    }
    Ok(output)
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
        let mut journal = store
            .open_journal("run-one", JournalLimits::new(220, 3))
            .unwrap();

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
            store
                .open_journal(&format!("run-{run}"), JournalLimits::new(1_024, 2))
                .unwrap();
        }

        let runs = fs::read_dir(temp.path().join("macro_data/runs"))
            .unwrap()
            .count();
        assert_eq!(runs, 2);
    }
}
