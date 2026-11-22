//! End-to-end account flows against a throwaway home directory. Nothing here
//! touches the real home or the real keychain.

use aiom::ctx::Ctx;
use aiom::ops;
use aiom::provider::{self, Provider};
use aiom::store;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn setup(keychain: bool) -> (TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::for_test(dir.path(), keychain);
    (dir, ctx)
}

fn get(ctx: &Ctx, id: &str) -> Provider {
    let all = provider::load_all(ctx).unwrap();
    provider::find(&all, id).unwrap().clone()
}

fn write(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
}

fn read(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn codex_auth(home: &Path) -> PathBuf {
    home.join(".codex/auth.json")
}

fn codex_login(home: &Path, email: &str, token: &str) {
    write(
        &codex_auth(home),
        &json!({"email": email, "tokens": {"access_token": token}}),
    );
}

#[test]
fn save_and_switch_between_two_accounts() {
    let (dir, ctx) = setup(false);
    let codex = get(&ctx, "codex");

    assert!(ops::save(&ctx, &codex, "work", false, None).is_err(), "not logged in yet");

    codex_login(dir.path(), "work@corp.com", "w1");
    let meta = ops::save(&ctx, &codex, "work", false, Some("day job".into())).unwrap();
    assert_eq!(meta.identity.as_deref(), Some("work@corp.com"));
    assert!(ops::save(&ctx, &codex, "work", false, None).is_err(), "needs --force");

    codex_login(dir.path(), "me@home.io", "p1");
    ops::save(&ctx, &codex, "personal", false, None).unwrap();
    assert_eq!(store::active(&ctx, "codex").unwrap().as_deref(), Some("personal"));

    let report = ops::switch(&ctx, &codex, "work", false).unwrap();
    assert!(report.synced);
    assert!(report.warnings.is_empty());
    assert_eq!(read(&codex_auth(dir.path()))["email"], "work@corp.com");
    assert_eq!(store::active(&ctx, "codex").unwrap().as_deref(), Some("work"));

    ops::switch(&ctx, &codex, "personal", false).unwrap();
    assert_eq!(read(&codex_auth(dir.path()))["email"], "me@home.io");

    assert!(ops::switch(&ctx, &codex, "nobody", false).is_err());
    assert_eq!(store::list(&ctx, "codex").unwrap().len(), 2);
}

#[test]
fn refreshed_tokens_follow_the_account() {
    let (dir, ctx) = setup(false);
    let codex = get(&ctx, "codex");
    codex_login(dir.path(), "a@x.io", "a1");
    ops::save(&ctx, &codex, "a", false, None).unwrap();
    codex_login(dir.path(), "b@x.io", "b1");
    ops::save(&ctx, &codex, "b", false, None).unwrap();

    ops::switch(&ctx, &codex, "a", false).unwrap();
    // The CLI refreshes a's token in place while a is active.
    codex_login(dir.path(), "a@x.io", "a2-refreshed");

    ops::switch(&ctx, &codex, "b", false).unwrap();
    assert_eq!(read(&codex_auth(dir.path()))["tokens"]["access_token"], "b1");
    ops::switch(&ctx, &codex, "a", false).unwrap();
    assert_eq!(
        read(&codex_auth(dir.path()))["tokens"]["access_token"],
        "a2-refreshed"
    );
}

#[test]
fn switching_to_the_active_account_is_a_no_op() {
    let (dir, ctx) = setup(false);
    let codex = get(&ctx, "codex");
    codex_login(dir.path(), "a@x.io", "a1");
    ops::save(&ctx, &codex, "a", false, None).unwrap();
    codex_login(dir.path(), "a@x.io", "a2-refreshed");

    let report = ops::switch(&ctx, &codex, "a", false).unwrap();
    assert!(report.already_active);
    assert_eq!(
        read(&codex_auth(dir.path()))["tokens"]["access_token"],
        "a2-refreshed",
        "a fresher live token is not rolled back"
    );

    let report = ops::switch(&ctx, &codex, "a", true).unwrap();
    assert!(!report.already_active);
    assert_eq!(read(&codex_auth(dir.path()))["tokens"]["access_token"], "a1");
}

#[test]
fn a_foreign_live_login_never_overwrites_an_account() {
    let (dir, ctx) = setup(false);
    let codex = get(&ctx, "codex");
    codex_login(dir.path(), "a@x.io", "a1");
    ops::save(&ctx, &codex, "a", false, None).unwrap();
    codex_login(dir.path(), "b@x.io", "b1");
    ops::save(&ctx, &codex, "b", false, None).unwrap();
    ops::switch(&ctx, &codex, "a", false).unwrap();

    // The user logs in as someone else directly in the CLI.
    codex_login(dir.path(), "stranger@x.io", "s1");
    assert!(ops::sync(&ctx, &codex, false).is_err());

    let report = ops::switch(&ctx, &codex, "b", false).unwrap();
    assert!(!report.synced);
    assert_eq!(report.warnings.len(), 1);
    assert_eq!(read(&codex_auth(dir.path()))["email"], "b@x.io");

    // The stranger's login was backed up, and 'a' is still a.
    let backup = report.backup.unwrap();
    assert_eq!(read(&backup.join("auth.json"))["email"], "stranger@x.io");
    ops::switch(&ctx, &codex, "a", false).unwrap();
    assert_eq!(read(&codex_auth(dir.path()))["email"], "a@x.io");

    // `restore` brings back what was live before that last switch (b).
    let (_, identity) = ops::restore(&ctx, &codex).unwrap();
    assert_eq!(identity.as_deref(), Some("b@x.io"));
    assert_eq!(read(&codex_auth(dir.path()))["email"], "b@x.io");
    assert_eq!(store::active(&ctx, "codex").unwrap(), None);
}

#[test]
fn claude_switch_preserves_the_rest_of_claude_json() {
    let (dir, ctx) = setup(false);
    let claude = get(&ctx, "claude");
    let creds = dir.path().join(".claude/.credentials.json");
    let state = dir.path().join(".claude.json");
    let login = |email: &str, token: &str, startups: u32| {
        write(&creds, &json!({"claudeAiOauth": {"accessToken": token}}));
        write(
            &state,
            &json!({
                "numStartups": startups,
                "oauthAccount": {"emailAddress": email, "organizationName": "Org"},
                "projects": {"/tmp/p": {"history": []}}
            }),
        );
    };

    login("a@x.io", "tok-a", 1);
    ops::save(&ctx, &claude, "a", false, None).unwrap();
    login("b@x.io", "tok-b", 2);
    ops::save(&ctx, &claude, "b", false, None).unwrap();

    ops::switch(&ctx, &claude, "a", false).unwrap();
    assert_eq!(read(&creds)["claudeAiOauth"]["accessToken"], "tok-a");
    let root = read(&state);
    assert_eq!(root["oauthAccount"]["emailAddress"], "a@x.io");
    assert_eq!(root["numStartups"], 2, "unrelated state is not rolled back");
    assert!(root["projects"]["/tmp/p"].is_object());
    let keys: Vec<&String> = root.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["numStartups", "oauthAccount", "projects"]);
}

