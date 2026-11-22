//! AIOM desktop app: a window on top of the same core the `aiom` CLI uses.
//!
//! Started without arguments it opens the window. Started with arguments it
//! behaves exactly like the `aiom` CLI, which is how the app runs a
//! provider's interactive login inside a Terminal window.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use aiom::ctx::Ctx;
use aiom::provider::{self, Provider};
use aiom::{ops, store, util};
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

// ---- environment -------------------------------------------------------------

/// Apps launched from Finder get a bare PATH, so the user's CLIs (usually in
/// `~/.local/bin`, Homebrew, npm or cargo directories) would look missing.
/// Ask the login shell for its PATH once and add the usual suspects.
fn full_path(ctx: &Ctx) -> &'static str {
    static FULL_PATH: OnceLock<String> = OnceLock::new();
    FULL_PATH.get_or_init(|| {
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut add = |raw: &str| {
            for dir in std::env::split_paths(raw) {
                if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
                    dirs.push(dir);
                }
            }
        };
        if let Some(shell_path) = login_shell_path(ctx) {
            add(&shell_path);
        }
        if let Some(inherited) = ctx.var("PATH") {
            add(inherited);
        }
        for relative in [
            ".local/bin",
            ".cargo/bin",
            ".bun/bin",
            ".npm-global/bin",
            ".volta/bin",
            ".opencode/bin",
            ".grok/bin",
        ] {
            add(&ctx.home.join(relative).to_string_lossy());
        }
        add("/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin");
        std::env::join_paths(dirs)
            .ok()
            .and_then(|joined| joined.into_string().ok())
            .unwrap_or_default()
    })
}

fn login_shell_path(ctx: &Ctx) -> Option<String> {
    const START: &str = "__AIOM_PATH__";
    const END: &str = "__AIOM_END__";
    let shell = ctx.var("SHELL").unwrap_or("/bin/zsh");
    let mut child = Command::new(shell)
        .args(["-lic", "printf '__AIOM_PATH__%s__AIOM_END__' \"$PATH\""])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // A shell profile that waits for input must not hang the app.
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(40)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    let start = out.find(START)? + START.len();
    let end = start + out[start..].find(END)?;
    Some(out[start..end].to_string())
}

fn gui_ctx() -> Result<Ctx> {
    let mut ctx = Ctx::from_env()?;
    let path = full_path(&ctx).to_string();
    ctx.set_var("PATH", &path);
    Ok(ctx)
}

/// Run one request against a fresh context. Errors become plain text for the UI.
fn run<T>(f: impl FnOnce(&Ctx, &[Provider]) -> Result<T>) -> Result<T, String> {
    let ctx = gui_ctx().map_err(|e| format!("{e:#}"))?;
    provider::load_all(&ctx)
        .and_then(|providers| f(&ctx, &providers))
        .map_err(|e| ctx.tilde(&format!("{e:#}")))
}

// ---- data sent to the UI -----------------------------------------------------

#[derive(Serialize)]
struct AccountView {
    name: String,
    identity: Option<String>,
    /// Friendlier name for the identity (an email), when known.
    label: Option<String>,
    note: Option<String>,
    updated_at: u64,
    active: bool,
}

#[derive(Serialize)]
struct ArtifactView {
    location: String,
    primary: bool,
    present: bool,
}

#[derive(Serialize)]
struct ProviderView {
    id: String,
    name: String,
    builtin: bool,
    cli: Option<String>,
    cli_installed: bool,
    logged_in: bool,
    live_identity: Option<String>,
    /// Friendlier name for the live account (an email), when known.
    live_label: Option<String>,
    active: Option<String>,
    /// Whether the live login still belongs to the active account.
    matches_active: bool,
    accounts: Vec<AccountView>,
    artifacts: Vec<ArtifactView>,
    login_command: Option<String>,
    /// Shell command that installs the CLI, when one is known.
    install: Option<String>,
    /// Whether "restore" has anything to put back.
    has_backup: bool,
    /// Name of the desktop app this provider's login belongs to, if any.
    app: Option<String>,
    /// Whether that app is running now.
    app_running: bool,
}

