//! Provider definitions: which artifacts make up a login for each AI CLI,
//! how to tell which account is logged in, and how to start a fresh login.
//!
//! Built-in: Claude Code, the Claude desktop app, Codex, Gemini CLI, Grok,
//! OpenCode. More can be
//! declared (or a built-in overridden) in `<data dir>/providers.toml`.

use crate::artifact::Artifact;
use crate::chromium;
use crate::ctx::Ctx;
use crate::util::{json_get, jwt_claim};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Provider {
    pub id: String,
    pub name: String,
    /// Executable looked up on PATH to report whether the CLI is installed.
    pub binary: Option<String>,
    /// Command that starts the CLI's own login flow.
    pub login: Option<Vec<String>>,
    /// Shell command that installs the CLI, shown when it is missing.
    pub install: Option<String>,
    /// Set for a desktop app whose login lives in its own files: the app is
    /// quit before those files are replaced and started again afterwards.
    pub app: Option<AppInfo>,
    pub artifacts: Vec<Artifact>,
    /// Tried in order; the first that yields a value names the live account.
    /// Used to tell accounts apart, so it must be stable.
    pub identity: Vec<IdentitySource>,
    /// Tried in order for a friendlier name to show (an email) when the
    /// identity is an opaque id. Display only: it may be unavailable at
    /// times, so it is never used to tell accounts apart.
    pub label: Vec<IdentitySource>,
    pub builtin: bool,
}

