// Demo backend used only when the page is opened in an ordinary browser
// (no Tauri). It imitates the Rust commands with made-up data so the
// interface can be tried without touching any real login.

(() => {
  const now = Math.floor(Date.now() / 1000);
  const account = (name, identity, note, age, active = false) => ({
    name, identity, note, updated_at: now - age, active,
  });

  const providers = [
    {
      id: 'claude', name: 'Claude Code', builtin: true, cli: 'claude', cli_installed: true,
      logged_in: true, live_identity: 'me@home.example', active: 'personal', matches_active: true,
      accounts: [
        account('personal', 'me@home.example', null, 3600, true),
        account('work', 'dev@corp.example', '公司帳號', 86400 * 2),
      ],
      artifacts: [
        { location: 'keychain: Claude Code-credentials (demo)', primary: true, present: true },
        { location: '~/.claude/.credentials.json', primary: true, present: false },
        { location: '~/.claude.json [oauthAccount]', primary: false, present: true },
      ],
      login_command: 'claude auth login',
    },
    {
      id: 'codex', name: 'Codex CLI', builtin: true, cli: 'codex', cli_installed: true,
      logged_in: true, live_identity: 'someone@new.example', active: null, matches_active: false,
      accounts: [],
      artifacts: [{ location: '~/.codex/auth.json', primary: true, present: true }],
      login_command: 'codex login',
    },
    {
      id: 'gemini', name: 'Gemini CLI', builtin: true, cli: 'gemini', cli_installed: true,
      logged_in: true, live_identity: 'stranger@other.example', active: 'main', matches_active: false,
      accounts: [account('main', 'me@home.example', null, 86400 * 5, true)],
      artifacts: [
        { location: '~/.gemini/oauth_creds.json', primary: true, present: true },
        { location: '~/.gemini/google_accounts.json', primary: false, present: true },
        { location: '~/.gemini/.env', primary: true, present: false },
        { location: '~/.gemini/settings.json [security.auth, selectedAuthType]', primary: false, present: true },
      ],
      login_command: 'gemini',
    },
    {
      id: 'grok', name: 'Grok CLI', builtin: true, cli: 'grok', cli_installed: false,
      logged_in: false, live_identity: null, active: null, matches_active: false,
      accounts: [],
      artifacts: [{ location: '~/.grok/auth.json', primary: true, present: false }],
      login_command: 'grok login',
    },
    {
      id: 'opencode', name: 'OpenCode', builtin: true, cli: 'opencode', cli_installed: true,
      logged_in: false, live_identity: null, active: null, matches_active: false,
      accounts: [account('max', null, 'Claude Max', 86400), account('api', null, 'API key', 86400 * 9)],
      artifacts: [{ location: '~/.local/share/opencode/auth.json', primary: true, present: false }],
      login_command: 'opencode auth login',
    },
  ];

  const find = (id) => providers.find((p) => p.id === id);
  const done = (message, warnings = []) => ({ message, warnings });

  function activate(p, name) {
    const target = p.accounts.find((a) => a.name === name);
    if (!target) throw `${p.name} has no account named '${name}'`;
    p.accounts.forEach((a) => { a.active = a === target; });
    p.active = name;
    p.logged_in = true;
    p.matches_active = true;
    p.live_identity = target.identity;
    return done(`${p.name} 已切換到「${name}」。請重新啟動執行中的 ${p.name}。`);
  }

  const commands = {
    // The real backend saves unsaved logins automatically; the demo keeps one
    // unsaved so that state can be seen.
    overview: () => ({ providers: JSON.parse(JSON.stringify(providers)), notices: [], warnings: [] }),

    ui_ready: () => null,

    save_login({ provider, name, note }) {
      const p = find(provider);
      p.accounts = p.accounts.filter((a) => a.name !== name);
      p.accounts.push(account(name, p.live_identity, note, 0));
      p.accounts.sort((a, b) => a.name.localeCompare(b.name));
      activate(p, name);
      return done(`已將目前的 ${p.name} 登入儲存為「${name}」`);
    },

    switch_account: ({ provider, name }) => activate(find(provider), name),

    next_account({ provider }) {
      const p = find(provider);
      const index = p.accounts.findIndex((a) => a.active);
      return activate(p, p.accounts[(index + 1) % p.accounts.length].name);
    },

    sync_account({ provider }) {
      const p = find(provider);
      if (!p.matches_active) throw `the live ${p.name} login is ${p.live_identity} but the active account is someone else`;
      return done(`已用目前的登入更新 ${p.name} 帳號「${p.active}」的存檔`);
    },

    rename_account({ provider, from, to }) {
      const p = find(provider);
      p.accounts.find((a) => a.name === from).name = to;
      if (p.active === from) p.active = to;
      return done(`已將 ${p.name} 帳號「${from}」改名為「${to}」`);
    },

    remove_account({ provider, name }) {
      const p = find(provider);
      p.accounts = p.accounts.filter((a) => a.name !== name);
      if (p.active === name) { p.active = null; p.matches_active = false; }
      return done(`已刪除 ${p.name} 帳號「${name}」的存檔`);
    },

    restore_login({ provider }) {
      const p = find(provider);
      p.accounts.forEach((a) => { a.active = false; });
      p.active = null;
      p.matches_active = false;
      return done(`已還原 ${p.name} 上一次切換前的登入。`);
    },

    start_login({ provider, name }) {
      const p = find(provider);
      p.live_identity = `${name}@demo.example`;
      p.accounts.push(account(name, p.live_identity, null, 0));
      p.accounts.sort((a, b) => a.name.localeCompare(b.name));
      activate(p, name);
      return done(`（示範模式）已模擬 ${p.name} 的登入並存成「${name}」。`);
    },
  };

  window.aiomMock = (command, args = {}) => new Promise((resolve, reject) => {
    setTimeout(() => {
      try {
        resolve(commands[command](args));
      } catch (error) {
        reject(error);
      }
    }, 120);
  });
})();
