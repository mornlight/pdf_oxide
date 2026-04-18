use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::UNIX_EPOCH;

pub(crate) const FULL_INDEX_SCHEMA_VERSION: u32 = 1;
pub(crate) const FONTDB_VERSION: &str = "0.23";
pub(crate) const HOT_ANSWER_SCHEMA_VERSION: u32 = 1;
pub(crate) const HOT_ANSWER_SIGNATURE_VERSION: u32 = 1;

static TEST_CACHE_DIR: LazyLock<Mutex<Option<PathBuf>>> = LazyLock::new(|| Mutex::new(None));
#[cfg(test)]
static TEST_DEFAULT_CACHE_ROOT: LazyLock<Mutex<Option<Option<PathBuf>>>> =
    LazyLock::new(|| Mutex::new(None));
#[cfg(test)]
static TEST_CACHE_ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static CACHE_IO_AUDIT: LazyLock<Mutex<CacheIoAudit>> =
    LazyLock::new(|| Mutex::new(CacheIoAudit::default()));

/// Snapshot of system-font cache I/O side effects observed in the current process.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CacheIoAudit {
    /// Resolved cache directory used for on-disk artifacts, when available.
    pub cache_dir: Option<String>,
    /// Number of attempts to write the full persisted font index.
    pub full_index_write_attempts: usize,
    /// Number of successful writes of the full persisted font index.
    pub full_index_write_successes: usize,
    /// Number of attempts to write the persisted hot-answer cache.
    pub hot_answer_write_attempts: usize,
    /// Number of successful writes of the persisted hot-answer cache.
    pub hot_answer_write_successes: usize,
    /// Most recent cache read/write error observed by the helper layer.
    pub last_error: Option<String>,
}