#[derive(Serialize, Default)]
struct Outcome {
    message: String,
    warnings: Vec<String>,
}

impl Outcome {
    fn new(message: String) -> Self {
        Outcome {
            message,
            warnings: Vec::new(),
        }
    }
}

fn provider_view(ctx: &Ctx, p: &Provider) -> Result<ProviderView> {
    let st = ops::status(ctx, p)?;
    let active = st.active.as_ref().map(|m| m.name.clone());
    Ok(ProviderView {
        id: p.id.clone(),
        name: p.name.clone(),
        builtin: p.builtin,
        cli: p.binary.clone(),
        cli_installed: st.binary.is_some(),
        logged_in: st.logged_in,
        live_identity: st.live_identity,
        live_label: st.live_label,
        matches_active: st.matches_active,
        accounts: st
            .accounts
            .iter()
            .map(|m| AccountView {
                name: m.name.clone(),
                identity: m.identity.clone(),
                label: m.label.clone(),
                note: m.note.clone(),
                updated_at: m.updated_at,
                active: active.as_deref() == Some(m.name.as_str()),
            })
            .collect(),
        active,
        artifacts: p
            .artifacts
            .iter()
            .map(|a| ArtifactView {
                location: ctx.tilde(&a.describe()),
                primary: a.primary,
                present: a.exists_live(ctx),
            })
            .collect(),
        login_command: p.login.as_ref().map(|cmd| cmd.join(" ")),
        install: p.install.clone(),
        has_backup: ops::has_backup(ctx, p),
        app: p.app.as_ref().map(|a| a.name.clone()),
        app_running: p
            .app
            .as_ref()
            .map(|a| ctx.apps.is_running(&a.bundle_id).unwrap_or(false))
            .unwrap_or(false),
    })
}

fn identity_suffix(identity: &Option<String>) -> String {
    identity
        .as_ref()
        .map(|i| format!("（{i}）"))
        .unwrap_or_default()
}

fn switch_outcome(ctx: &Ctx, p: &Provider, report: ops::SwitchReport) -> Outcome {
    let who = identity_suffix(&report.label.clone().or_else(|| report.identity.clone()));
    let message = if report.already_active {
        format!("{} 已經在使用「{}」{who}", p.name, report.to)
    } else {
        let mut text = format!("{} 已切換到「{}」{who}", p.name, report.to);
        if let (true, Some(from)) = (report.synced, &report.from) {
            text.push_str(&format!("，並先保存了「{from}」的最新憑證"));
        }
        match (&report.restarted_app, &p.app) {
            (Some(app), _) => text.push_str(&format!("。{app} 已關閉並重新開啟。")),
            (None, Some(_)) => text.push('。'),
            (None, None) => text.push_str(&format!("。請重新啟動執行中的 {}。", p.name)),
        }
        text
    };
    Outcome {
        message,
        warnings: report.warnings.iter().map(|w| ctx.tilde(w)).collect(),
    }
}

// ---- commands ----------------------------------------------------------------

#[derive(Serialize)]
struct Overview {
    providers: Vec<ProviderView>,
    /// What was saved automatically while looking.
    notices: Vec<String>,
    /// Automatic saves that failed.
    warnings: Vec<String>,
}

/// Give saved accounts that have no email yet the one stored in their
/// snapshot. Each account (as of its last save) is tried once per launch, so
/// an account whose snapshot holds no readable email is not re-read on every
/// refresh. Failures only mean the account keeps showing its id.
fn backfill_labels(ctx: &Ctx, providers: &[Provider]) {
    static TRIED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    for p in providers.iter().filter(|p| !p.label.is_empty()) {
        let Ok(accounts) = store::list(ctx, &p.id) else { continue };
        for meta in accounts.iter().filter(|m| m.label.is_none()) {
            let key = format!("{}/{}/{}", p.id, meta.name, meta.updated_at);
            {
                let mut tried = TRIED.lock().unwrap_or_else(|e| e.into_inner());
                if tried.contains(&key) {
                    continue;
                }
                tried.push(key);
            }
            let _ = ops::backfill_label(ctx, p, &meta.name);
        }
    }
}

