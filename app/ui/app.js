// AIOM window. All state lives in the Rust side; this file asks for an
// overview, draws it, and sends the user's actions back.

const call = window.__TAURI__
  ? (command, args) => window.__TAURI__.core.invoke(command, args)
  : window.aiomMock; // opened in a plain browser: demo data, nothing is touched

const state = { providers: [], selected: null, busy: false };

const $ = (id) => document.getElementById(id);

function esc(value) {
  return String(value ?? '').replace(/[&<>"']/g, (c) => (
    { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]
  ));
}

function when(seconds) {
  return new Date(seconds * 1000).toLocaleString('zh-TW', { dateStyle: 'medium', timeStyle: 'short' });
}

function current() {
  return state.providers.find((p) => p.id === state.selected) || state.providers[0] || null;
}

// ---- toasts ----

function toast(text, kind = 'ok') {
  const el = document.createElement('div');
  el.className = `toast ${kind}`;
  el.innerHTML = `<span class="selectable">${esc(text)}</span><button type="button" aria-label="關閉">×</button>`;
  const close = () => el.remove();
  el.querySelector('button').addEventListener('click', close);
  $('toasts').append(el);
  setTimeout(close, kind === 'ok' ? 6000 : 14000);
}

function report(outcome) {
  (outcome.warnings || []).forEach((w) => toast(w, 'warn'));
  toast(outcome.message, 'ok');
}

// ---- dialog ----

// Shows a modal with optional text fields. Resolves to the field values, or
// null when cancelled. `validate` may return an error string to keep it open.
function ask({ title, body = '', fields = [], ok = '確定', danger = false, validate }) {
  const dialog = $('dialog');
  $('dialog-title').textContent = title;
  $('dialog-body').textContent = body;
  $('dialog-body').hidden = !body;
  $('dialog-error').hidden = true;
  $('dialog-fields').innerHTML = fields.map((f) => `
    <label>${esc(f.label)}
      <input name="${esc(f.key)}" value="${esc(f.value || '')}" placeholder="${esc(f.placeholder || '')}"
        autocomplete="off" autocapitalize="off" autocorrect="off" spellcheck="false">
    </label>`).join('');
  const okButton = $('dialog-ok');
  okButton.textContent = ok;
  okButton.className = danger ? 'danger' : 'primary';

  return new Promise((resolve) => {
    const form = $('dialog-form');
    const finish = (value) => {
      form.removeEventListener('submit', onSubmit);
      $('dialog-cancel').removeEventListener('click', onCancel);
      dialog.removeEventListener('cancel', onCancel);
      if (dialog.open) dialog.close();
      resolve(value);
    };
    const onCancel = (event) => {
      event.preventDefault();
      finish(null);
    };
    const onSubmit = (event) => {
      event.preventDefault();
      const values = Object.fromEntries(new FormData(form).entries());
      const problem = validate ? validate(values) : null;
      if (problem) {
        $('dialog-error').textContent = problem;
        $('dialog-error').hidden = false;
        return;
      }
      finish(values);
    };
    form.addEventListener('submit', onSubmit);
    $('dialog-fields').oninput = () => { $('dialog-error').hidden = true; };
    $('dialog-cancel').addEventListener('click', onCancel);
    dialog.addEventListener('cancel', onCancel);
    dialog.showModal();
    const first = dialog.querySelector('input');
    if (first) {
      first.focus();
      first.select();
    }
  });
}

const NAME_RULE = /^[A-Za-z0-9][A-Za-z0-9._@+-]{0,63}$/;

function checkName(values) {
  const name = (values.name || '').trim();
  if (!name) return '請輸入帳號名稱。';
  if (!NAME_RULE.test(name)) return '名稱只能用英數字與 . _ - @ +，並以英數字開頭。';
  return null;
}

// ---- drawing ----

// Prefer the label (an email) when the backend found one; show an email
// identity as it is, and shorten an opaque account id (a UUID).
function showIdentity(identity, label) {
  if (label) return label;
  if (!identity) return '';
  return identity.includes('@') ? identity : `帳號 ID ${identity.slice(0, 8)}…`;
}