#[derive(Debug, Clone)]
pub struct AppInfo {
    /// macOS bundle identifier, used to find, quit and start the app.
    pub bundle_id: String,
    /// Name to show, e.g. "Claude".
    pub name: String,
    /// Where the app may be installed.
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub enum IdentitySource {
    /// A string at a dotted key in a JSON file.
    JsonString { path: PathBuf, key: String },
    /// A claim inside a JWT stored at a dotted key in a JSON file.
    JwtClaim {
        path: PathBuf,
        key: String,
        claim: String,
    },
    /// A fixed label when a dotted key holds a non-empty string (e.g. an API key).
    Present {
        path: PathBuf,
        key: String,
        label: String,
    },
    /// The email that a Chromium IndexedDB cache stores next to an account
    /// id, as the claude.ai web app does for its account object. The id is
    /// read from `id_key` in the JSON file `id_path`.
    CachedEmail {
        dir: PathBuf,
        id_path: PathBuf,
        id_key: String,
    },
}

impl IdentitySource {
    fn resolve(&self) -> Option<String> {
        let (path, key) = match self {
            IdentitySource::JsonString { path, key }
            | IdentitySource::JwtClaim { path, key, .. }
            | IdentitySource::Present { path, key, .. } => (path, key),
            IdentitySource::CachedEmail {
                dir,
                id_path,
                id_key,
            } => {
                let root: Value =
                    serde_json::from_str(&fs::read_to_string(id_path).ok()?).ok()?;
                let id = json_get(&root, id_key)?.as_str()?.trim();
                return cached_email(dir, id);
            }
        };
        let root: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
        let value = json_get(&root, key)?.as_str()?.trim();
        if value.is_empty() {
            return None;
        }
        match self {
            IdentitySource::JsonString { .. } => Some(value.to_string()),
            IdentitySource::JwtClaim { claim, .. } => jwt_claim(value, claim),
            IdentitySource::Present { label, .. } => Some(label.clone()),
            IdentitySource::CachedEmail { .. } => unreachable!("returned above"),
        }
    }
}

/// Read a LEB128 varint, as V8 uses for string lengths.
fn read_varint(bytes: &[u8], at: &mut usize) -> Option<usize> {
    let mut value: usize = 0;
    for shift in (0..35).step_by(7) {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= usize::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// Read a V8 serialised one-byte string (tag `"`, varint length, bytes).
fn read_v8_string<'a>(bytes: &'a [u8], at: &mut usize) -> Option<&'a [u8]> {
    if *bytes.get(*at)? != b'"' {
        return None;
    }
    *at += 1;
    let len = read_varint(bytes, at)?;
    let end = at.checked_add(len)?;
    let text = bytes.get(*at..end)?;
    *at = end;
    Some(text)
}

/// Look for `uuid: <id>, email_address: <email>` in a Chromium IndexedDB
/// folder, as the claude.ai web app caches its account object. Values there
/// are V8-serialised and may be Snappy-compressed, inside LevelDB logs or
/// tables, or moved into blob files; `chromium::readable_parts` undoes all
/// that. Returns `None` when no such record can be read, and the caller then
/// shows the account id instead.
pub(crate) fn cached_email(dir: &Path, id: &str) -> Option<String> {
    let needle = email_needle(id)?;
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in fs::read_dir(&next).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    // Newest data first: LevelDB log files carry the latest writes, and
    // higher file numbers are newer.
    files.sort_by_key(|p| {
        let is_log = p.extension().is_some_and(|e| e == "log");
        (std::cmp::Reverse(is_log), std::cmp::Reverse(p.clone()))
    });
    files
        .iter()
        .flat_map(|file| chromium::readable_parts(file))
        .find_map(|part| email_after(&part, &needle))
}

/// The V8 one-byte string encoding of an account id, as it appears just
/// before `email_address` in the cached account object.
fn email_needle(id: &str) -> Option<Vec<u8>> {
    if id.is_empty() || id.len() > 127 {
        return None;
    }
    let mut needle = vec![b'"', id.len() as u8];
    needle.extend_from_slice(id.as_bytes());
    Some(needle)
}

/// The plausible email in `email_address: "..."` right after `needle`.
fn email_after(bytes: &[u8], needle: &[u8]) -> Option<String> {
    let mut from = 0;
    while let Some(pos) = chromium::find_bytes(&bytes[from..], needle) {
        let mut at = from + pos + needle.len();
        from += pos + 1;
        if read_v8_string(bytes, &mut at) != Some(b"email_address".as_slice()) {
            continue;
        }
        let Some(email) = read_v8_string(bytes, &mut at) else { continue };
        let Ok(email) = std::str::from_utf8(email) else { continue };
        let plausible = email.len() <= 254
            && email.contains('@')
            && !email.starts_with('@')
            && !email.ends_with('@')
            && email.bytes().all(|b| b.is_ascii_graphic());
        if plausible {
            return Some(email.to_string());
        }
    }
    None
}

impl Provider {
    /// Who is logged in right now, if the provider's files say.
    pub fn live_identity(&self) -> Option<String> {
        self.identity.iter().find_map(IdentitySource::resolve)
    }

    /// A friendlier name for the live account (an email), when there is one.
    pub fn live_label(&self) -> Option<String> {
        self.label.iter().find_map(IdentitySource::resolve)
    }

    /// The same label, read from a saved account instead of the live files.
    /// `snapshot` returns the saved content of an artifact by id; `identity`
    /// is the saved account's identity.
    pub fn saved_label(
        &self,
        identity: &str,
        snapshot: impl Fn(&str) -> Option<Vec<u8>>,
    ) -> Option<String> {
        self.label.iter().find_map(|source| {
            let IdentitySource::CachedEmail { dir, .. } = source else {
                return None;
            };
            let artifact = self.artifacts.iter().find(|a| {
                matches!(&a.kind, crate::artifact::Kind::Dir { path } if path == dir)
            })?;
            let packed = snapshot(&artifact.id)?;
            let mut files = crate::artifact::packed_files(&packed)?;
            files.sort_by_key(|(name, _)| {
                (std::cmp::Reverse(name.ends_with(".log")), std::cmp::Reverse(name.clone()))
            });
            let needle = email_needle(identity)?;
            files
                .into_iter()
                .flat_map(|(name, data)| chromium::readable_parts_of(&name, data.to_vec()))
                .find_map(|part| email_after(&part, &needle))
        })
    }