fn with_cache_io_audit<F>(f: F)
where
    F: FnOnce(&mut CacheIoAudit),
{
    if let Ok(mut audit) = CACHE_IO_AUDIT.lock() {
        f(&mut audit);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct FontFileMetadata {
    pub(crate) len: u64,
    pub(crate) modified_unix_secs: Option<u64>,
}

impl FontFileMetadata {
    pub(crate) fn from_path(path: &Path) -> io::Result<Self> {
        let metadata = fs::metadata(path)?;
        let modified_unix_secs = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs());
        Ok(Self {
            len: metadata.len(),
            modified_unix_secs,
        })
    }

    pub(crate) fn matches_path(&self, path: &Path) -> bool {
        Self::from_path(path).is_ok_and(|actual| actual == *self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedFontFile {
    pub(crate) path: PathBuf,
    pub(crate) metadata: FontFileMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DiskSystemFontIndex {
    pub(crate) schema_version: u32,
    pub(crate) platform: String,
    pub(crate) fontdb_version: String,
    pub(crate) font_roots: Vec<PathBuf>,
    pub(crate) font_files: Vec<CachedFontFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub(crate) struct SystemFontRequestSignature {
    pub(crate) signature_version: u32,
    pub(crate) base_name: String,
    pub(crate) bold_like: bool,
    pub(crate) italic_like: bool,
    pub(crate) serif_like: bool,
    pub(crate) cjk_probability: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HotFontAnswer {
    pub(crate) path: PathBuf,
    pub(crate) face_index: u32,
    pub(crate) metadata: FontFileMetadata,
}

impl HotFontAnswer {
    pub(crate) fn is_valid(&self) -> bool {
        self.metadata.matches_path(&self.path)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiskHotAnswerCache {
    schema_version: u32,
    platform: String,
    answers: Vec<(SystemFontRequestSignature, HotFontAnswer)>,
}

impl SystemFontRequestSignature {
    pub(crate) fn from_pdf_font_name(pdf_font_name: &str) -> Self {
        let base_name = pdf_font_name
            .split_once('+')
            .map(|(_, suffix)| suffix)
            .unwrap_or(pdf_font_name)
            .to_string();
        Self {
            signature_version: HOT_ANSWER_SIGNATURE_VERSION,
            bold_like: pdf_font_name.contains("Bold") || pdf_font_name.contains("Black"),
            italic_like: pdf_font_name.contains("Italic") || pdf_font_name.contains("Oblique"),
            serif_like: base_name.contains("Roman")
                || base_name.contains("Serif")
                || base_name.contains("Times")
                || base_name.contains("Palladio")
                || base_name.contains("Palatino")
                || base_name.contains("Bookman")
                || base_name.contains("Garamond")
                || base_name.contains("Century")
                || base_name.contains("Georgia")
                || base_name.contains("CMR")
                || base_name.contains("CMBX")
                || base_name.contains("CMTI"),
            cjk_probability: base_name.contains("GB2312")
                || base_name.contains("Identity")
                || base_name.contains("楷体")
                || base_name.contains("æ¥·ä½")
                || base_name.contains("宋体")
                || base_name.contains("å®\u{008b}ä½")
                || base_name.contains("黑体")
                || base_name.contains("é»\u{0091}ä½")
                || base_name.contains("FangSong")
                || base_name.contains("SimSun")
                || base_name.contains("SimHei")
                || base_name.contains("KaiTi")
                || pdf_font_name == "F1",
            base_name,
        }
    }
}

pub(crate) fn platform_key() -> &'static str {
    std::env::consts::OS
}

fn push_unique_root(roots: &mut Vec<PathBuf>, root: PathBuf) {
    if !roots.iter().any(|existing| existing == &root) {
        roots.push(root);
    }
}

pub(crate) fn current_font_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    #[cfg(target_os = "windows")]
    {
        if let Some(system_root) = std::env::var_os("SYSTEMROOT") {
            push_unique_root(&mut roots, PathBuf::from(system_root).join("Fonts"));
        } else {
            push_unique_root(&mut roots, PathBuf::from("C:\\Windows\\Fonts"));
        }

        if let Ok(home) = std::env::var("USERPROFILE") {
            let home = PathBuf::from(home);
            push_unique_root(&mut roots, home.join("AppData\\Local\\Microsoft\\Windows\\Fonts"));
            push_unique_root(&mut roots, home.join("AppData\\Roaming\\Microsoft\\Windows\\Fonts"));
        }
    }

    #[cfg(target_os = "macos")]
    {
        push_unique_root(&mut roots, PathBuf::from("/Library/Fonts"));
        push_unique_root(&mut roots, PathBuf::from("/System/Library/Fonts"));
        push_unique_root(&mut roots, PathBuf::from("/Network/Library/Fonts"));

        if let Ok(home) = std::env::var("HOME") {
            push_unique_root(&mut roots, PathBuf::from(home).join("Library/Fonts"));
        }
    }

    #[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
    {
        push_unique_root(&mut roots, PathBuf::from("/usr/share/fonts"));
        push_unique_root(&mut roots, PathBuf::from("/usr/local/share/fonts"));

        if let Ok(home) = std::env::var("HOME") {
            let home = PathBuf::from(home);
            push_unique_root(&mut roots, home.join(".fonts"));
            push_unique_root(&mut roots, home.join(".local/share/fonts"));
        }
    }

    roots.sort();
    roots
}

pub(crate) fn cache_dir() -> Option<PathBuf> {
    let resolved = if let Ok(guard) = TEST_CACHE_DIR.lock() {
        if let Some(path) = guard.as_ref() {
            Some(path.clone())
        } else {
            None
        }
    } else {
        None
    }
    .or_else(|| {
        if let Ok(path) = std::env::var("PDF_OXIDE_SYSTEM_FONT_CACHE_DIR") {
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
        None
    })
    .or_else(|| {
        #[cfg(test)]
        if let Ok(guard) = TEST_DEFAULT_CACHE_ROOT.lock() {
            if let Some(path) = guard.as_ref() {
                return path.clone().map(|root| root.join("pdf_oxide"));
            }
        }
        dirs::cache_dir().map(|root| root.join("pdf_oxide"))
    })
    .or_else(|| {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(".cache").join("pdf_oxide"))
    });

    with_cache_io_audit(|audit| {
        audit.cache_dir = resolved.as_ref().map(|path| path.display().to_string());
    });
    resolved
}

#[cfg(test)]
pub(crate) fn set_test_default_cache_root(path: Option<PathBuf>) {
    *TEST_DEFAULT_CACHE_ROOT
        .lock()
        .expect("test default cache root lock") = Some(path);
}

#[cfg(test)]
pub(crate) fn clear_test_default_cache_root() {
    *TEST_DEFAULT_CACHE_ROOT
        .lock()
        .expect("test default cache root lock") = None;
}

pub(crate) fn full_index_path() -> Option<PathBuf> {
    cache_dir().map(|dir| dir.join("system-font-index-v1.json"))
}

pub(crate) fn hot_answer_path() -> Option<PathBuf> {
    cache_dir().map(|dir| dir.join("system-font-hot-answers-v1.json"))
}

pub(crate) fn read_index_file(path: &Path) -> io::Result<DiskSystemFontIndex> {
    let data = fs::read(path)?;
    serde_json::from_slice(&data).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(crate) fn read_valid_index_file(path: &Path) -> Option<DiskSystemFontIndex> {
    let index = read_index_file(path).ok()?;
    if index.schema_version != FULL_INDEX_SCHEMA_VERSION
        || index.platform != platform_key()
        || index.fontdb_version != FONTDB_VERSION
        || index.font_roots != current_font_roots()
    {
        return None;
    }
    if index
        .font_files
        .iter()
        .any(|entry| !entry.metadata.matches_path(&entry.path))
    {
        return None;
    }
    Some(index)
}

pub(crate) fn write_index_file(path: &Path, index: &DiskSystemFontIndex) -> io::Result<()> {
    with_cache_io_audit(|audit| {
        audit.full_index_write_attempts += 1;
    });
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec(index)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    fs::write(&tmp, data)?;
    fs::rename(tmp, path)?;
    with_cache_io_audit(|audit| {
        audit.full_index_write_successes += 1;
    });
    Ok(())
}

fn read_hot_answer_file(path: &Path) -> io::Result<DiskHotAnswerCache> {
    let data = fs::read(path)?;
    serde_json::from_slice(&data).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn write_hot_answer_file(path: &Path, cache: &DiskHotAnswerCache) -> io::Result<()> {
    with_cache_io_audit(|audit| {
        audit.hot_answer_write_attempts += 1;
    });
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec(cache)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    fs::write(&tmp, data)?;
    fs::rename(tmp, path)?;
    with_cache_io_audit(|audit| {
        audit.hot_answer_write_successes += 1;
    });
    Ok(())
}

fn face_file_path(source: &fontdb::Source) -> Option<&Path> {
    match source {
        fontdb::Source::File(path) => Some(path.as_path()),
        _ => None,
    }
}

pub(crate) fn build_index_from_database(database: &fontdb::Database) -> DiskSystemFontIndex {
    let mut seen = HashSet::new();
    let mut font_files = Vec::new();
    for face in database.faces() {
        let Some(path) = face_file_path(&face.source) else {
            continue;
        };
        if !seen.insert(path.to_path_buf()) {
            continue;
        }
        if let Ok(metadata) = FontFileMetadata::from_path(path) {
            font_files.push(CachedFontFile {
                path: path.to_path_buf(),
                metadata,
            });
        }
    }
    DiskSystemFontIndex {
        schema_version: FULL_INDEX_SCHEMA_VERSION,
        platform: platform_key().to_string(),
        fontdb_version: FONTDB_VERSION.to_string(),
        font_roots: current_font_roots(),
        font_files,
    }
}

pub(crate) fn database_from_index(index: &DiskSystemFontIndex) -> Option<fontdb::Database> {
    let mut database = fontdb::Database::new();
    for entry in &index.font_files {
        if !entry.metadata.matches_path(&entry.path) {
            return None;
        }
        if database.load_font_file(&entry.path).is_err() {
            return None;
        }
    }

    if database.is_empty() {
        None
    } else {
        Some(database)
    }
}

pub(crate) fn load_or_build_database() -> fontdb::Database {
    if let Some(path) = full_index_path() {
        if let Some(index) = read_valid_index_file(&path) {
            if let Some(database) = database_from_index(&index) {
                return database;
            }
        }
    }

    let mut database = fontdb::Database::new();
    database.load_system_fonts();

    if let Some(path) = full_index_path() {
        let index = build_index_from_database(&database);
        if let Err(error) = write_index_file(&path, &index) {
            with_cache_io_audit(|audit| {
                audit.last_error = Some(format!("write_index_file {}: {}", path.display(), error));
            });
            log::debug!("failed to write system font index cache: {}", error);
        }
    }

    database
}

pub(crate) fn read_hot_answer_cache() -> HashMap<SystemFontRequestSignature, HotFontAnswer> {
    let Some(path) = hot_answer_path() else {
        return HashMap::new();
    };
    let Ok(cache) = read_hot_answer_file(&path) else {
        return HashMap::new();
    };
    if cache.schema_version != HOT_ANSWER_SCHEMA_VERSION || cache.platform != platform_key() {
        return HashMap::new();
    }
    cache.answers.into_iter().collect()
}

pub(crate) fn write_hot_answer_cache(
    cache: &HashMap<SystemFontRequestSignature, HotFontAnswer>,
) -> io::Result<()> {
    let Some(path) = hot_answer_path() else {
        return Ok(());
    };
    let disk = DiskHotAnswerCache {
        schema_version: HOT_ANSWER_SCHEMA_VERSION,
        platform: platform_key().to_string(),
        answers: cache.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
    };
    write_hot_answer_file(&path, &disk).inspect_err(|error| {
        with_cache_io_audit(|audit| {
            audit.last_error = Some(format!("write_hot_answer_file {}: {}", path.display(), error));
        });
    })
}

/// Clear the in-process cache I/O audit snapshot.
#[cfg(feature = "system-font-audit")]
pub fn reset_cache_io_audit() {
    if let Ok(mut audit) = CACHE_IO_AUDIT.lock() {
        *audit = CacheIoAudit::default();
    }
}

/// Return the current in-process cache I/O audit snapshot.
#[cfg(feature = "system-font-audit")]
pub fn cache_io_audit_snapshot() -> CacheIoAudit {
    CACHE_IO_AUDIT
        .lock()
        .map(|audit| audit.clone())
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) fn set_test_cache_dir(path: PathBuf) {
    *TEST_CACHE_DIR.lock().expect("test cache dir lock") = Some(path);
}

#[cfg(test)]
pub(crate) fn clear_test_cache_dir() {
    *TEST_CACHE_DIR.lock().expect("test cache dir lock") = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn cache_test_guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_CACHE_ENV_LOCK.lock().expect("test cache env lock")
    }

    #[test]
    fn cache_roundtrip_preserves_font_file_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let font_path = dir.path().join("Example.ttf");
        fs::write(&font_path, b"not-a-real-font").expect("write font");

        let metadata = FontFileMetadata::from_path(&font_path).expect("metadata");
        let index = DiskSystemFontIndex {
            schema_version: FULL_INDEX_SCHEMA_VERSION,
            platform: platform_key().to_string(),
            fontdb_version: FONTDB_VERSION.to_string(),
            font_roots: current_font_roots(),
            font_files: vec![CachedFontFile {
                path: font_path.clone(),
                metadata: metadata.clone(),
            }],
        };

        let cache_path = dir.path().join("system-font-index.json");
        write_index_file(&cache_path, &index).expect("write index");
        let restored = read_index_file(&cache_path).expect("read index");

        assert_eq!(restored.schema_version, FULL_INDEX_SCHEMA_VERSION);
        assert_eq!(restored.platform, platform_key());
        assert_eq!(restored.font_roots, current_font_roots());
        assert_eq!(restored.font_files.len(), 1);
        assert_eq!(restored.font_files[0].path, font_path);
        assert_eq!(restored.font_files[0].metadata.len, metadata.len);
        assert!(restored.font_files[0]
            .metadata
            .matches_path(&restored.font_files[0].path));
    }

    #[test]
    fn invalid_index_schema_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache_path = dir.path().join("system-font-index.json");
        fs::write(
            &cache_path,
            r#"{"schema_version":0,"platform":"bad","fontdb_version":"bad","font_roots":[],"font_files":[]}"#,
        )
        .expect("write bad index");

        assert!(read_valid_index_file(&cache_path).is_none());
    }

    #[test]
    fn changed_font_roots_invalidate_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache_path = dir.path().join("system-font-index.json");
        let index = DiskSystemFontIndex {
            schema_version: FULL_INDEX_SCHEMA_VERSION,
            platform: platform_key().to_string(),
            fontdb_version: FONTDB_VERSION.to_string(),
            font_roots: vec![PathBuf::from("/definitely/not/current/font/root")],
            font_files: Vec::new(),
        };

        write_index_file(&cache_path, &index).expect("write index");
        assert!(read_valid_index_file(&cache_path).is_none());
    }

    #[test]
    fn request_signature_removes_subset_prefix_and_tracks_style_bits() {
        let plain = SystemFontRequestSignature::from_pdf_font_name("ABCDEE+TimesNewRoman");
        let bold = SystemFontRequestSignature::from_pdf_font_name("ABCDEE+TimesNewRoman-Bold");

        assert_eq!(plain.base_name, "TimesNewRoman");
        assert!(!plain.bold_like);
        assert!(plain.serif_like);
        assert!(bold.bold_like);
        assert_ne!(plain, bold);
    }

    #[test]
    fn stale_hot_answer_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let font_path = dir.path().join("Answer.ttf");
        fs::write(&font_path, b"font-a").expect("write font");
        let answer = HotFontAnswer {
            path: font_path.clone(),
            face_index: 0,
            metadata: FontFileMetadata::from_path(&font_path).expect("metadata"),
        };
        fs::write(&font_path, b"font-b-with-different-size").expect("mutate font");

        assert!(!answer.is_valid());
    }

    #[test]
    fn cache_dir_prefers_test_override() {
        let _guard = cache_test_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::remove_var("PDF_OXIDE_SYSTEM_FONT_CACHE_DIR");
        clear_test_default_cache_root();
        clear_test_cache_dir();
        set_test_cache_dir(dir.path().join("test-override"));

        assert_eq!(cache_dir(), Some(dir.path().join("test-override")));

        clear_test_cache_dir();
        clear_test_default_cache_root();
    }

    #[test]
    fn cache_dir_uses_env_override_when_present() {
        let _guard = cache_test_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        clear_test_default_cache_root();
        clear_test_cache_dir();
        std::env::set_var("PDF_OXIDE_SYSTEM_FONT_CACHE_DIR", dir.path().join("env-override"));

        assert_eq!(cache_dir(), Some(dir.path().join("env-override")));

        std::env::remove_var("PDF_OXIDE_SYSTEM_FONT_CACHE_DIR");
        clear_test_cache_dir();
        clear_test_default_cache_root();
    }

    #[test]
    fn cache_dir_appends_pdf_oxide_to_default_root() {
        let _guard = cache_test_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::remove_var("PDF_OXIDE_SYSTEM_FONT_CACHE_DIR");
        clear_test_cache_dir();
        set_test_default_cache_root(Some(dir.path().join("cache-root")));

        assert_eq!(cache_dir(), Some(dir.path().join("cache-root").join("pdf_oxide")));

        clear_test_cache_dir();
        clear_test_default_cache_root();
    }
}
