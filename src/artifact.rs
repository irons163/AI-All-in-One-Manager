//! An artifact is one piece of a provider's login state: a whole file, a few
//! keys inside a shared JSON file, or an item in the OS keychain.

use crate::ctx::Ctx;
use crate::util::{atomic_write, json_get, json_remove, json_set};
use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum Kind {
    /// The whole file belongs to the login (e.g. `~/.codex/auth.json`).
    File { path: PathBuf },
    /// Only some keys of a JSON file belong to the login; the rest of the
    /// file is left alone (e.g. `oauthAccount` inside `~/.claude.json`).
    /// Keys are dotted paths. The snapshot is a JSON object of key -> value.
    JsonKeys { path: PathBuf, keys: Vec<String> },
    /// A generic-password item in the OS keychain.
    Keychain { service: String, account: String },
}

#[derive(Debug, Clone)]
pub struct Artifact {
    /// Stable name of this artifact inside a snapshot directory.
    pub id: String,
    pub kind: Kind,
    /// A provider counts as logged in when any primary artifact is present.
    pub primary: bool,
}

impl Artifact {
    pub fn file(id: &str, path: PathBuf, primary: bool) -> Self {
        Artifact {
            id: id.to_string(),
            kind: Kind::File { path },
            primary,
        }
    }

    pub fn json_keys(id: &str, path: PathBuf, keys: &[&str], primary: bool) -> Self {
        Artifact {
            id: id.to_string(),
            kind: Kind::JsonKeys {
                path,
                keys: keys.iter().map(|k| k.to_string()).collect(),
            },
            primary,
        }
    }

    pub fn keychain(id: &str, service: &str, account: &str, primary: bool) -> Self {
        Artifact {
            id: id.to_string(),
            kind: Kind::Keychain {
                service: service.to_string(),
                account: account.to_string(),
            },
            primary,
        }
    }

    pub fn describe(&self) -> String {
        match &self.kind {
            Kind::File { path } => path.display().to_string(),
            Kind::JsonKeys { path, keys } => format!("{} [{}]", path.display(), keys.join(", ")),
            Kind::Keychain { service, account } => format!("keychain: {service} ({account})"),
        }
    }

    /// Cheap presence check that never reads a keychain secret.
    pub fn exists_live(&self, ctx: &Ctx) -> bool {
        match &self.kind {
            Kind::File { path } => path.is_file(),
            Kind::JsonKeys { .. } => matches!(self.read_live(ctx), Ok(Some(_))),
            Kind::Keychain { service, account } => {
                ctx.secrets.exists(service, account).unwrap_or(false)
            }
        }
    }

    /// The artifact's current content, or `None` if it is absent.
    pub fn read_live(&self, ctx: &Ctx) -> Result<Option<Vec<u8>>> {
        match &self.kind {
            Kind::File { path } => {
                if !path.is_file() {
                    return Ok(None);
                }
                fs::read(path)
                    .map(Some)
                    .with_context(|| format!("reading {}", path.display()))
            }
            Kind::JsonKeys { path, keys } => {
                let Some((root, _)) = load_json(path)? else {
                    return Ok(None);
                };
                let mut snap = Map::new();
                for key in keys {
                    if let Some(value) = json_get(&root, key) {
                        snap.insert(key.clone(), value.clone());
                    }
                }
                if snap.is_empty() {
                    return Ok(None);
                }
                Ok(Some(serde_json::to_vec_pretty(&Value::Object(snap))?))
            }
            Kind::Keychain { service, account } => Ok(ctx
                .secrets
                .get(service, account)?
                .map(String::into_bytes)),
        }
    }