/// The state of every provider. Any live login that is not saved yet is saved
/// on the way, so a login made in a terminal shows up as an account by itself.
#[tauri::command(async)]
fn overview() -> Result<Overview, String> {
    run(|ctx, providers| {
        let mut notices = Vec::new();
        let mut warnings = Vec::new();
        backfill_labels(ctx, providers);
        for p in providers {
            match ops::adopt(ctx, p) {
                Ok(Some(adopted)) => {
                    let who = identity_suffix(&adopted.label.clone().or_else(|| adopted.identity.clone()));
                    notices.push(if adopted.created {
                        format!(
                            "已自動將 {} 目前的登入儲存為帳號「{}」{who}",
                            p.name, adopted.name
                        )
                    } else {
                        format!(
                            "{} 目前登入的是已儲存的帳號「{}」{who}，已設為使用中",
                            p.name, adopted.name
                        )
                    });
                }
                Ok(None) => {}
                Err(err) => warnings.push(ctx.tilde(&format!(
                    "無法自動儲存 {} 的登入：{err:#}",
                    p.name
                ))),
            }
        }
        let providers = providers
            .iter()
            .map(|p| provider_view(ctx, p))
            .collect::<Result<Vec<_>>>()?;
        Ok(Overview {
            providers,
            notices,
            warnings,
        })
    })
}

#[tauri::command(async)]
fn save_login(
    provider: String,
    name: String,
    note: Option<String>,
    force: bool,
) -> Result<Outcome, String> {
    run(|ctx, providers| {
        let p = provider::find(providers, &provider)?;
        let note = note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
        let meta = ops::save(ctx, p, name.trim(), force, note)?;
        Ok(Outcome::new(format!(
            "已將目前的 {} 登入儲存為「{}」{}",
            p.name,
            meta.name,
            identity_suffix(&meta.shown().map(str::to_string))
        )))
    })
}

#[tauri::command(async)]
fn switch_account(provider: String, name: String, force: bool) -> Result<Outcome, String> {
    run(|ctx, providers| {
        let p = provider::find(providers, &provider)?;
        let report = ops::switch(ctx, p, &name, force)?;
        Ok(switch_outcome(ctx, p, report))
    })
}

#[tauri::command(async)]
fn next_account(provider: String) -> Result<Outcome, String> {
    run(|ctx, providers| {
        let p = provider::find(providers, &provider)?;
        let name = ops::next_account(ctx, p)?;
        let report = ops::switch(ctx, p, &name, false)?;
        Ok(switch_outcome(ctx, p, report))
    })
}

#[tauri::command(async)]
fn sync_account(provider: String, force: bool) -> Result<Outcome, String> {
    run(|ctx, providers| {
        let p = provider::find(providers, &provider)?;
        let meta = ops::sync(ctx, p, force)?;
        Ok(Outcome::new(format!(
            "已用目前的登入更新 {} 帳號「{}」的存檔",
            p.name, meta.name
        )))
    })
}

#[tauri::command(async)]
fn rename_account(provider: String, from: String, to: String) -> Result<Outcome, String> {
    run(|ctx, providers| {
        let p = provider::find(providers, &provider)?;
        ops::rename(ctx, p, &from, to.trim())?;
        Ok(Outcome::new(format!(
            "已將 {} 帳號「{from}」改名為「{}」",
            p.name,
            to.trim()
        )))
    })
}