#[test]
fn claude_keychain_credentials_are_swapped() {
    let (dir, ctx) = setup(true);
    let claude = get(&ctx, "claude");
    let state = dir.path().join(".claude.json");
    let service = "Claude Code-credentials";
    let login = |email: &str, token: &str| {
        ctx.secrets.set(service, "tester", token).unwrap();
        write(&state, &json!({"oauthAccount": {"emailAddress": email}}));
    };

    login("a@x.io", "kc-a");
    ops::save(&ctx, &claude, "a", false, None).unwrap();
    login("b@x.io", "kc-b");
    ops::save(&ctx, &claude, "b", false, None).unwrap();

    ops::switch(&ctx, &claude, "a", false).unwrap();
    assert_eq!(ctx.secrets.get(service, "tester").unwrap().as_deref(), Some("kc-a"));
    assert_eq!(read(&state)["oauthAccount"]["emailAddress"], "a@x.io");

    // Saved copies live in the keychain, not on disk.
    let on_disk = store::profile_loc(&ctx, "claude", "b").dir.join("keychain-credentials");
    assert!(!on_disk.exists());
    assert_eq!(
        ctx.secrets.get("aiom:claude:b", "keychain-credentials").unwrap().as_deref(),
        Some("kc-b")
    );

    ops::rename(&ctx, &claude, "b", "second").unwrap();
    assert_eq!(ctx.secrets.get("aiom:claude:b", "keychain-credentials").unwrap(), None);
    ops::switch(&ctx, &claude, "second", false).unwrap();
    assert_eq!(ctx.secrets.get(service, "tester").unwrap().as_deref(), Some("kc-b"));

    ops::remove(&ctx, &claude, "a").unwrap();
    assert_eq!(ctx.secrets.get("aiom:claude:a", "keychain-credentials").unwrap(), None);
    assert_eq!(store::list(&ctx, "claude").unwrap().len(), 1);
}

