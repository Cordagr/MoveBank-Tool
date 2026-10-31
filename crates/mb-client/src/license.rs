use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LicenseAcceptance {
    study_id: i64,
    auth_scope: String,
    accepted_at: DateTime<Utc>,
    terms_md5: String,
}

#[derive(Debug)]
pub(crate) struct LicenseStore {
    path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum LicenseStoreError {
    #[error("license store I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid license store JSON: {0}")]
    Json(#[from] serde_json::Error),
}

impl LicenseStore {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn check_acceptance_status(
        &self,
        study_id: i64,
        auth_scope: &str,
        terms_md5: &str,
    ) -> Result<bool, LicenseStoreError> {
        let licenses = self.load()?;
        Ok(licenses.iter().any(|license| {
            license.study_id == study_id
                && license.auth_scope == auth_scope
                && license.terms_md5 == terms_md5
        }))
    }

    pub(crate) fn record_acceptance(
        &self,
        study_id: i64,
        auth_scope: &str,
        terms_md5: &str,
    ) -> Result<(), LicenseStoreError> {
        let mut licenses = self.load()?;
        if let Some(license) = licenses.iter_mut().find(|license| {
            license.study_id == study_id
                && license.auth_scope == auth_scope
                && license.terms_md5 == terms_md5
        }) {
            license.accepted_at = Utc::now();
        } else {
            licenses.push(LicenseAcceptance {
                study_id,
                auth_scope: auth_scope.to_owned(),
                accepted_at: Utc::now(),
                terms_md5: terms_md5.to_owned(),
            });
        }

        if let Some(parent) = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.path, serde_json::to_vec_pretty(&licenses)?)?;
        Ok(())
    }

    fn load(&self) -> Result<Vec<LicenseAcceptance>, LicenseStoreError> {
        let contents = match fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        Ok(serde_json::from_str(&contents)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn license_store_path() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        std::env::temp_dir()
            .join(format!(
                "mb-client-license-test-{}-{nonce}",
                std::process::id()
            ))
            .join("licenses.json")
    }

    #[test]
    fn acceptance_persists_and_is_scoped_to_study_user_and_terms() {
        let path = license_store_path();
        let store = LicenseStore::new(&path);

        assert!(!store
            .check_acceptance_status(42, "user-a", "terms-hash")
            .expect("missing store should be treated as no acceptance"));
        store
            .record_acceptance(42, "user-a", "terms-hash")
            .expect("acceptance should be saved");

        let reopened_store = LicenseStore::new(&path);
        assert!(reopened_store
            .check_acceptance_status(42, "user-a", "terms-hash")
            .expect("saved acceptance should be readable"));
        assert!(!reopened_store
            .check_acceptance_status(43, "user-a", "terms-hash")
            .expect("different study should not match"));
        assert!(!reopened_store
            .check_acceptance_status(42, "user-b", "terms-hash")
            .expect("different user should not match"));
        assert!(!reopened_store
            .check_acceptance_status(42, "user-a", "changed-terms-hash")
            .expect("changed terms should not match"));

        fs::remove_dir_all(path.parent().expect("license file should have a parent"))
            .expect("test license directory should be removable");
    }

    #[test]
    fn malformed_store_returns_a_json_error() {
        let path = license_store_path();
        let parent = path.parent().expect("license file should have a parent");
        fs::create_dir_all(parent).expect("test license directory should be creatable");
        fs::write(&path, "not valid json").expect("test license file should be writable");

        let result = LicenseStore::new(&path).check_acceptance_status(42, "user-a", "hash");
        assert!(matches!(result, Err(LicenseStoreError::Json(_))));

        fs::remove_dir_all(parent).expect("test license directory should be removable");
    }
}