    /// Where the provider's CLI or app is installed, if it is.
    pub fn installed_path(&self, ctx: &Ctx) -> Option<PathBuf> {
        if let Some(found) = self
            .binary
            .as_deref()
            .and_then(|b| crate::util::find_in_path(b, ctx.var("PATH")))
        {
            return Some(found);
        }
        self.app
            .as_ref()
            .and_then(|app| app.paths.iter().find(|p| p.exists()).cloned())
    }

    pub fn logged_in(&self, ctx: &Ctx) -> bool {
        self.artifacts
            .iter()
            .any(|a| a.primary && a.exists_live(ctx))
    }
}

fn json_string(path: &PathBuf, key: &str) -> IdentitySource {
    IdentitySource::JsonString {
        path: path.clone(),
        key: key.to_string(),
    }
}

fn jwt(path: &PathBuf, key: &str, claim: &str) -> IdentitySource {
    IdentitySource::JwtClaim {
        path: path.clone(),
        key: key.to_string(),
        claim: claim.to_string(),
    }
}

fn command(parts: &[&str]) -> Option<Vec<String>> {
    Some(parts.iter().map(|p| p.to_string()).collect())
}

/// Claude Code. On macOS the OAuth tokens live in the login keychain; on
/// other systems (or when the keychain is unavailable) in
/// `.credentials.json`. The account profile shown in the UI is the
/// `oauthAccount` key of `.claude.json`, which also holds unrelated state.
fn claude(ctx: &Ctx) -> Provider {
    let custom_dir = ctx.var("CLAUDE_CONFIG_DIR").map(PathBuf::from);
    let config_dir = custom_dir.clone().unwrap_or_else(|| ctx.home.join(".claude"));
    let state_file = match &custom_dir {
        Some(dir) => dir.join(".claude.json"),
        None => ctx.home.join(".claude.json"),
    };
    let mut artifacts = Vec::new();
    if ctx.keychain {
        artifacts.push(Artifact::keychain(
            "keychain-credentials",
            "Claude Code-credentials",
            &ctx.user,
            true,
        ));
    }
    artifacts.push(Artifact::file(
        "credentials.json",
        config_dir.join(".credentials.json"),
        true,
    ));
    artifacts.push(Artifact::json_keys(
        "claude.json-account",
        state_file.clone(),
        &["oauthAccount"],
        false,
    ));
    Provider {
        id: "claude".into(),
        name: "Claude Code".into(),
        binary: Some("claude".into()),
        login: command(&["claude", "auth", "login"]),
        install: Some("npm install -g @anthropic-ai/claude-code".into()),
        app: None,
        artifacts,
        identity: vec![json_string(&state_file, "oauthAccount.emailAddress")],
        label: Vec::new(),
        builtin: true,
    }
}

/// The Claude desktop app (macOS). It keeps its login in its own data
/// folder, separate from the Claude Code CLI: encrypted OAuth tokens in
/// `config.json`, the claude.ai session cookie in `Cookies`, and the web
/// app's local storage. Everything is encrypted with a key from the login
/// keychain ("Claude Safe Storage") that is the same for every account on
/// this Mac, so swapping the files swaps the account. The app must be closed
/// while that happens.
fn claude_desktop(ctx: &Ctx) -> Provider {
    let dir = ctx
        .home
        .join("Library")
        .join("Application Support")
        .join("Claude");
    let config = dir.join("config.json");
    Provider {
        id: "claude-desktop".into(),
        name: "Claude Desktop".into(),
        binary: None,
        login: None,
        install: None,
        app: Some(AppInfo {
            bundle_id: "com.anthropic.claudefordesktop".into(),
            name: "Claude".into(),
            paths: vec![
                PathBuf::from("/Applications/Claude.app"),
                ctx.home.join("Applications").join("Claude.app"),
            ],
        }),
        artifacts: vec![
            Artifact::json_keys(
                "config-oauth",
                config.clone(),
                &["oauth:tokenCacheV2", "oauth:tokenCache"],
                true,
            ),
            Artifact::json_keys("config-account", config.clone(), &["lastKnownAccountUuid"], false),
            Artifact::file("Cookies", dir.join("Cookies"), false),
            Artifact::dir("local-storage", dir.join("Local Storage"), false),
            Artifact::dir("session-storage", dir.join("Session Storage"), false),
            Artifact::dir("indexeddb", dir.join("IndexedDB"), false),
            Artifact::dir("webstorage", dir.join("WebStorage"), false),
        ],
        identity: vec![json_string(&config, "lastKnownAccountUuid")],
        label: vec![IdentitySource::CachedEmail {
            dir: dir.join("IndexedDB"),
            id_path: config.clone(),
            id_key: "lastKnownAccountUuid".into(),
        }],
        builtin: true,
    }
}

/// OpenAI Codex CLI: everything is in `$CODEX_HOME/auth.json`.
fn codex(ctx: &Ctx) -> Provider {
    let home = ctx.dir_from("CODEX_HOME", ctx.home.join(".codex"));
    let auth = home.join("auth.json");
    Provider {
        id: "codex".into(),
        name: "Codex CLI".into(),
        binary: Some("codex".into()),
        login: command(&["codex", "login"]),
        install: Some("npm install -g @openai/codex".into()),
        app: None,
        artifacts: vec![Artifact::file("auth.json", auth.clone(), true)],
        identity: vec![
            json_string(&auth, "email"),
            jwt(&auth, "tokens.id_token", "email"),
            IdentitySource::Present {
                path: auth.clone(),
                key: "OPENAI_API_KEY".into(),
                label: "API key".into(),
            },
        ],
        label: Vec::new(),
        builtin: true,
    }
}

/// Gemini CLI: OAuth tokens in `oauth_creds.json` (or the keychain on newer
/// builds), the account list in `google_accounts.json`, an optional API key
/// in `.env`, and the chosen auth method inside `settings.json`.
fn gemini(ctx: &Ctx) -> Provider {
    let base = ctx.dir_from("GEMINI_CLI_HOME", ctx.home.clone());
    let dir = base.join(".gemini");
    let accounts = dir.join("google_accounts.json");
    let mut artifacts = vec![
        Artifact::file("oauth_creds.json", dir.join("oauth_creds.json"), true),
        Artifact::file("google_accounts.json", accounts.clone(), false),
        Artifact::file("env", dir.join(".env"), true),
        Artifact::json_keys(
            "settings.json-auth",
            dir.join("settings.json"),
            &["security.auth", "selectedAuthType"],
            false,
        ),
    ];
    if ctx.keychain {
        artifacts.push(Artifact::keychain(
            "keychain-oauth",
            "gemini-cli-oauth",
            "main-account",
            true,
        ));
    }
    Provider {
        id: "gemini".into(),
        name: "Gemini CLI".into(),
        binary: Some("gemini".into()),
        login: command(&["gemini"]),
        install: Some("npm install -g @google/gemini-cli".into()),
        app: None,
        artifacts,
        identity: vec![json_string(&accounts, "active")],
        label: Vec::new(),
        builtin: true,
    }
}

/// xAI Grok CLI: `$GROK_HOME/auth.json`.
fn grok(ctx: &Ctx) -> Provider {
    let home = ctx.dir_from("GROK_HOME", ctx.home.join(".grok"));
    let auth = home.join("auth.json");
    Provider {
        id: "grok".into(),
        name: "Grok CLI".into(),
        binary: Some("grok".into()),
        login: command(&["grok", "login"]),
        install: None,
        app: None,
        artifacts: vec![Artifact::file("auth.json", auth.clone(), true)],
        identity: vec![
            json_string(&auth, "email"),
            json_string(&auth, "user.email"),
            json_string(&auth, "account.email"),
            jwt(&auth, "id_token", "email"),
            jwt(&auth, "tokens.id_token", "email"),
        ],
        label: Vec::new(),
        builtin: true,
    }
}

/// OpenCode: one `auth.json` under the XDG data directory holding the
/// credentials of every model provider it is connected to.
fn opencode(ctx: &Ctx) -> Provider {
    let data = ctx.dir_from("XDG_DATA_HOME", ctx.home.join(".local").join("share"));
    Provider {
        id: "opencode".into(),
        name: "OpenCode".into(),
        binary: Some("opencode".into()),
        login: command(&["opencode", "auth", "login"]),
        install: Some("npm install -g opencode-ai".into()),
        app: None,
        artifacts: vec![Artifact::file(
            "auth.json",
            data.join("opencode").join("auth.json"),
            true,
        )],
        identity: Vec::new(),
        label: Vec::new(),
        builtin: true,
    }
}

pub fn builtins(ctx: &Ctx) -> Vec<Provider> {
    vec![
        claude(ctx),
        claude_desktop(ctx),
        codex(ctx),
        gemini(ctx),
        grok(ctx),
        opencode(ctx),
    ]
}

// ---- user-defined providers -------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CustomFile {
    #[serde(default)]
    provider: Vec<CustomProvider>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CustomProvider {
    id: String,
    name: Option<String>,
    binary: Option<String>,
    login: Option<Vec<String>>,
    install: Option<String>,
    app: Option<CustomApp>,
    #[serde(default)]
    artifact: Vec<CustomArtifact>,
    #[serde(default)]
    identity: Vec<CustomIdentity>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CustomApp {
    bundle_id: String,
    name: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
enum CustomArtifact {
    File {
        path: String,
        id: Option<String>,
        #[serde(default = "yes")]
        primary: bool,
    },
    JsonKeys {
        path: String,
        keys: Vec<String>,
        id: Option<String>,
        #[serde(default)]
        primary: bool,
    },
    Dir {
        path: String,
        id: Option<String>,
        #[serde(default)]
        primary: bool,
    },
    Keychain {
        service: String,
        account: Option<String>,
        id: Option<String>,
        #[serde(default = "yes")]
        primary: bool,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CustomIdentity {
    path: String,
    key: String,
    jwt_claim: Option<String>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Turn free text into something safe as a file name inside a snapshot.
fn slug(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_start_matches('.');
    if trimmed.is_empty() || trimmed == "meta.json" {
        format!("artifact_{trimmed}")
    } else {
        trimmed.to_string()
    }
}

fn build_custom(ctx: &Ctx, raw: CustomProvider) -> Result<Provider> {
    if !valid_id(&raw.id) {
        bail!(
            "provider id '{}' must be 1-32 characters of a-z, 0-9, '-' or '_'",
            raw.id
        );
    }
    if raw.artifact.is_empty() {
        bail!("provider '{}' declares no artifacts", raw.id);
    }
    let mut artifacts: Vec<Artifact> = Vec::new();
    for (index, item) in raw.artifact.into_iter().enumerate() {
        let (explicit, fallback, mut artifact) = match item {
            CustomArtifact::File { path, id, primary } => {
                let path = ctx.expand(&path);
                let fallback = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (id, fallback, Artifact::file("", path, primary))
            }
            CustomArtifact::JsonKeys {
                path,
                keys,
                id,
                primary,
            } => {
                if keys.is_empty() {
                    bail!("provider '{}': a json-keys artifact needs keys", raw.id);
                }
                let path = ctx.expand(&path);
                let fallback = format!(
                    "{}-keys",
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                );
                let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
                (id, fallback, Artifact::json_keys("", path, &keys, primary))
            }
            CustomArtifact::Dir { path, id, primary } => {
                let path = ctx.expand(&path);
                let fallback = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (id, fallback, Artifact::dir("", path, primary))
            }
            CustomArtifact::Keychain {
                service,
                account,
                id,
                primary,
            } => {
                let account = account.unwrap_or_else(|| ctx.user.clone());
                let fallback = format!("keychain-{service}");
                (
                    id,
                    fallback,
                    Artifact::keychain("", &service, &account, primary),
                )
            }
        };
        let mut id = slug(&explicit.unwrap_or(fallback));
        if artifacts.iter().any(|a| a.id == id) {
            id = format!("{id}-{index}");
        }
        artifact.id = id;
        artifacts.push(artifact);
    }
    if !artifacts.iter().any(|a| a.primary) {
        bail!(
            "provider '{}' needs at least one artifact with primary = true",
            raw.id
        );
    }
    let identity = raw
        .identity
        .into_iter()
        .map(|i| {
            let path = ctx.expand(&i.path);
            match i.jwt_claim {
                Some(claim) => IdentitySource::JwtClaim {
                    path,
                    key: i.key,
                    claim,
                },
                None => IdentitySource::JsonString { path, key: i.key },
            }
        })
        .collect();
    Ok(Provider {
        name: raw.name.unwrap_or_else(|| raw.id.clone()),
        id: raw.id,
        binary: raw.binary,
        login: raw.login.filter(|cmd| !cmd.is_empty()),
        install: raw.install.filter(|cmd| !cmd.trim().is_empty()),
        app: raw.app.map(|app| AppInfo {
            name: app.name.unwrap_or_else(|| app.bundle_id.clone()),
            paths: app.paths.iter().map(|p| ctx.expand(p)).collect(),
            bundle_id: app.bundle_id,
        }),
        artifacts,
        identity,
        label: Vec::new(),
        builtin: false,
    })
}

pub fn parse_custom(ctx: &Ctx, text: &str) -> Result<Vec<Provider>> {
    let file: CustomFile = toml::from_str(text)?;
    file.provider
        .into_iter()
        .map(|raw| build_custom(ctx, raw))
        .collect()
}

pub fn custom_file(ctx: &Ctx) -> PathBuf {
    ctx.data_dir.join("providers.toml")
}

/// Built-in providers plus those from `providers.toml`. A custom provider
/// with a built-in's id replaces it.
pub fn load_all(ctx: &Ctx) -> Result<Vec<Provider>> {
    let mut providers = builtins(ctx);
    let path = custom_file(ctx);
    if path.is_file() {
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let custom =
            parse_custom(ctx, &text).with_context(|| format!("parsing {}", path.display()))?;
        for provider in custom {
            match providers.iter_mut().find(|p| p.id == provider.id) {
                Some(slot) => *slot = provider,
                None => providers.push(provider),
            }
        }
    }
    Ok(providers)
}

/// Resolve what the user typed to a provider: exact id, or a unique prefix.
pub fn find<'a>(providers: &'a [Provider], query: &str) -> Result<&'a Provider> {
    let query = query.to_ascii_lowercase();
    if let Some(exact) = providers.iter().find(|p| p.id == query) {
        return Ok(exact);
    }
    let matches: Vec<&Provider> = providers
        .iter()
        .filter(|p| p.id.starts_with(&query))
        .collect();
    let ids = |list: &[&Provider]| {
        list.iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match matches.as_slice() {
        [only] => Ok(*only),
        [] => bail!(
            "unknown provider '{query}' (available: {})",
            ids(&providers.iter().collect::<Vec<_>>())
        ),
        many => bail!("'{query}' is ambiguous: {}", ids(many)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::Kind;

    #[test]
    fn builtins_follow_env_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = Ctx::for_test(dir.path(), false);
        ctx.set_var("CODEX_HOME", "/tmp/codex-alt");
        ctx.set_var("XDG_DATA_HOME", "/tmp/xdg");
        let providers = builtins(&ctx);
        let path_of = |id: &str| {
            let p = find(&providers, id).unwrap();
            match &p.artifacts[0].kind {
                Kind::File { path } => path.clone(),
                other => panic!("unexpected {other:?}"),
            }
        };
        assert_eq!(path_of("codex"), PathBuf::from("/tmp/codex-alt/auth.json"));
        assert_eq!(
            path_of("opencode"),
            PathBuf::from("/tmp/xdg/opencode/auth.json")
        );
        assert_eq!(path_of("grok"), dir.path().join(".grok/auth.json"));
        assert_eq!(
            path_of("claude"),
            dir.path().join(".claude/.credentials.json")
        );
    }

    #[test]
    fn claude_uses_keychain_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_test(dir.path(), true);
        let providers = builtins(&ctx);
        let claude = find(&providers, "claude").unwrap();
        assert!(matches!(
            &claude.artifacts[0].kind,
            Kind::Keychain { service, account }
                if service == "Claude Code-credentials" && account == "tester"
        ));
    }

    #[test]
    fn reads_the_email_cached_next_to_an_account_id() {
        let dir = tempfile::tempdir().unwrap();
        let idb = dir.path().join("https_claude.ai_0.indexeddb.leveldb");
        fs::create_dir_all(&idb).unwrap();
        let id = "09f01da4-8573-4be3-9c4f-42b5cf8c50da";
        let other = "11111111-2222-4333-8444-555555555555";
        let record = |uuid: &str, email: &str| {
            let mut b = b"\x00\xffo\"\x04uuid\"".to_vec();
            b.push(uuid.len() as u8);
            b.extend_from_slice(uuid.as_bytes());
            b.extend_from_slice(b"\"\x0demail_address\"");
            b.push(email.len() as u8);
            b.extend_from_slice(email.as_bytes());
            b.extend_from_slice(b"\"\x09full_name\"\x04Phil");
            b
        };
        let mut log = b"noise".to_vec();
        log.extend(record(other, "someone@else.io"));
        log.extend(record(id, "me@example.com"));
        fs::write(idb.join("000004.log"), &log).unwrap();
        fs::write(idb.join("000005.ldb"), b"\x8f\x01compressed garbage").unwrap();

        assert_eq!(cached_email(dir.path(), id).as_deref(), Some("me@example.com"));
        assert_eq!(cached_email(dir.path(), other).as_deref(), Some("someone@else.io"));
        assert_eq!(cached_email(dir.path(), "not-there"), None);
        assert_eq!(cached_email(dir.path(), ""), None);
        assert_eq!(cached_email(&dir.path().join("missing"), id), None);

        // A record cut off in the middle yields nothing rather than garbage.
        let mut cut = record(id, "me@example.com");
        cut.truncate(cut.len() - 25);
        fs::write(idb.join("000004.log"), &cut).unwrap();
        assert_eq!(cached_email(dir.path(), id), None);

        // Blink compressed the value with Snappy ("mail" of "gmail" became a
        // copy of the "mail" in "email_address"), and the LevelDB log split
        // it across a block boundary.
        let mut value = vec![b'x'; 40_000];
        value.extend(record(id, "someone@gmail.com"));
        let mail = find_bytes_in(&value, b"email_address") + 1;
        let gmail = find_bytes_in(&value, b"gmail.com") + 1;
        let packed = crate::chromium::tests::snappy_encode(&value, &[(gmail, 4, gmail - mail)]);
        let mut stored = b"\xff\x11\x02".to_vec();
        stored.extend(packed);
        assert!(find_bytes_in_opt(&stored, b"gmail").is_none(), "not readable as-is");
        let log = crate::chromium::tests::leveldb_log(&[b"other record", &stored]);
        fs::write(idb.join("000004.log"), &log).unwrap();
        assert_eq!(cached_email(dir.path(), id).as_deref(), Some("someone@gmail.com"));

        // After compaction the same value sits in a Snappy-compressed table.
        fs::remove_file(idb.join("000004.log")).unwrap();
        fs::write(idb.join("000007.ldb"), crate::chromium::tests::leveldb_table(&stored, true)).unwrap();
        assert_eq!(cached_email(dir.path(), id).as_deref(), Some("someone@gmail.com"));
    }

    fn find_bytes_in_opt(hay: &[u8], needle: &[u8]) -> Option<usize> {
        crate::chromium::find_bytes(hay, needle)
    }

    fn find_bytes_in(hay: &[u8], needle: &[u8]) -> usize {
        find_bytes_in_opt(hay, needle).expect("present")
    }

    #[test]
    fn find_accepts_unique_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_test(dir.path(), false);
        let providers = builtins(&ctx);
        assert_eq!(find(&providers, "claude").unwrap().id, "claude", "exact id wins over prefix");
        assert_eq!(find(&providers, "claude-d").unwrap().id, "claude-desktop");
        assert!(find(&providers, "cl").is_err(), "claude and claude-desktop both match");
        assert_eq!(find(&providers, "Codex").unwrap().id, "codex");
        assert!(find(&providers, "g").is_err(), "gemini and grok both match");
        assert!(find(&providers, "nope").is_err());
    }

    #[test]
    fn parses_custom_providers() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_test(dir.path(), false);
        let text = r#"
            [[provider]]
            id = "qwen"
            name = "Qwen Code"
            binary = "qwen"
            login = ["qwen"]

            [[provider.artifact]]
            type = "file"
            path = "~/.qwen/oauth_creds.json"

            [[provider.artifact]]
            type = "json-keys"
            path = "~/.qwen/settings.json"
            keys = ["security.auth"]

            [[provider.identity]]
            path = "~/.qwen/oauth_creds.json"
            key = "id_token"
            jwt_claim = "email"
        "#;
        let providers = parse_custom(&ctx, text).unwrap();
        assert_eq!(providers.len(), 1);
        let qwen = &providers[0];
        assert_eq!(qwen.name, "Qwen Code");
        assert_eq!(qwen.artifacts.len(), 2);
        assert_eq!(qwen.artifacts[0].id, "oauth_creds.json");
        assert!(qwen.artifacts[0].primary);
        assert_eq!(qwen.artifacts[1].id, "settings.json-keys");
        assert!(!qwen.artifacts[1].primary);
        assert!(matches!(
            &qwen.artifacts[0].kind,
            Kind::File { path } if *path == dir.path().join(".qwen/oauth_creds.json")
        ));
    }

    #[test]
    fn rejects_bad_custom_providers() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_test(dir.path(), false);
        assert!(parse_custom(&ctx, "[[provider]]\nid = \"Bad Id\"").is_err());
        assert!(parse_custom(&ctx, "[[provider]]\nid = \"empty\"").is_err());
        let no_primary = r#"
            [[provider]]
            id = "x"
            [[provider.artifact]]
            type = "json-keys"
            path = "~/x.json"
            keys = ["a"]
        "#;
        assert!(parse_custom(&ctx, no_primary).is_err());
    }

    #[test]
    fn custom_provider_overrides_builtin() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_test(dir.path(), false);
        fs::create_dir_all(&ctx.data_dir).unwrap();
        fs::write(
            custom_file(&ctx),
            "[[provider]]\nid = \"grok\"\n[[provider.artifact]]\ntype = \"file\"\npath = \"~/.grok-cli/auth.json\"\n",
        )
        .unwrap();
        let providers = load_all(&ctx).unwrap();
        assert_eq!(providers.len(), 6);
        let grok = find(&providers, "grok").unwrap();
        assert!(!grok.builtin);
        assert!(matches!(
            &grok.artifacts[0].kind,
            Kind::File { path } if *path == dir.path().join(".grok-cli/auth.json")
        ));
    }
}