#[test]
fn artifacts_absent_from_an_account_are_removed_on_switch() {
    let (dir, ctx) = setup(false);
    let gemini = get(&ctx, "gemini");
    let g = dir.path().join(".gemini");

    // Account 1: Google login.
    write(&g.join("oauth_creds.json"), &json!({"refresh_token": "r1"}));
    write(&g.join("google_accounts.json"), &json!({"active": "g@x.io", "old": []}));
    write(
        &g.join("settings.json"),
        &json!({"ui": {"theme": "dark"}, "security": {"auth": {"selectedType": "oauth-personal"}}}),
    );
    ops::save(&ctx, &gemini, "google", false, None).unwrap();

    // Account 2: API key only.
    fs::remove_file(g.join("oauth_creds.json")).unwrap();
    fs::remove_file(g.join("google_accounts.json")).unwrap();
    fs::write(g.join(".env"), "GEMINI_API_KEY=k\n").unwrap();
    write(
        &g.join("settings.json"),
        &json!({"ui": {"theme": "dark"}, "security": {"auth": {"selectedType": "gemini-api-key"}}}),
    );
    ops::save(&ctx, &gemini, "apikey", false, None).unwrap();

    ops::switch(&ctx, &gemini, "google", false).unwrap();
    assert!(g.join("oauth_creds.json").is_file());
    assert!(!g.join(".env").exists(), "the API key belongs to the other account");
    let settings = read(&g.join("settings.json"));
    assert_eq!(settings["security"]["auth"]["selectedType"], "oauth-personal");
    assert_eq!(settings["ui"]["theme"], "dark");

    ops::switch(&ctx, &gemini, "apikey", false).unwrap();
    assert!(!g.join("oauth_creds.json").exists());
    assert_eq!(fs::read_to_string(g.join(".env")).unwrap(), "GEMINI_API_KEY=k\n");
}

#[test]
fn next_cycles_and_remove_clears_active() {
    let (dir, ctx) = setup(false);
    let grok = get(&ctx, "grok");
    let auth = dir.path().join(".grok/auth.json");
    for name in ["one", "three", "two"] {
        write(&auth, &json!({"email": format!("{name}@x.io")}));
        ops::save(&ctx, &grok, name, false, None).unwrap();
    }
    // Sorted: one, three, two. Active is "two", the last saved.
    assert_eq!(ops::next_account(&ctx, &grok).unwrap(), "one");
    ops::switch(&ctx, &grok, "one", false).unwrap();
    assert_eq!(ops::next_account(&ctx, &grok).unwrap(), "three");

    ops::remove(&ctx, &grok, "one").unwrap();
    assert_eq!(store::active(&ctx, "grok").unwrap(), None);
    assert!(auth.is_file(), "removing an account leaves the live login alone");
    assert!(ops::remove(&ctx, &grok, "one").is_err());

    let st = ops::status(&ctx, &grok).unwrap();
    assert!(st.logged_in);
    assert_eq!(st.live_identity.as_deref(), Some("one@x.io"));
    assert!(st.active.is_none());
    assert_eq!(st.accounts.len(), 2);
}

