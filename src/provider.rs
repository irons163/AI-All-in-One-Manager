//! Provider definitions: which artifacts make up a login for each AI CLI,
//! how to tell which account is logged in, and how to start a fresh login.
//!
//! Built-in: Claude Code, Codex, Gemini CLI, Grok, OpenCode. More can be
//! declared (or a built-in overridden) in `<data dir>/providers.toml`.

use crate::artifact::Artifact;
use crate::ctx::Ctx;
use crate::util::{json_get, jwt_claim};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

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
    pub artifacts: Vec<Artifact>,
    /// Tried in order; the first that yields a value names the live account.
    pub identity: Vec<IdentitySource>,
    pub builtin: bool,
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
}

impl IdentitySource {
    fn resolve(&self) -> Option<String> {
        let (path, key) = match self {
            IdentitySource::JsonString { path, key }
            | IdentitySource::JwtClaim { path, key, .. }
            | IdentitySource::Present { path, key, .. } => (path, key),
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
        }
    }
}

impl Provider {
    /// Who is logged in right now, if the provider's files say.
    pub fn live_identity(&self) -> Option<String> {
        self.identity.iter().find_map(IdentitySource::resolve)
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
        artifacts,
        identity: vec![json_string(&state_file, "oauthAccount.emailAddress")],
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
        artifacts,
        identity: vec![json_string(&accounts, "active")],
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
        artifacts: vec![Artifact::file("auth.json", auth.clone(), true)],
        identity: vec![
            json_string(&auth, "email"),
            json_string(&auth, "user.email"),
            json_string(&auth, "account.email"),
            jwt(&auth, "id_token", "email"),
            jwt(&auth, "tokens.id_token", "email"),
        ],
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
        artifacts: vec![Artifact::file(
            "auth.json",
            data.join("opencode").join("auth.json"),
            true,
        )],
        identity: Vec::new(),
        builtin: true,
    }
}

pub fn builtins(ctx: &Ctx) -> Vec<Provider> {
    vec![claude(ctx), codex(ctx), gemini(ctx), grok(ctx), opencode(ctx)]
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
    #[serde(default)]
    artifact: Vec<CustomArtifact>,
    #[serde(default)]
    identity: Vec<CustomIdentity>,
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
        artifacts,
        identity,
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
    fn find_accepts_unique_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_test(dir.path(), false);
        let providers = builtins(&ctx);
        assert_eq!(find(&providers, "cl").unwrap().id, "claude");
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
        assert_eq!(providers.len(), 5);
        let grok = find(&providers, "grok").unwrap();
        assert!(!grok.builtin);
        assert!(matches!(
            &grok.artifacts[0].kind,
            Kind::File { path } if *path == dir.path().join(".grok-cli/auth.json")
        ));
    }
}