function providerSummary(p) {
  if (p.logged_in && p.active && p.matches_active) return { dot: 'ok', text: p.active };
  if (p.logged_in && p.active) return { dot: 'warn', text: '登入與帳號不一致' };
  if (p.logged_in) return { dot: 'warn', text: '已登入，尚未儲存' };
  if (p.accounts.length) return { dot: '', text: '未登入' };
  return { dot: '', text: p.cli_installed ? '未登入' : '未安裝' };
}

function drawSidebar() {
  const selected = current();
  $('providers').innerHTML = state.providers.map((p) => {
    const summary = providerSummary(p);
    return `
      <button type="button" class="provider" data-provider="${esc(p.id)}"
        aria-current="${selected && selected.id === p.id}">
        <span class="dot ${summary.dot}"></span>
        <span class="label"><b>${esc(p.name)}</b><span>${esc(summary.text)}</span></span>
        <span class="count" title="已儲存的帳號數">${p.accounts.length}</span>
      </button>`;
  }).join('');
}

function drawLive(p) {
  const identity = esc(showIdentity(p.live_identity, p.live_label) || '已登入');
  if (!p.logged_in) {
    const how = p.login_command ? `，或在終端機執行 <code>${esc(p.login_command)}</code>` : '';
    let hint;
    if (p.app_login) {
      hint = p.cli_installed
        ? `在 ${esc(p.app)} 裡登入後回到這裡，AIOM 會自動存成帳號。`
        : `這台電腦找不到 ${esc(p.app)} App。`;
    } else {
      hint = p.cli_installed
        ? `按「登入新帳號」開始${how}。`
        : `這台電腦還沒有安裝 ${esc(p.name)} 的命令列工具，安裝後就能在這裡登入與管理帳號。`;
    }
    return `
      <section class="card live">
        <div class="who">
          <div class="eyebrow">目前登入</div>
          <div class="identity">尚未登入</div>
          <div class="hint">${hint}</div>
        </div>
      </section>`;
  }
  if (p.active && p.matches_active) {
    return `
      <section class="card live">
        <div class="who">
          <div class="eyebrow">目前登入</div>
          <div class="identity selectable">${identity}</div>
          <div class="hint">使用中的帳號：${esc(p.active)}</div>
        </div>
      </section>`;
  }
  const hint = p.active
    ? `這個登入和使用中的帳號「${esc(p.active)}」不是同一個身分。切換帳號前，建議先把它存成另一個帳號。`
    : '這個登入還沒有存成帳號，存起來之後才能在帳號之間切換。';
  return `
    <section class="card live warn">
      <div class="who">
        <div class="eyebrow">目前登入</div>
        <div class="identity selectable">${identity}</div>
        <div class="hint">${hint}</div>
      </div>
      <button type="button" class="primary" data-action="save">儲存為帳號…</button>
    </section>`;
}

function drawAccounts(p) {
  if (!p.accounts.length) {
    return '<div class="card none">還沒有儲存任何帳號。</div>';
  }
  const rows = p.accounts.map((a) => {
    const meta = [showIdentity(a.identity, a.label), a.note, `存於 ${when(a.updated_at)}`].filter(Boolean).map(esc).join(' · ');
    const badge = a.active
      ? (p.matches_active
        ? '<span class="chip ok">使用中</span>'
        : '<span class="chip warn">使用中（登入不一致）</span>')
      : '';
    const reapply = a.active && !p.matches_active;
    const switchButton = a.active && !reapply
      ? ''
      : `<button type="button" class="${reapply ? 'secondary' : 'primary'}" data-action="switch" data-name="${esc(a.name)}"
          ${reapply ? 'data-force="1"' : ''}>${reapply ? '重新套用' : '切換'}</button>`;
    return `
      <div class="account ${a.active ? 'active' : ''}">
        <div>
          <div class="title"><span class="selectable">${esc(a.name)}</span>${badge}</div>
          <div class="meta">${meta}</div>
        </div>
        <div class="actions">
          ${switchButton}
          <button type="button" class="ghost" data-action="rename" data-name="${esc(a.name)}">改名</button>
          <button type="button" class="ghost" data-action="remove" data-name="${esc(a.name)}">刪除</button>
        </div>
      </div>`;
  }).join('');
  return `<div class="card accounts">${rows}</div>`;
}