#[test]
fn opencode_accounts_work_without_an_identity() {
    let (dir, ctx) = setup(false);
    let opencode = get(&ctx, "opencode");
    let auth = dir.path().join(".local/share/opencode/auth.json");
    write(&auth, &json!({"anthropic": {"type": "oauth", "refresh": "r1"}}));
    ops::save(&ctx, &opencode, "max", false, None).unwrap();
    write(&auth, &json!({"openai": {"type": "api", "key": "k"}}));
    ops::save(&ctx, &opencode, "api", false, None).unwrap();

    ops::switch(&ctx, &opencode, "max", false).unwrap();
    assert!(read(&auth)["anthropic"].is_object());
    ops::switch(&ctx, &opencode, "api", false).unwrap();
    assert!(read(&auth)["openai"].is_object());
}

#[test]
fn adopt_saves_an_unsaved_login_under_its_identity() {
    let (dir, ctx) = setup(false);
    let codex = get(&ctx, "codex");
    assert!(ops::adopt(&ctx, &codex).unwrap().is_none(), "not logged in");

    codex_login(dir.path(), "phil@home.io", "p1");
    let adopted = ops::adopt(&ctx, &codex).unwrap().expect("saved");
    assert!(adopted.created);
    assert_eq!(adopted.name, "phil");
    assert_eq!(adopted.identity.as_deref(), Some("phil@home.io"));
    assert_eq!(store::active(&ctx, "codex").unwrap().as_deref(), Some("phil"));

    // Nothing more to do, even after the CLI refreshes the token.
    codex_login(dir.path(), "phil@home.io", "p2");
    assert!(ops::adopt(&ctx, &codex).unwrap().is_none());
    assert_eq!(store::list(&ctx, "codex").unwrap().len(), 1);
}

#[test]
fn adopt_follows_logins_made_outside_aiom() {
    let (dir, ctx) = setup(false);
    let codex = get(&ctx, "codex");
    codex_login(dir.path(), "phil@home.io", "p1");
    ops::adopt(&ctx, &codex).unwrap();

    // The user runs `codex login` as someone else: a second account appears.
    codex_login(dir.path(), "phil@work.io", "w1");
    let second = ops::adopt(&ctx, &codex).unwrap().expect("saved");
    assert!(second.created);
    assert_eq!(second.name, "phil@work.io", "the short name was taken");
    assert_eq!(store::active(&ctx, "codex").unwrap().as_deref(), Some("phil@work.io"));

    // Logging back in as the first user reattaches the existing account
    // and keeps the fresher token, without creating a duplicate.
    codex_login(dir.path(), "PHIL@home.io", "p3");
    let back = ops::adopt(&ctx, &codex).unwrap().expect("reattached");
    assert!(!back.created);
    assert_eq!(back.name, "phil");
    assert_eq!(store::list(&ctx, "codex").unwrap().len(), 2);

    ops::switch(&ctx, &codex, "phil@work.io", false).unwrap();
    assert_eq!(read(&codex_auth(dir.path()))["tokens"]["access_token"], "w1");
    ops::switch(&ctx, &codex, "phil", false).unwrap();
    assert_eq!(read(&codex_auth(dir.path()))["tokens"]["access_token"], "p3");
}

#[test]
fn adopt_recognises_identity_less_logins_by_content() {
    let (dir, ctx) = setup(false);
    let opencode = get(&ctx, "opencode");
    let auth = dir.path().join(".local/share/opencode/auth.json");

    write(&auth, &json!({"anthropic": {"refresh": "r1"}}));
    let first = ops::adopt(&ctx, &opencode).unwrap().expect("saved");
    assert_eq!(first.name, "account");

    write(&auth, &json!({"openai": {"key": "k"}}));
    ops::save(&ctx, &opencode, "api", false, None).unwrap();
    ops::switch(&ctx, &opencode, "account", false).unwrap();

    // After a restore there is no active account; the live login is byte for
    // byte the saved "api" account, so it is reattached rather than duplicated.
    ops::restore(&ctx, &opencode).unwrap();
    let again = ops::adopt(&ctx, &opencode).unwrap().expect("reattached");
    assert!(!again.created);
    assert_eq!(again.name, "api");

    // A login that matches nothing gets a new numbered name.
    ops::restore(&ctx, &opencode).unwrap();
    write(&auth, &json!({"xai": {"key": "new"}}));
    let third = ops::adopt(&ctx, &opencode).unwrap().expect("saved");
    assert!(third.created);
    assert_eq!(third.name, "account-2");
    assert_eq!(store::list(&ctx, "opencode").unwrap().len(), 3);
}