#[tauri::command(async)]
fn remove_account(provider: String, name: String) -> Result<Outcome, String> {
    run(|ctx, providers| {
        let p = provider::find(providers, &provider)?;
        ops::remove(ctx, p, &name)?;
        Ok(Outcome::new(format!("已刪除 {} 帳號「{name}」的存檔", p.name)))
    })
}

#[tauri::command(async)]
fn restore_login(provider: String) -> Result<Outcome, String> {
    run(|ctx, providers| {
        let p = provider::find(providers, &provider)?;
        let (_, identity) = ops::restore(ctx, p)?;
        Ok(Outcome::new(format!(
            "已還原 {} 上一次切換前的登入{}。請重新啟動執行中的 {}。",
            p.name,
            identity_suffix(&identity),
            p.name
        )))
    })
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The script a Terminal window runs for a login: this executable in CLI
/// mode, with the PATH the app worked out, then a line saying how it went.
fn login_script(path: &str, exe: &str, p: &Provider, name: &str, force: bool) -> String {
    format!(
        "#!/bin/zsh\nexport PATH={path}\nclear\necho {title}\necho\n{exe} login {id} {name}{force}\nresult=$?\necho\nif [ $result -eq 0 ]; then\n  echo {done}\nelse\n  echo {failed}\nfi\n",
        path = shell_quote(path),
        title = shell_quote(&format!("AIOM：登入 {} 的新帳號「{name}」", p.name)),
        exe = shell_quote(exe),
        id = shell_quote(&p.id),
        name = shell_quote(name),
        force = if force { " --force" } else { "" },
        done = shell_quote("完成。可以關閉這個視窗，回到 AIOM。"),
        failed = shell_quote("登入沒有完成，原本的登入已保留。可以關閉這個視窗。"),
    )
}

/// A provider's login is interactive (it prints a URL, waits for a browser,
/// sometimes asks questions), so it runs in a Terminal window: write a small
/// script that calls this same executable in CLI mode and open it.
#[tauri::command(async)]
fn start_login(provider: String, name: String, force: bool) -> Result<Outcome, String> {
    run(|ctx, providers| {
        let p = provider::find(providers, &provider)?;
        let name = name.trim();
        if let (Some(app), None) = (&p.app, &p.login) {
            // The login happens inside the app: sign it out and let the user
            // sign in there; the window saves the new login when it appears.
            let warnings = ops::app_logout(ctx, p, Some(name))?;
            return Ok(Outcome {
                message: format!(
                    "已備份目前的登入並登出 {app}，{app} 已重新開啟。請在 {app} 裡登入另一個帳號，完成後回到 AIOM，會自動存成「{name}」。",
                    app = app.name
                ),
                warnings: warnings.iter().map(|w| ctx.tilde(w)).collect(),
            });
        }
        store::validate_name(name)?;
        if store::load_meta(ctx, &p.id, name)?.is_some() && !force {
            bail!("{} account '{name}' already exists", p.name);
        }
        let cmd = p
            .login
            .as_ref()
            .ok_or_else(|| anyhow!("{} has no login command configured", p.name))?;
        if util::find_in_path(&cmd[0], ctx.var("PATH")).is_none() {
            bail!("'{}' was not found; is {} installed?", cmd[0], p.name);
        }
        let exe = std::env::current_exe().context("locating the AIOM executable")?;

        let dir = ctx.data_dir.join("tmp");
        util::ensure_private_dir(&ctx.data_dir)?;
        util::ensure_private_dir(&dir)?;
        let script = dir.join(format!("login-{}.command", p.id));
        let body = login_script(
            ctx.var("PATH").unwrap_or("/usr/bin:/bin"),
            &exe.to_string_lossy(),
            p,
            name,
            force,
        );
        util::atomic_write(&script, body.as_bytes())?;
        util::set_mode(&script, 0o700)?;
        let opened = Command::new("open")
            .arg(&script)
            .status()
            .context("opening Terminal")?;
        if !opened.success() {
            bail!("could not open a Terminal window for the login");
        }
        Ok(Outcome::new(format!(
            "已在終端機視窗開始 {} 的登入流程。完成後回到這裡，帳號「{name}」會自動出現。",
            p.name
        )))
    })
}

/// Self-test hook: with `AIOM_SELFTEST` set, print what the window rendered
/// on first load and quit. That proves the whole chain (window, embedded
/// page, script, commands, real data) without anyone looking at the screen.
#[tauri::command]
fn ui_ready(summary: String) {
    if std::env::var_os("AIOM_SELFTEST").is_some() {
        println!("{summary}");
        std::process::exit(if summary.starts_with("ok:") { 0 } else { 1 });
    }
}

// ---- entry point -------------------------------------------------------------

fn run_cli(args: Vec<String>) -> ! {
    use clap::Parser;
    let cli = aiom::cli::Cli::parse_from(args);
    let code = match Ctx::from_env() {
        Ok(ctx) => match aiom::cli::run(cli, &ctx) {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("error: {}", ctx.tilde(&format!("{err:#}")));
                1
            }
        },
        Err(err) => {
            eprintln!("error: {err:#}");
            1
        }
    };
    std::process::exit(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codex() -> Provider {
        let home = std::env::temp_dir();
        let ctx = Ctx::for_test(&home, false);
        let all = provider::builtins(&ctx);
        provider::find(&all, "codex").unwrap().clone()
    }

    /// Run the generated script with a stand-in executable that prints its
    /// arguments one per line, so quoting mistakes show up as wrong arguments.
    fn run_script(exe_body: &str, name: &str, force: bool) -> (bool, String) {
        let dir = std::env::temp_dir().join(format!("aiom app test {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(format!("fake exe {}", name.len()));
        std::fs::write(&exe, exe_body).unwrap();
        util::set_mode(&exe, 0o755).unwrap();
        let script = dir.join(format!("login {}.command", name.len()));
        let body = login_script("/usr/bin:/bin", &exe.to_string_lossy(), &codex(), name, force);
        std::fs::write(&script, &body).unwrap();
        let out = Command::new("/bin/zsh")
            .arg(&script)
            .env("TERM", "dumb")
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    #[test]
    fn login_script_passes_arguments_intact() {
        let echo_args = "#!/bin/sh\nfor a in \"$@\"; do echo \"arg=$a\"; done\n";
        let (ok, out) = run_script(echo_args, "me@home.io", false);
        assert!(ok);
        assert!(out.contains("arg=login\narg=codex\narg=me@home.io\n"), "{out}");
        assert!(!out.contains("--force"));
        assert!(out.contains("完成。"), "{out}");

        let (_, out) = run_script(echo_args, "work", true);
        assert!(out.contains("arg=login\narg=codex\narg=work\narg=--force\n"), "{out}");
    }

    #[test]
    fn login_script_reports_failure() {
        let (ok, out) = run_script("#!/bin/sh\nexit 3\n", "x", false);
        assert!(ok, "the script itself still ends cleanly");
        assert!(out.contains("登入沒有完成"), "{out}");
    }

    #[test]
    fn shell_quote_survives_single_quotes() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        let out = Command::new("/bin/sh")
            .args(["-c", &format!("printf %s {}", shell_quote("a 'b' $HOME \"c\""))])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "a 'b' $HOME \"c\"");
    }
}

fn main() {
    // macOS may pass a `-psn_…` process serial number when launching from Finder.
    let args: Vec<String> = std::env::args()
        .filter(|a| !a.starts_with("-psn_"))
        .collect();
    if args.len() > 1 {
        run_cli(args);
    }

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            overview,
            save_login,
            switch_account,
            next_account,
            sync_account,
            rename_account,
            remove_account,
            restore_login,
            start_login,
            ui_ready,
        ])
        .run(tauri::generate_context!())
        .expect("failed to start AIOM");
}
