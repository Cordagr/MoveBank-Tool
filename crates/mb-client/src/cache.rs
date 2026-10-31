use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const CACHE_TTL_SECONDS: u64 = 24 * 60 * 60;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("cache I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid cache entry: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("system clock is before the Unix epoch")]
    Clock,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    key: String,
    expiry: u64,
    data: String,
}

pub struct Cache {
    cache_dir: PathBuf,
}

impl Cache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: dir.into(),
        }
    }

    pub fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        let path = self.entry_path(key);
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };

        let entry: CacheEntry = serde_json::from_str(&contents)?;
        if entry.key != key {
            return Ok(None);
        }

        if entry.expiry <= unix_now()? {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            return Ok(None);
        }

        Ok(Some(entry.data))
    }

    pub fn store(&self, key: &str, data: &str) -> Result<(), CacheError> {
        fs::create_dir_all(&self.cache_dir)?;
        let entry = CacheEntry {
            key: key.to_owned(),
            expiry: unix_now()?.saturating_add(CACHE_TTL_SECONDS),
            data: data.to_owned(),
        };
        let contents = serde_json::to_vec(&entry)?;
        fs::write(self.entry_path(key), contents)?;
        Ok(())
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        let filename = format!("{:x}.json", Md5::digest(key.as_bytes()));
        self.cache_dir.join(filename)
    }
}

fn unix_now() -> Result<u64, CacheError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| CacheError::Clock)
}

#[cfg(test)]
mod tests {
    use super::Cache;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_cache_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("mb-client-cache-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn cache_miss_then_hit() {
        let dir = test_cache_dir();
        let cache = Cache::new(&dir);

        assert_eq!(cache.get("study-list:user-a").unwrap(), None);
        cache
            .store("study-list:user-a", "id,name\n1,Test study\n")
            .unwrap();
        assert_eq!(
            cache.get("study-list:user-a").unwrap().as_deref(),
            Some("id,name\n1,Test study\n")
        );
        assert_eq!(cache.get("study-list:user-b").unwrap(), None);

        fs::remove_dir_all(dir).unwrap();
    }
}