// ---- Claude desktop app ------------------------------------------------------

use aiom::apps::FakeApps;

const BUNDLE: &str = "com.anthropic.claudefordesktop";

fn desktop_dir(home: &Path) -> PathBuf {
    home.join("Library/Application Support/Claude")
}

/// Lay out a signed-in Claude desktop profile for account `who`.
fn desktop_login(home: &Path, who: &str, startups: u32) {
    let dir = desktop_dir(home);
    write(
        &dir.join("config.json"),
        &json!({
            "locale": "zh-TW",
            "launches": startups,
            "lastKnownAccountUuid": format!("uuid-{who}"),
            "oauth:tokenCacheV2": format!("djEw-v2-{who}"),
            "oauth:tokenCache": format!("djEw-v1-{who}"),
            "userThemeMode": "dark"
        }),
    );
    fs::write(dir.join("Cookies"), format!("sqlite cookies of {who}")).unwrap();
    let ls = dir.join("Local Storage/leveldb");
    let _ = fs::remove_dir_all(dir.join("Local Storage"));
    fs::create_dir_all(&ls).unwrap();
    fs::write(ls.join("000003.log"), format!("local storage of {who}")).unwrap();
    let idb = dir.join("IndexedDB/https_claude.ai_0.indexeddb.leveldb");
    let _ = fs::remove_dir_all(dir.join("IndexedDB"));
    fs::create_dir_all(&idb).unwrap();
    fs::write(idb.join("CURRENT"), format!("idb of {who}")).unwrap();
    // The web app's cached account object, V8-serialised as the app stores it.
    fs::write(idb.join("000004.log"), v8_account(&format!("uuid-{who}"), &format!("{who}@example.com"))).unwrap();
    // Not part of the login: must never be touched.
    fs::create_dir_all(dir.join("claude-code-sessions")).unwrap();
    fs::write(dir.join("claude_desktop_config.json"), "{\"mcpServers\":{}}").unwrap();
}

fn v8_account(uuid: &str, email: &str) -> Vec<u8> {
    let mut b = b"\xff\x0fo\"\x04uuid\"".to_vec();
    b.push(uuid.len() as u8);
    b.extend_from_slice(uuid.as_bytes());
    b.extend_from_slice(b"\"\x0demail_address\"");
    b.push(email.len() as u8);
    b.extend_from_slice(email.as_bytes());
    b
}

fn desktop_ctx(home: &Path, running: bool, refuse_quit: bool) -> Ctx {
    let mut ctx = Ctx::for_test(home, false);
    let apps = FakeApps::default();
    apps.running.set(running);
    apps.refuse_quit.set(refuse_quit);
    ctx.apps = Box::new(apps);
    ctx
}

fn who_is_live(home: &Path) -> String {
    let dir = desktop_dir(home);
    let config = read(&dir.join("config.json"));
    let cookies = fs::read_to_string(dir.join("Cookies")).unwrap();
    let ls = fs::read_to_string(dir.join("Local Storage/leveldb/000003.log")).unwrap();
    let idb = fs::read_to_string(dir.join("IndexedDB/https_claude.ai_0.indexeddb.leveldb/CURRENT")).unwrap();
    let who = config["lastKnownAccountUuid"].as_str().unwrap().trim_start_matches("uuid-").to_string();
    assert_eq!(config["oauth:tokenCacheV2"], format!("djEw-v2-{who}"));
    assert_eq!(cookies, format!("sqlite cookies of {who}"));
    assert_eq!(ls, format!("local storage of {who}"));
    assert_eq!(idb, format!("idb of {who}"));
    who
}