// Things worth knowing about one provider that the backend cannot tell.
const PROVIDER_NOTES = {
  claude: '這裡管理的是 Claude Code 命令列工具（claude 指令）的登入。Claude 桌面 App 的帳號請到左側的「Claude Desktop」管理。',
};

// Why each unavailable button is unavailable, in the order the buttons appear.
// A greyed-out button with no explanation just looks broken.
function disabledReasons(p) {
  const reasons = [];
  if (p.app_login && !p.cli_installed) {
    reasons.push(`<b>登入新帳號</b>：這台電腦找不到 ${esc(p.app)} App。`);
  } else if (p.app_login) {
    // Logging in happens inside the app, so the button is always usable.
  } else if (!p.cli_installed) {
    const install = p.install
      ? `在終端機執行 <code class="selectable">${esc(p.install)}</code> 安裝`
      : `依 ${esc(p.name)} 的官方說明安裝`;
    reasons.push(`<b>登入新帳號</b>：找不到 <code>${esc(p.cli || 'CLI')}</code> 指令。${install}，然後按左下角的「重新整理」。`);
  } else if (!p.login_command) {
    reasons.push('<b>登入新帳號</b>：這個 provider 沒有設定登入指令。');
  }
  if (!p.logged_in) {
    reasons.push('<b>儲存目前登入、更新存檔</b>：目前沒有登入，沒有東西可以儲存。');
  } else if (!p.active) {
    reasons.push('<b>更新存檔</b>：目前的登入還沒有對應的帳號。');
  }
  if (p.accounts.length < 2) {
    reasons.push(`<b>切換到下一個</b>：要有兩個以上已儲存的帳號（目前 ${p.accounts.length} 個）。`);
  }
  if (!p.has_backup) {
    reasons.push('<b>還原上次切換前</b>：還沒有切換過，沒有可還原的備份。');
  }
  if (!p.cli_installed && PROVIDER_NOTES[p.id]) {
    reasons.push(esc(PROVIDER_NOTES[p.id]));
  }
  return reasons;
}

function drawMain() {
  const p = current();
  if (!p) {
    $('main').innerHTML = '<p class="empty">沒有可用的 provider。</p>';
    return;
  }
  const appChip = `<span class="chip ok">${esc(p.app)} App ${p.app_running ? '執行中' : '已安裝'}</span>`;
  let cliChip;
  if (p.app_login) {
    cliChip = p.cli_installed ? appChip : `<span class="chip warn">找不到 ${esc(p.app)} App</span>`;
  } else {
    cliChip = p.cli_installed
      ? '<span class="chip ok">CLI 已安裝</span>'
      : `<span class="chip warn">找不到 ${esc(p.cli || 'CLI')} 指令</span>`;
    // An app that shares the CLI's login (the Codex app) is shown only when
    // it is installed; it is optional.
    if (p.app && p.app_installed) cliChip += appChip;
  }
  const customChip = p.builtin ? '' : '<span class="chip">自訂</span>';
  const canSave = p.logged_in;
  const canLogin = p.cli_installed && (p.app_login || p.login_command);
  const canSync = p.logged_in && p.active;
  const reasons = disabledReasons(p);
  const locations = p.artifacts.map((a) => `
    <li>
      <span class="state ${a.present ? 'present' : ''}">${a.present ? '存在' : '不存在'}</span>
      <span class="mono selectable">${esc(a.location)}</span>
    </li>`).join('');

  $('main').innerHTML = `
    <header class="head">
      <h1>${esc(p.name)}</h1>
      ${cliChip}${customChip}
    </header>
    ${drawLive(p)}
    <div class="toolbar">
      <button type="button" class="secondary" data-action="login" ${canLogin ? '' : 'disabled'}
        title="${canLogin ? '在終端機視窗執行登入流程，完成後自動存成帳號' : '需要先安裝這個 CLI'}">登入新帳號…</button>
      <button type="button" class="secondary" data-action="save" ${canSave ? '' : 'disabled'}>儲存目前登入…</button>
      <button type="button" class="secondary" data-action="next" ${p.accounts.length > 1 ? '' : 'disabled'}>切換到下一個</button>
      <button type="button" class="ghost" data-action="sync" ${canSync ? '' : 'disabled'}
        title="把目前登入（含更新過的憑證）存回使用中的帳號">更新存檔</button>
      <button type="button" class="ghost" data-action="restore" ${p.has_backup ? '' : 'disabled'}
        title="把登入還原成上一次切換之前的狀態">還原上次切換前</button>
    </div>
    ${reasons.length ? `
    <aside class="why">
      <h2>為什麼有些按鈕不能按</h2>
      <ul>${reasons.map((r) => `<li>${r}</li>`).join('')}</ul>
    </aside>` : ''}
    <h2 class="section">已儲存的帳號</h2>
    ${drawAccounts(p)}
    <details class="locations">
      <summary>AIOM 為 ${esc(p.name)} 管理的憑證位置</summary>
      <ul class="card">${locations}</ul>
    </details>`;
}

