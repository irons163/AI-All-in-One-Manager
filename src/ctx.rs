//! Everything aiom reads from its surroundings (home directory, environment
//! variables, the OS secret store) goes through `Ctx`, so tests can point the
//! whole program at a throwaway directory.

use crate::secrets::{MacKeychain, MemoryStore, NoStore, SecretStore};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct Ctx {
    pub home: PathBuf,
    /// Where aiom keeps accounts, state and backups (`~/.aiom` or `$AIOM_HOME`).
    pub data_dir: PathBuf,
    /// OS user name; the account field of Claude Code's keychain item.
    pub user: String,
    /// Whether providers that use the OS keychain should be read from it.
    pub keychain: bool,
    pub secrets: Box<dyn SecretStore>,
    vars: HashMap<String, String>,
}

impl Ctx {
    pub fn from_env() -> Result<Self> {
        let vars: HashMap<String, String> = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect();
        let home = vars
            .get("HOME")
            .or_else(|| vars.get("USERPROFILE"))
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .context("cannot determine the home directory (HOME is not set)")?;
        let data_dir = match vars.get("AIOM_HOME").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => home.join(".aiom"),
        };
        let keychain = cfg!(target_os = "macos") && !vars.contains_key("AIOM_NO_KEYCHAIN");
        let user = vars
            .get("USER")
            .or_else(|| vars.get("USERNAME"))
            .cloned()
            .unwrap_or_default();
        let secrets: Box<dyn SecretStore> = if keychain {
            Box::new(MacKeychain)
        } else {
            Box::new(NoStore)
        };
        Ok(Ctx {
            home,
            data_dir,
            user,
            keychain,
            secrets,
            vars,
        })
    }

    /// A context rooted in `home` with an in-memory secret store and no
    /// inherited environment. Never touches the real home or keychain.
    pub fn for_test(home: &Path, keychain: bool) -> Self {
        Ctx {
            home: home.to_path_buf(),
            data_dir: home.join(".aiom"),
            user: "tester".to_string(),
            keychain,
            secrets: Box::new(MemoryStore::default()),
            vars: HashMap::new(),
        }
    }

    pub fn set_var(&mut self, key: &str, value: &str) {
        self.vars.insert(key.to_string(), value.to_string());
    }

    /// An environment variable, treating empty as unset.
    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    /// The directory named by `var` if set, otherwise `default`.
    pub fn dir_from(&self, var: &str, default: PathBuf) -> PathBuf {
        self.var(var).map(PathBuf::from).unwrap_or(default)
    }

    /// Shorten the home directory to `~` in text shown to the user.
    pub fn tilde(&self, text: &str) -> String {
        let home = self.home.to_string_lossy();
        if home.len() > 1 {
            text.replace(home.as_ref(), "~")
        } else {
            text.to_string()
        }
    }

    /// Expand a leading `~` or `$HOME` in a user-supplied path.
    pub fn expand(&self, raw: &str) -> PathBuf {
        if raw == "~" || raw == "$HOME" {
            return self.home.clone();
        }
        for prefix in ["~/", "$HOME/", "${HOME}/"] {
            if let Some(rest) = raw.strip_prefix(prefix) {
                return self.home.join(rest);
            }
        }
        PathBuf::from(raw)
    }
}