    /// Replace the artifact's content, or remove it when `data` is `None`.
    pub fn write_live(&self, ctx: &Ctx, data: Option<&[u8]>) -> Result<()> {
        match &self.kind {
            Kind::File { path } => match data {
                Some(bytes) => atomic_write(path, bytes),
                None => {
                    if path.is_file() {
                        fs::remove_file(path)
                            .with_context(|| format!("removing {}", path.display()))?;
                    }
                    Ok(())
                }
            },
            Kind::JsonKeys { path, keys } => {
                let snap: Map<String, Value> = match data {
                    Some(bytes) => serde_json::from_slice(bytes)
                        .with_context(|| format!("snapshot '{}' is not a JSON object", self.id))?,
                    None => Map::new(),
                };
                let (mut root, style) = match load_json(path)? {
                    Some(loaded) => loaded,
                    None if path.exists() => return Ok(()), // unparseable: leave it alone
                    None if snap.is_empty() => return Ok(()), // nothing to add, nothing to remove
                    None => (Value::Object(Map::new()), Style::default()),
                };
                let mut changed = false;
                for key in keys {
                    match snap.get(key) {
                        Some(value) => {
                            if json_get(&root, key) != Some(value) {
                                json_set(&mut root, key, value.clone());
                                changed = true;
                            }
                        }
                        None => changed |= json_remove(&mut root, key),
                    }
                }
                if !changed {
                    return Ok(());
                }
                atomic_write(path, style.render(&root)?.as_bytes())
            }
            Kind::Keychain { service, account } => match data {
                Some(bytes) => {
                    let secret = std::str::from_utf8(bytes)
                        .with_context(|| format!("snapshot '{}' is not valid UTF-8", self.id))?;
                    ctx.secrets.set(service, account, secret)
                }
                None => ctx.secrets.delete(service, account).map(|_| ()),
            },
        }
    }
}

/// How a JSON file was formatted on disk, so it can be written back the same way.
struct Style {
    pretty: bool,
    trailing_newline: bool,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            pretty: true,
            trailing_newline: false,
        }
    }
}

impl Style {
    fn render(&self, value: &Value) -> Result<String> {
        let mut out = if self.pretty {
            serde_json::to_string_pretty(value)?
        } else {
            serde_json::to_string(value)?
        };
        if self.trailing_newline {
            out.push('\n');
        }
        Ok(out)
    }
}

/// Load a JSON file. `None` means it is missing, or present but not plain
/// JSON (some tools allow comments in their settings); callers then leave the
/// file untouched instead of risking it.
fn load_json(path: &PathBuf) -> Result<Option<(Value, Style)>> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    match serde_json::from_str::<Value>(&text) {
        Ok(value) => {
            let style = Style {
                pretty: text.trim_end().contains('\n'),
                trailing_newline: text.ends_with('\n'),
            };
            Ok(Some((value, style)))
        }
        Err(err) => {
            eprintln!(
                "warning: {} is not plain JSON ({err}); leaving it untouched",
                path.display()
            );
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_keys_only_touch_their_keys() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_test(dir.path(), false);
        let path = dir.path().join("config.json");
        fs::write(
            &path,
            "{\n  \"first\": 1,\n  \"oauthAccount\": {\n    \"emailAddress\": \"a@x.io\"\n  },\n  \"last\": [1, 2]\n}\n",
        )
        .unwrap();
        let art = Artifact::json_keys("cfg", path.clone(), &["oauthAccount", "absent.key"], false);

        let snap = art.read_live(&ctx).unwrap().expect("key present");
        let parsed: Value = serde_json::from_slice(&snap).unwrap();
        assert_eq!(parsed, json!({"oauthAccount": {"emailAddress": "a@x.io"}}));

        let other = serde_json::to_vec(&json!({"oauthAccount": {"emailAddress": "b@x.io"}})).unwrap();
        art.write_live(&ctx, Some(&other)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let root: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(root["oauthAccount"]["emailAddress"], "b@x.io");
        assert_eq!(root["first"], 1);
        let keys: Vec<&String> = root.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["first", "oauthAccount", "last"]);
        assert!(text.ends_with("}\n"), "keeps trailing newline");

        art.write_live(&ctx, None).unwrap();
        let root: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(root.get("oauthAccount").is_none());
        assert_eq!(root["last"], json!([1, 2]));
        assert!(art.read_live(&ctx).unwrap().is_none());
    }

    #[test]
    fn json_keys_leave_unparseable_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_test(dir.path(), false);
        let path = dir.path().join("settings.json");
        let original = "{ // comment\n \"a\": 1 }";
        fs::write(&path, original).unwrap();
        let art = Artifact::json_keys("s", path.clone(), &["a"], false);
        assert!(art.read_live(&ctx).unwrap().is_none());
        art.write_live(&ctx, Some(b"{\"a\": 2}")).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn keychain_artifact_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_test(dir.path(), true);
        let art = Artifact::keychain("kc", "Some Service", "tester", true);
        assert!(!art.exists_live(&ctx));
        art.write_live(&ctx, Some(b"{\"token\":1}")).unwrap();
        assert!(art.exists_live(&ctx));
        assert_eq!(art.read_live(&ctx).unwrap().unwrap(), b"{\"token\":1}");
        art.write_live(&ctx, None).unwrap();
        assert!(art.read_live(&ctx).unwrap().is_none());
    }
}