function draw() {
  drawSidebar();
  drawMain();
}

// ---- actions ----

async function refresh({ quiet = false } = {}) {
  try {
    const result = await call('overview');
    state.providers = result.providers;
    (result.warnings || []).forEach((w) => toast(w, 'warn'));
    (result.notices || []).forEach((n) => toast(n, 'ok'));
    if (!state.selected || !state.providers.some((p) => p.id === state.selected)) {
      const preferred = state.providers.find((p) => p.logged_in || p.accounts.length);
      state.selected = (preferred || state.providers[0] || {}).id || null;
    }
    // Keep an open "locations" section open across redraws.
    const open = Boolean(document.querySelector('details.locations[open]'));
    draw();
    if (open) document.querySelector('details.locations')?.setAttribute('open', '');
    const unavailable = state.providers
      .map((p) => `${p.id}=${disabledReasons(p).length}${p.install ? '' : '(no install hint)'}`)
      .join(' ');
    announceReady(`ok: ${state.providers.length} providers | unavailable-button notes: ${unavailable} | ${$('main').innerText.replace(/\s+/g, ' ').slice(0, 300)}`);
  } catch (error) {
    if (!quiet) toast(String(error), 'error');
    announceReady(`error: ${error}`);
  }
}

// Tell the backend once what the first render looked like. It ignores this
// unless the app was started in self-test mode.
let announced = false;
function announceReady(summary) {
  if (announced) return;
  announced = true;
  call('ui_ready', { summary }).catch(() => {});
}

async function perform(command, args) {
  if (state.busy) return;
  state.busy = true;
  document.body.style.cursor = 'progress';
  try {
    report(await call(command, args));
  } catch (error) {
    toast(String(error), 'error');
  } finally {
    state.busy = false;
    document.body.style.cursor = '';
    await refresh();
  }
}

function suggestName(p) {
  const local = (p.live_label || p.live_identity || '').split('@')[0].replace(/[^A-Za-z0-9._+-]/g, '');
  return NAME_RULE.test(local) ? local : '';
}

// A desktop app has to be closed while its login is swapped. Say so first,
// because whatever is going on in it (including this kind of chat) stops.
async function confirmAppRestart(p, what) {
  if (!p.app || !p.app_running) return true;
  const sessions = p.id === 'claude-desktop' ? `（包括在 ${p.app} 裡執行的 Claude Code 工作階段）` : '';
  return Boolean(await ask({
    title: `${what}需要先關閉 ${p.app}`,
    body: `AIOM 會關閉 ${p.app}、替換登入資料，完成後自動重新開啟。${p.app} 裡進行中的對話與工作${sessions}會被中斷，重新開啟後可以從側邊欄接回。`,
    ok: `關閉 ${p.app} 並切換`,
  }));
}

