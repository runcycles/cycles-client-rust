//! Durable, byte-compatible pending-commit journal.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use pbkdf2::pbkdf2_hmac;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::CyclesConfig;
use crate::models::{CommitRequest, EventCreateRequest};

const RECORD_VERSION: u8 = 1;
const JOURNAL_SUFFIX: &str = ".json";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum JournalMode {
    #[default]
    Commit,
    Event,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingCommitRecord {
    pub version: u8,
    pub reservation_id: String,
    pub base_url: String,
    #[serde(default)]
    pub mode: JournalMode,
    pub commit_body: Option<CommitRequest>,
    pub event_fallback_body: Option<EventCreateRequest>,
    pub recorded_at_ms: u64,
    pub not_before_ms: Option<u64>,
}

impl PendingCommitRecord {
    pub(crate) fn commit(
        reservation_id: String,
        base_url: String,
        commit_body: CommitRequest,
        event_fallback_body: EventCreateRequest,
    ) -> Self {
        Self {
            version: RECORD_VERSION,
            reservation_id,
            base_url,
            mode: JournalMode::Commit,
            commit_body: Some(commit_body),
            event_fallback_body: Some(event_fallback_body),
            recorded_at_ms: now_ms(),
            not_before_ms: None,
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != RECORD_VERSION {
            return Err(format!("unsupported journal version {}", self.version));
        }
        if self.reservation_id.is_empty() {
            return Err("journal record missing reservation_id".to_string());
        }
        match self.mode {
            JournalMode::Commit if self.commit_body.is_none() => {
                Err("commit-mode journal record missing commit_body".to_string())
            }
            JournalMode::Event if self.event_fallback_body.is_none() => {
                Err("event-mode journal record missing event_fallback_body".to_string())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CommitJournal {
    directory: PathBuf,
}

impl CommitJournal {
    pub(crate) fn for_config(config: &CyclesConfig) -> Option<Self> {
        if !config.journal_enabled {
            return None;
        }
        let base = match &config.journal_dir {
            Some(path) => path.clone(),
            None => default_journal_dir()?,
        };
        let fingerprint =
            auth_fingerprint(&config.base_url, &config.api_key, config.tenant.as_deref());
        Some(Self {
            directory: base.join(fingerprint),
        })
    }

    pub(crate) fn record(&self, entry: &PendingCommitRecord) -> Result<(), String> {
        entry.validate()?;
        fs::create_dir_all(&self.directory).map_err(|error| error.to_string())?;
        if let Some(base) = self.directory.parent() {
            restrict_directory_permissions(base);
        }
        restrict_directory_permissions(&self.directory);

        let target = self.path_for(&entry.reservation_id);
        let mut temporary =
            tempfile::NamedTempFile::new_in(&self.directory).map_err(|error| error.to_string())?;
        serde_json::to_writer(&mut temporary, entry).map_err(|error| error.to_string())?;
        temporary.flush().map_err(|error| error.to_string())?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|error| error.to_string())?;
        restrict_file_permissions(temporary.path());
        temporary
            .persist(&target)
            .map_err(|error| error.error.to_string())?;
        Ok(())
    }

    pub(crate) fn discard(&self, reservation_id: &str) -> Result<(), String> {
        remove_if_exists(&self.path_for(reservation_id))?;
        let legacy = self.legacy_path_for(reservation_id);
        if legacy.exists() {
            // Legacy sanitization was collision-prone. Delete only when the
            // record itself proves it belongs to the requested identifier.
            if let Ok(raw) = fs::read_to_string(&legacy) {
                if let Ok(record) = serde_json::from_str::<PendingCommitRecord>(&raw) {
                    if record.validate().is_ok() && record.reservation_id == reservation_id {
                        remove_if_exists(&legacy)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn load_pending(&self, base_url: &str) -> Vec<PendingCommitRecord> {
        let mut records = Vec::new();
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return records,
            Err(error) => {
                warn_journal_io("read directory", &self.directory, &error);
                return records;
            }
        };
        let mut paths = entries
            .filter_map(|entry| match entry {
                Ok(entry) => Some(entry),
                Err(error) => {
                    warn_journal_io("inspect entry", &self.directory, &error);
                    None
                }
            })
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect::<Vec<_>>();
        paths.sort();

        for path in paths {
            match fs::read_to_string(&path)
                .map_err(|error| error.to_string())
                .and_then(|raw| {
                    serde_json::from_str::<PendingCommitRecord>(&raw)
                        .map_err(|error| error.to_string())
                })
                .and_then(|record| {
                    record.validate()?;
                    Ok(record)
                }) {
                Ok(record) => {
                    if self.migrate_legacy_path(&path, &record) {
                        continue;
                    }
                    if record.base_url == base_url {
                        records.push(record);
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %error,
                        "skipping corrupt pending-commit journal entry"
                    );
                    let mut corrupt = path.clone();
                    corrupt.set_extension("corrupt");
                    if let Err(rename_error) = fs::rename(&path, &corrupt) {
                        tracing::warn!(
                            path = %path.display(),
                            error = %rename_error,
                            "failed to quarantine corrupt pending-commit journal entry"
                        );
                    }
                }
            }
        }
        records
    }

    fn path_for(&self, reservation_id: &str) -> PathBuf {
        let digest = Sha256::digest(reservation_id.as_bytes());
        self.directory
            .join(format!("v2-{digest:x}{JOURNAL_SUFFIX}"))
    }

    fn legacy_path_for(&self, reservation_id: &str) -> PathBuf {
        let sanitized = reservation_id
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                    character
                } else {
                    '_'
                }
            })
            .collect::<String>();
        self.directory.join(format!("{sanitized}{JOURNAL_SUFFIX}"))
    }

    /// Returns true when `source` duplicated an existing standard record.
    fn migrate_legacy_path(&self, source: &Path, record: &PendingCommitRecord) -> bool {
        let standard = self.path_for(&record.reservation_id);
        if source == standard {
            return false;
        }
        let mut duplicate_of_standard = false;
        let result = if standard.exists() {
            fs::read_to_string(&standard)
                .map_err(|error| error.to_string())
                .and_then(|raw| {
                    serde_json::from_str::<PendingCommitRecord>(&raw)
                        .map_err(|error| error.to_string())
                })
                .and_then(|existing| existing.validate().map(|_| existing))
                .and_then(|existing| {
                    if existing.reservation_id == record.reservation_id {
                        remove_if_exists(source)?;
                        duplicate_of_standard = true;
                        Ok(())
                    } else {
                        Err("standard filename contains a different reservation".to_string())
                    }
                })
        } else {
            fs::rename(source, &standard).map_err(|error| error.to_string())
        };
        if let Err(error) = result {
            tracing::warn!(
                reservation_id = %record.reservation_id,
                path = %source.display(),
                error = %error,
                "could not safely migrate legacy journal filename"
            );
        }
        duplicate_of_standard
    }
}

fn remove_if_exists(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

pub(crate) fn auth_fingerprint(base_url: &str, api_key: &str, tenant: Option<&str>) -> String {
    let principal = tenant.filter(|value| !value.trim().is_empty()).map_or_else(
        || format!("key\n{api_key}"),
        |value| format!("tenant\n{value}"),
    );
    let cache_key = format!("{base_url}\n{principal}");
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(value) = cache
        .lock()
        .expect("fingerprint cache poisoned")
        .get(&cache_key)
    {
        return value.clone();
    }

    let mut digest = [0_u8; 32];
    pbkdf2_hmac::<Sha256>(
        principal.as_bytes(),
        format!("runcycles-commit-journal\n{base_url}").as_bytes(),
        30_000,
        &mut digest,
    );
    let fingerprint = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut values = cache.lock().expect("fingerprint cache poisoned");
    if values.len() >= 256 {
        values.clear();
    }
    values.insert(cache_key, fingerprint.clone());
    fingerprint
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn default_journal_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".runcycles").join("commit-journal"))
}

fn warn_journal_io(operation: &'static str, path: &Path, error: &std::io::Error) {
    tracing::warn!("commit journal {operation} failed at {path:?}: {error}");
}

#[cfg(unix)]
fn restrict_directory_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o700)) {
        tracing::warn!(path = %path.display(), error = %error, "failed to restrict journal directory permissions");
    }
}

#[cfg(not(unix))]
fn restrict_directory_permissions(_path: &Path) {}

#[cfg(unix)]
fn restrict_file_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        tracing::warn!(path = %path.display(), error = %error, "failed to restrict journal file permissions");
    }
}