#[test]
fn desktop_switch_closes_swaps_and_reopens_the_app() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let ctx = desktop_ctx(home, true, false);
    let desktop = get(&ctx, "claude-desktop");

    desktop_login(home, "alice", 1);
    let a = ops::adopt(&ctx, &desktop).unwrap().unwrap();
    assert_eq!(a.name, "alice", "named after the cached email");
    assert_eq!(a.identity.as_deref(), Some("uuid-alice"), "matched by the account id");
    assert_eq!(a.label.as_deref(), Some("alice@example.com"));

    desktop_login(home, "bob", 2);
    let bob = ops::save(&ctx, &desktop, "bob", false, None).unwrap();
    assert_eq!(bob.label.as_deref(), Some("bob@example.com"));
    assert!(ctx.apps.log().is_empty(), "saving never closes the app");

    // The app keeps working on bob and writes unrelated settings meanwhile.
    let mut config = read(&desktop_dir(home).join("config.json"));
    config["userThemeMode"] = json!("light");
    write(&desktop_dir(home).join("config.json"), &config);

    let report = ops::switch(&ctx, &desktop, "alice", false).unwrap();
    assert_eq!(report.label.as_deref(), Some("alice@example.com"));
    assert_eq!(ctx.apps.log(), [format!("quit {BUNDLE}"), format!("launch {BUNDLE}")]);
    assert_eq!(report.restarted_app.as_deref(), Some("Claude"));
    assert!(report.synced);
    assert_eq!(who_is_live(home), "alice");
    let config = read(&desktop_dir(home).join("config.json"));
    assert_eq!(config["userThemeMode"], "light", "settings outside the login stay as they are");
    assert_eq!(config["launches"], 2);
    assert!(desktop_dir(home).join("claude-code-sessions").is_dir());
    assert_eq!(
        fs::read_to_string(desktop_dir(home).join("claude_desktop_config.json")).unwrap(),
        "{\"mcpServers\":{}}"
    );

    ops::switch(&ctx, &desktop, "bob", false).unwrap();
    assert_eq!(who_is_live(home), "bob");

    // Already on bob: nothing is closed.
    let before = ctx.apps.log().len();
    assert!(ops::switch(&ctx, &desktop, "bob", false).unwrap().already_active);
    assert_eq!(ctx.apps.log().len(), before);
}

#[test]
fn desktop_switch_changes_nothing_when_the_app_will_not_quit() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let ctx = desktop_ctx(home, false, false);
    let desktop = get(&ctx, "claude-desktop");
    desktop_login(home, "alice", 1);
    ops::save(&ctx, &desktop, "alice", false, None).unwrap();
    desktop_login(home, "bob", 1);
    ops::save(&ctx, &desktop, "bob", false, None).unwrap();

    // Not running: switched without starting it.
    ops::switch(&ctx, &desktop, "alice", false).unwrap();
    assert!(ctx.apps.log().is_empty());
    assert_eq!(who_is_live(home), "alice");

    let mut stubborn = desktop_ctx(home, true, true);
    stubborn.set_var("AIOM_QUIT_TIMEOUT_SECS", "1");
    let err = ops::switch(&stubborn, &desktop, "bob", false).unwrap_err();
    assert!(format!("{err:#}").contains("did not quit"), "{err:#}");
    assert!(format!("{err:#}").contains("nothing was changed"), "{err:#}");
    assert_eq!(who_is_live(home), "alice", "the live login is untouched");
    assert_eq!(store::active(&stubborn, "claude-desktop").unwrap().as_deref(), Some("alice"));
    assert_eq!(stubborn.apps.log(), [format!("quit {BUNDLE}")], "not restarted either");
}

#[test]
fn desktop_logout_makes_room_for_a_new_named_account() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    // The app has to be "installed" for a logout to be offered.
    let mut ctx = desktop_ctx(home, true, false);
    fs::create_dir_all(home.join("Applications/Claude.app")).unwrap();
    ctx.set_var("PATH", "/usr/bin");
    let desktop = get(&ctx, "claude-desktop");

    desktop_login(home, "alice", 1);
    ops::save(&ctx, &desktop, "alice", false, None).unwrap();
    assert!(ops::app_logout(&ctx, &desktop, Some("alice")).is_err(), "name taken");

    ops::app_logout(&ctx, &desktop, Some("work")).unwrap();
    assert_eq!(ctx.apps.log(), [format!("quit {BUNDLE}"), format!("launch {BUNDLE}")]);
    let d = desktop_dir(home);
    assert!(!d.join("Cookies").exists());
    assert!(!d.join("Local Storage").exists());
    let config = read(&d.join("config.json"));
    assert!(config.get("oauth:tokenCacheV2").is_none());
    assert_eq!(config["locale"], "zh-TW", "only the login is cleared");
    assert!(!desktop.logged_in(&ctx));
    assert_eq!(store::active(&ctx, "claude-desktop").unwrap(), None);

    // The user signs in to another account inside the app.
    desktop_login(home, "carol", 1);
    let saved = ops::adopt(&ctx, &desktop).unwrap().unwrap();
    assert_eq!(saved.name, "work", "the name chosen beforehand is used");
    assert!(store::load_state(&ctx).unwrap().pending.is_empty());

    ops::switch(&ctx, &desktop, "alice", false).unwrap();
    assert_eq!(who_is_live(home), "alice");
    ops::switch(&ctx, &desktop, "work", false).unwrap();
    assert_eq!(who_is_live(home), "carol");
}