const actions = {
  async save(p) {
    const values = await ask({
      title: `儲存目前的 ${p.name} 登入`,
      body: p.live_identity ? `目前登入：${showIdentity(p.live_identity, p.live_label)}` : '',
      fields: [
        { key: 'name', label: '帳號名稱', value: suggestName(p), placeholder: '例如 work 或 personal' },
        { key: 'note', label: '備註（選填）', placeholder: '例如 公司帳號' },
      ],
      ok: '儲存',
      validate: checkName,
    });
    if (!values) return;
    const name = values.name.trim();
    let force = false;
    if (p.accounts.some((a) => a.name === name)) {
      const overwrite = await ask({
        title: `覆寫帳號「${name}」？`,
        body: '已經有同名的帳號，它的存檔會被目前的登入取代。',
        ok: '覆寫',
        danger: true,
      });
      if (!overwrite) return;
      force = true;
    }
    await perform('save_login', { provider: p.id, name, note: values.note || null, force });
  },

  async login(p) {
    const closes = p.app && p.app_running
      ? `${p.app} 會先關閉，登入完成後重新開啟；${p.app} 裡進行中的對話與工作會被中斷。`
      : '';
    const body = p.app_login
      ? `AIOM 會關閉 ${p.app}、備份目前的登入並登出，再重新開啟 ${p.app}。請在 ${p.app} 裡登入另一個帳號，回到 AIOM 後會自動存檔。${p.app} 裡進行中的對話與工作會被中斷。`
      : `會開啟終端機視窗執行「${p.login_command}」。目前的登入會先備份，登入成功後新帳號會自動存檔；沒完成的話會還原。${closes}`;
    const values = await ask({
      title: `登入 ${p.name} 的新帳號`,
      body,
      fields: [{ key: 'name', label: '新帳號的名稱', placeholder: '例如 personal' }],
      ok: '開始登入',
      validate: (v) => checkName(v)
        || (p.accounts.some((a) => a.name === v.name.trim()) ? '已經有同名的帳號，請換一個名稱。' : null),
    });
    if (!values) return;
    await perform('start_login', { provider: p.id, name: values.name.trim(), force: false });
  },

  async switch(p, { name, force }) {
    if (!(await confirmAppRestart(p, `切換到「${name}」`))) return;
    await perform('switch_account', { provider: p.id, name, force: Boolean(force) });
  },

  async next(p) {
    if (!(await confirmAppRestart(p, '切換到下一個帳號'))) return;
    await perform('next_account', { provider: p.id });
  },

  async sync(p) {
    await perform('sync_account', { provider: p.id, force: false });
  },

  async rename(p, { name }) {
    const values = await ask({
      title: `將「${name}」改名`,
      fields: [{ key: 'name', label: '新的名稱', value: name }],
      ok: '改名',
      validate: (v) => checkName(v)
        || (v.name.trim() !== name && p.accounts.some((a) => a.name === v.name.trim()) ? '已經有同名的帳號。' : null),
    });
    if (!values || values.name.trim() === name) return;
    await perform('rename_account', { provider: p.id, from: name, to: values.name.trim() });
  },

  async remove(p, { name }) {
    const sure = await ask({
      title: `刪除帳號「${name}」？`,
      body: '只會刪除 AIOM 裡的存檔，不會登出目前的登入。刪除後無法復原。',
      ok: '刪除',
      danger: true,
    });
    if (sure) await perform('remove_account', { provider: p.id, name });
  },

  async restore(p) {
    const closes = p.app && p.app_running
      ? `會先關閉 ${p.app}，完成後重新開啟；進行中的對話與工作會被中斷。`
      : '';
    const sure = await ask({
      title: `還原 ${p.name} 的登入？`,
      body: `登入會回到上一次切換之前的狀態，之後不會有使用中的帳號。${closes}`,
      ok: '還原',
    });
    if (sure) await perform('restore_login', { provider: p.id });
  },
};

document.addEventListener('click', (event) => {
  const providerButton = event.target.closest('[data-provider]');
  if (providerButton) {
    state.selected = providerButton.dataset.provider;
    draw();
    return;
  }
  const actionButton = event.target.closest('[data-action]');
  if (!actionButton || actionButton.disabled) return;
  const p = current();
  const action = actions[actionButton.dataset.action];
  if (p && action) action(p, actionButton.dataset);
});

$('refresh').addEventListener('click', () => refresh());

// Logins and switches also happen outside the app (in a terminal), so look
// again whenever the window comes back to the front.
window.addEventListener('focus', () => {
  if (!state.busy && !$('dialog').open) refresh({ quiet: true });
});

refresh();