#[cfg(not(unix))]
fn restrict_file_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Action, Amount, EventCreateRequest, IdempotencyKey, Subject};

    fn pending(reservation_id: &str) -> PendingCommitRecord {
        let key = IdempotencyKey::new("idem-1");
        let commit = CommitRequest::builder()
            .idempotency_key(key.clone())
            .actual(Amount::tokens(7))
            .build();
        let event = EventCreateRequest::builder()
            .idempotency_key(key)
            .subject(Subject {
                tenant: Some("acme".to_string()),
                ..Subject::default()
            })
            .action(Action::new("llm.completion", "test"))
            .actual(Amount::tokens(7))
            .build();
        PendingCommitRecord::commit(
            reservation_id.to_string(),
            "http://localhost".to_string(),
            commit,
            event,
        )
    }

    #[test]
    fn fingerprints_match_the_python_typescript_and_java_contract() {
        assert_eq!(
            auth_fingerprint("http://localhost", "test-key", None),
            "68c905017df7dbfc"
        );
        assert_eq!(
            auth_fingerprint("http://localhost", "any-key", Some("acme")),
            "8baba538fb970da4"
        );
        assert_eq!(
            auth_fingerprint("http://localhost", "rotated-key", Some("acme")),
            "8baba538fb970da4"
        );
    }

    #[test]
    fn record_load_and_discard_use_the_shared_wire_shape() {
        let temp = tempfile::tempdir().unwrap();
        let journal = CommitJournal {
            directory: temp.path().join("journal"),
        };
        let record = pending("rsv/a");
        journal.record(&record).unwrap();

        let path = journal.path_for("rsv/a");
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["reservation_id"], "rsv/a");
        assert_eq!(value["mode"], "commit");
        assert_eq!(value["commit_body"]["actual"]["amount"], 7);
        assert_eq!(value["event_fallback_body"]["subject"]["tenant"], "acme");

        let loaded = journal.load_pending("http://localhost");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].reservation_id, "rsv/a");
        journal.discard("rsv/a").unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn digest_names_do_not_collide_and_legacy_discard_checks_record_identity() {
        let temp = tempfile::tempdir().unwrap();
        let journal = CommitJournal {
            directory: temp.path().join("journal"),
        };
        journal.record(&pending("rsv/a")).unwrap();
        journal.record(&pending("rsv_a")).unwrap();
        assert_ne!(journal.path_for("rsv/a"), journal.path_for("rsv_a"));
        assert!(journal.path_for("rsv/a").exists());
        assert!(journal.path_for("rsv_a").exists());

        let legacy = journal.legacy_path_for("rsv/a");
        fs::write(&legacy, serde_json::to_string(&pending("rsv/a")).unwrap()).unwrap();
        journal.discard("rsv_a").unwrap();
        assert!(legacy.exists());
        let loaded = journal.load_pending("http://localhost");
        assert!(!legacy.exists());
        assert!(journal.path_for("rsv/a").exists());
        let ids = loaded
            .iter()
            .map(|record| record.reservation_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["rsv/a"]);
    }

    #[test]
    #[tracing_test::traced_test]
    fn corrupt_and_unsupported_records_are_quarantined_without_blocking_valid_records() {
        let temp = tempfile::tempdir().unwrap();
        let journal = CommitJournal {
            directory: temp.path().join("journal"),
        };
        fs::create_dir_all(&journal.directory).unwrap();
        fs::write(journal.directory.join("bad.json"), "{not-json").unwrap();
        let mut future = pending("future");
        future.version = 2;
        fs::write(
            journal.directory.join("future.json"),
            serde_json::to_string(&future).unwrap(),
        )
        .unwrap();
        fs::write(journal.directory.join("array.json"), "[]").unwrap();
        journal.record(&pending("good")).unwrap();

        let loaded = journal.load_pending("http://localhost");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].reservation_id, "good");
        assert!(journal.directory.join("bad.corrupt").exists());
        assert!(journal.directory.join("future.corrupt").exists());
        assert!(journal.directory.join("array.corrupt").exists());
        assert!(logs_contain(
            "skipping corrupt pending-commit journal entry"
        ));
        assert!(logs_contain("future.json"));
    }

    #[test]
    fn legacy_missing_mode_defaults_to_commit() {
        let mut value = serde_json::to_value(pending("legacy")).unwrap();
        value.as_object_mut().unwrap().remove("mode");
        let record: PendingCommitRecord = serde_json::from_value(value).unwrap();
        assert_eq!(record.mode, JournalMode::Commit);
        assert!(record.validate().is_ok());
    }

    #[test]
    fn semantic_validation_rejects_every_invalid_record_shape() {
        let mut wrong_version = pending("rsv");
        wrong_version.version = 2;
        assert!(wrong_version.validate().unwrap_err().contains("version"));

        let mut missing_id = pending("rsv");
        missing_id.reservation_id.clear();
        assert!(missing_id
            .validate()
            .unwrap_err()
            .contains("reservation_id"));

        let mut missing_commit = pending("rsv");
        missing_commit.commit_body = None;
        assert!(missing_commit
            .validate()
            .unwrap_err()
            .contains("commit_body"));

        let mut missing_event = pending("rsv");
        missing_event.mode = JournalMode::Event;
        missing_event.event_fallback_body = None;
        assert!(missing_event
            .validate()
            .unwrap_err()
            .contains("event_fallback_body"));
    }

    #[test]
    fn wrong_server_records_are_ignored_and_missing_discard_is_safe() {
        let temp = tempfile::tempdir().unwrap();
        let journal = CommitJournal {
            directory: temp.path().join("journal"),
        };
        journal.record(&pending("rsv")).unwrap();
        assert!(journal.load_pending("http://other").is_empty());
        journal.discard("missing").unwrap();
    }

    #[test]
    fn corrupt_quarantine_failure_is_best_effort() {
        let temp = tempfile::tempdir().unwrap();
        let journal = CommitJournal {
            directory: temp.path().join("journal"),
        };
        fs::create_dir_all(&journal.directory).unwrap();
        fs::write(journal.directory.join("bad.json"), "{not-json").unwrap();
        fs::create_dir(journal.directory.join("bad.corrupt")).unwrap();
        assert!(journal.load_pending("http://localhost").is_empty());
        assert!(journal.directory.join("bad.json").exists());
    }

    #[test]
    fn unreadable_journal_path_is_reported_and_treated_as_empty() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("not-a-directory");
        fs::write(&path, "file").unwrap();
        let journal = CommitJournal { directory: path };
        assert!(journal.load_pending("http://localhost").is_empty());
    }
}
