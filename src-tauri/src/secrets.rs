//! 平台差异化的凭据存储。
//!
//! 桌面端沿用 Windows 凭据管理器（`keyring`）；Android/iOS 没有等价实现，
//! 改用 App 私有目录下的 `secrets.json`。该目录由系统沙箱隔离，其他应用无法读取，
//! 且写入时显式收紧为 0600。两种实现的对外行为一致：空值表示删除。

use std::path::Path;
use std::sync::OnceLock;

#[cfg(not(desktop))]
use std::collections::BTreeMap;
#[cfg(not(desktop))]
use std::path::PathBuf;

#[cfg(desktop)]
const KEYRING_SERVICE: &str = "com.oshical.app";

pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&self, key: &str, value: Option<String>) -> Result<(), String>;
}

static STORE: OnceLock<Box<dyn SecretStore>> = OnceLock::new();

/// 在 `setup` 中调用一次。重复调用被忽略，便于测试与热路径复用。
pub fn init(data_dir: &Path) {
    if STORE.get().is_some() {
        return;
    }
    let _ = STORE.set(build(data_dir));
}

fn build(data_dir: &Path) -> Box<dyn SecretStore> {
    #[cfg(desktop)]
    {
        let _ = data_dir;
        Box::new(KeyringSecretStore)
    }
    #[cfg(not(desktop))]
    {
        Box::new(FileSecretStore::new(data_dir.join("secrets.json")))
    }
}

fn store() -> Option<&'static dyn SecretStore> {
    STORE.get().map(|store| store.as_ref())
}

pub fn get_secret(key: &str) -> Option<String> {
    store()?.get(key).filter(|value| !value.is_empty())
}

pub fn set_secret(key: &str, value: Option<String>) -> Result<(), String> {
    match store() {
        Some(store) => store.set(key, value),
        None => Err("凭据存储尚未初始化".to_owned()),
    }
}

fn normalize(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(desktop)]
struct KeyringSecretStore;

#[cfg(desktop)]
impl SecretStore for KeyringSecretStore {
    fn get(&self, key: &str) -> Option<String> {
        keyring::Entry::new(KEYRING_SERVICE, key)
            .ok()?
            .get_password()
            .ok()
            .filter(|value| !value.is_empty())
    }

    fn set(&self, key: &str, value: Option<String>) -> Result<(), String> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, key).map_err(|error| error.to_string())?;
        match normalize(value) {
            Some(value) => entry
                .set_password(&value)
                .map_err(|error| error.to_string()),
            None => match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(error.to_string()),
            },
        }
    }
}

#[cfg(not(desktop))]
struct FileSecretStore {
    path: PathBuf,
}

#[cfg(not(desktop))]
impl FileSecretStore {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn load(&self) -> BTreeMap<String, String> {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self, values: &BTreeMap<String, String>) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let payload = serde_json::to_vec(values).map_err(|error| error.to_string())?;
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, &payload).map_err(|error| error.to_string())?;
        restrict_permissions(&temporary)?;
        std::fs::rename(&temporary, &self.path).map_err(|error| error.to_string())
    }
}

#[cfg(not(desktop))]
impl SecretStore for FileSecretStore {
    fn get(&self, key: &str) -> Option<String> {
        self.load()
            .get(key)
            .cloned()
            .filter(|value| !value.is_empty())
    }

    fn set(&self, key: &str, value: Option<String>) -> Result<(), String> {
        let mut values = self.load();
        match normalize(value) {
            Some(value) => {
                values.insert(key.to_owned(), value);
            }
            None => {
                values.remove(key);
            }
        }
        self.save(&values)
    }
}

#[cfg(not(desktop))]
fn restrict_permissions(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| error.to_string())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(all(test, not(desktop)))]
mod tests {
    use super::*;

    fn temporary_store(name: &str) -> FileSecretStore {
        let path = std::env::temp_dir().join(format!("oshical-secret-{name}.json"));
        let _ = std::fs::remove_file(&path);
        FileSecretStore::new(path)
    }

    #[test]
    fn file_store_round_trip_and_delete() {
        let store = temporary_store("round-trip");
        assert_eq!(store.get("token"), None);
        store.set("token", Some("  abc  ".to_owned())).unwrap();
        assert_eq!(store.get("token").as_deref(), Some("abc"));
        store.set("token", Some("   ".to_owned())).unwrap();
        assert_eq!(store.get("token"), None);
    }

    #[test]
    fn file_store_keeps_other_keys() {
        let store = temporary_store("keep");
        store.set("a", Some("1".to_owned())).unwrap();
        store.set("b", Some("2".to_owned())).unwrap();
        store.set("a", None).unwrap();
        assert_eq!(store.get("a"), None);
        assert_eq!(store.get("b").as_deref(), Some("2"));
    }
}