#[test]
fn desktop_email_is_a_label_not_an_identity() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let ctx = desktop_ctx(home, false, false);
    let desktop = get(&ctx, "claude-desktop");
    let idb_log = desktop_dir(home).join("IndexedDB/https_claude.ai_0.indexeddb.leveldb/000004.log");

    // The cache has been compacted away: no readable email.
    desktop_login(home, "dana", 1);
    fs::remove_file(&idb_log).unwrap();
    let first = ops::adopt(&ctx, &desktop).unwrap().unwrap();
    assert_eq!(first.name, "account");
    assert_eq!(first.label, None);

    // Later the app writes the account object again: the existing account
    // learns its email; no second account appears for the same person.
    fs::write(&idb_log, v8_account("uuid-dana", "dana@example.com")).unwrap();
    assert!(ops::adopt(&ctx, &desktop).unwrap().is_none());
    let accounts = store::list(&ctx, "claude-desktop").unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].label.as_deref(), Some("dana@example.com"));
    assert_eq!(accounts[0].shown(), Some("dana@example.com"));

    // And when the email disappears again, it is still the same account.
    fs::remove_file(&idb_log).unwrap();
    assert!(ops::adopt(&ctx, &desktop).unwrap().is_none());
    let st = ops::status(&ctx, &desktop).unwrap();
    assert!(st.matches_active);
    assert_eq!(st.live_label, None);
    assert_eq!(st.active.unwrap().label.as_deref(), Some("dana@example.com"), "the saved label is kept");
}

#[test]
fn desktop_backfills_the_email_of_an_inactive_account_from_its_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let ctx = desktop_ctx(home, false, false);
    let desktop = get(&ctx, "claude-desktop");
    let idb_log = desktop_dir(home).join("IndexedDB/https_claude.ai_0.indexeddb.leveldb/000004.log");

    // "erin" was saved while her email could not be read...
    desktop_login(home, "erin", 1);
    let cached = fs::read(&idb_log).unwrap();
    fs::remove_file(&idb_log).unwrap();
    ops::save(&ctx, &desktop, "erin", false, None).unwrap();
    // ...then the cache appeared and was folded into her snapshot on the
    // way out, but by a version of aiom that did not record labels.
    fs::write(&idb_log, &cached).unwrap();
    ops::sync(&ctx, &desktop, false).unwrap();
    let mut meta = store::load_meta(&ctx, "claude-desktop", "erin").unwrap().unwrap();
    meta.label = None;
    store::save_meta(&ctx, &meta).unwrap();

    desktop_login(home, "frank", 1);
    ops::save(&ctx, &desktop, "frank", false, None).unwrap();

    // erin is not live; her email comes from her saved snapshot.
    let label = ops::backfill_label(&ctx, &desktop, "erin").unwrap();
    assert_eq!(label.as_deref(), Some("erin@example.com"));
    let meta = store::load_meta(&ctx, "claude-desktop", "erin").unwrap().unwrap();
    assert_eq!(meta.label.as_deref(), Some("erin@example.com"));
    assert_eq!(who_is_live(home), "frank", "the live login is not touched");

    // Providers without labels, and accounts that already have one, are left as they are.
    assert_eq!(ops::backfill_label(&ctx, &desktop, "frank").unwrap().as_deref(), Some("frank@example.com"));
    let codex = get(&ctx, "codex");
    assert_eq!(ops::backfill_label(&ctx, &codex, "nobody").unwrap(), None);
}
