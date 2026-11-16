# aiom — AI All-in-One Manager

用 Rust 寫的 AI 程式助理 CLI 帳號管理工具。一個指令管理 **Claude Code、Codex、Gemini CLI、Grok、OpenCode**（以及任何你自行定義的工具）的多個帳號，並在它們之間切換。

各家 CLI 一次只能登入一個帳號。`aiom` 把「目前的登入狀態」存成具名帳號，之後可以隨時換回來，不必重新走一次瀏覽器登入流程。

有兩種使用方式，共用同一份核心與同一份帳號資料：

- **AIOM.app**：macOS 桌面 App（Rust + Tauri），用視窗管理與切換帳號。
- **`aiom`**：命令列工具，適合寫進腳本或在終端機快速切換。

## 桌面 App

需要 Rust 工具鏈（<https://rustup.rs>）與 Xcode Command Line Tools。

```bash
scripts/bundle-macos.sh
```

會產生 `dist/AIOM.app`，雙擊即可開啟。加上 `--install` 會另外複製一份到 `/Applications`：

```bash
scripts/bundle-macos.sh --install
```

App 的畫面：

- 左側列出所有 provider，圓點顯示狀態（綠：登入與使用中帳號一致；黃：已登入但尚未儲存，或登入的身分與使用中帳號不同）。
- 右側顯示目前登入的身分、已儲存的帳號清單，以及每個帳號的「切換／改名／刪除」。
- **自動儲存**：App 一看到還沒存過的登入就會自動存成帳號，名稱取自信箱 `@` 前面的部分（撞名時用完整信箱，再不行就加編號）。在終端機直接用 CLI 登入別的帳號，回到 App 也會自動多出一個帳號；登入的若是已存過的帳號，則直接設為使用中，不會重複建立。
- **儲存目前登入…**：用自訂的名稱與備註另存目前的登入。
- **登入新帳號…**：開啟終端機視窗執行該 CLI 自己的登入流程（例如 `codex login`），成功後自動存成新帳號，沒完成則還原原本的登入。
- **切換到下一個**、**更新存檔**、**還原上次切換前**：對應命令列的 `next`、`sync`、`restore`。
- 視窗回到前景時會自動重新整理，所以在終端機做的變更也會反映出來。
- 有按鈕不能按時，工具列下方會列出每個按鈕停用的原因。最常見的是該 CLI 還沒安裝，這時會一併顯示安裝指令。

注意：AIOM 管理的是各家**命令列工具**的登入。Claude 桌面 App 自己的登入是分開的，要管理 Claude 的帳號需要另外安裝 Claude Code CLI（`claude` 指令）。

App 從 Finder 啟動時會向登入 shell 詢問 `PATH`，因此裝在 `~/.local/bin`、Homebrew、npm、cargo 等位置的 CLI 都找得到。

想先看介面而不碰任何真實登入，可以用瀏覽器開啟示範模式（假資料）：

```bash
python3 -m http.server 4173 --directory app/ui
```

然後開啟 <http://localhost:4173>。

## 命令列工具

```bash
cargo install --path .
```

或只編譯不安裝，執行檔會在 `target/release/aiom`：

```bash
cargo build --release
```

## 快速上手

```bash
# 1. 先用原本的 CLI 正常登入（例如 codex login），然後存成帳號
aiom add codex work

# 2. 登入第二個帳號並存檔：aiom 會先備份目前登入、呼叫 codex login、成功後自動儲存
aiom login codex personal

# 3. 切換
aiom use codex work
aiom use codex            # 不給名稱會列出清單讓你選
aiom next codex           # 輪到下一個帳號（額度用完時很方便）

# 4. 總覽
aiom status
```

`aiom status` 會列出每個 provider 的 CLI 是否安裝、目前登入的身分、使用中的帳號與已儲存的帳號數：

```text
PROVIDER  CLI        LIVE LOGIN     ACTIVE ACCOUNT  SAVED
claude    installed  me@home.io     personal        2
codex     installed  work@corp.com  work            2
gemini    not found  not logged in  -               0
grok      not found  not logged in  -               0
opencode  installed  logged in      (not saved)     0
```

## 指令

| 指令 | 說明 |
| --- | --- |
| `aiom status` | 所有 provider 的總覽（不帶參數執行 `aiom` 也是這個） |
| `aiom providers` | 列出支援的 provider 以及各自管理的憑證位置 |
| `aiom list [provider]` | 列出已儲存的帳號（別名 `ls`） |
| `aiom add <provider> <name>` | 把目前的登入存成帳號（別名 `save`；`--force` 覆寫、`--note` 加註記） |
| `aiom adopt [provider]` | 把所有還沒存過的登入自動存成帳號，名稱取自登入身分（別名 `import`） |
| `aiom use <provider> [name]` | 切換到某個帳號（別名 `switch`） |
| `aiom next <provider>` | 切換到下一個帳號，循環 |
| `aiom login <provider> <name>` | 呼叫該 CLI 自己的登入流程登入新帳號並儲存；失敗會還原原本的登入 |
| `aiom current [provider]` | 顯示使用中的帳號 |
| `aiom sync [provider]` | 把目前登入（含刷新過的 token）更新回使用中帳號的存檔 |
| `aiom rename <provider> <old> <new>` | 重新命名帳號 |
| `aiom remove <provider> <name>` | 刪除帳號存檔，不影響目前登入（別名 `rm`；`-y` 略過確認） |
| `aiom restore <provider>` | 還原上一次切換前的登入狀態 |
| `aiom path` | 顯示 aiom 的資料目錄 |

provider 名稱可以只打唯一的前綴，例如 `aiom use cl work` 等同 `aiom use claude work`。

## 支援的 provider 與管理的憑證

| Provider | 管理的內容 | 路徑可由環境變數改變 |
| --- | --- | --- |
| `claude` | macOS：Keychain 的 `Claude Code-credentials`；其他系統：`~/.claude/.credentials.json`。另外加上 `~/.claude.json` 裡的 `oauthAccount` 欄位（檔案其他內容不動） | `CLAUDE_CONFIG_DIR` |
| `codex` | `~/.codex/auth.json` | `CODEX_HOME` |
| `gemini` | `~/.gemini/oauth_creds.json`、`google_accounts.json`、`.env`、`settings.json` 裡的認證方式欄位；macOS 另含 Keychain 的 `gemini-cli-oauth` | `GEMINI_CLI_HOME` |
| `grok` | `~/.grok/auth.json` | `GROK_HOME` |
| `opencode` | `~/.local/share/opencode/auth.json` | `XDG_DATA_HOME` |

執行 `aiom providers` 可以看到在你的機器上實際解析出的路徑，以及每一項目前是否存在。

### 自訂 provider

在 `~/.aiom/providers.toml` 可以新增其他工具，或用相同的 `id` 覆寫內建定義（例如你用的是把憑證放在別處的第三方 Grok CLI）：

```toml
[[provider]]
id = "qwen"
name = "Qwen Code"
binary = "qwen"              # 用來判斷 CLI 是否已安裝
login = ["qwen"]             # aiom login 要執行的指令
install = "npm install -g @qwen-code/qwen-code"   # 選填：CLI 未安裝時，App 會顯示這個安裝指令

[[provider.artifact]]
type = "file"                # 整個檔案都屬於登入狀態
path = "~/.qwen/oauth_creds.json"

[[provider.artifact]]
type = "json-keys"           # 只管理 JSON 檔中的某些欄位（可用 a.b.c 路徑）
path = "~/.qwen/settings.json"
keys = ["security.auth"]

# [[provider.artifact]]
# type = "keychain"          # macOS Keychain 的 generic password
# service = "some-service"
# account = "some-account"   # 省略時使用系統使用者名稱

[[provider.identity]]        # 選填：如何顯示目前登入的是誰
path = "~/.qwen/oauth_creds.json"
key = "id_token"
jwt_claim = "email"          # 省略 jwt_claim 則直接取該欄位的字串
```

`file` 與 `keychain` 預設 `primary = true`（存在即視為已登入），`json-keys` 預設 `primary = false`。

## 運作方式與安全性

- 帳號存檔位於 `~/.aiom/profiles/<provider>/<帳號>/`，目錄權限 `0700`、檔案 `0600`。可用環境變數 `AIOM_HOME` 改變位置。
- 來自 Keychain 的憑證存檔時仍放在 Keychain（service 名稱為 `aiom:<provider>:<帳號>`），不會以明文寫到磁碟。
- 桌面 App 會自動儲存尚未存過的登入；命令列的 `status`、`list` 等查詢指令則維持唯讀，要自動儲存請明確執行 `aiom adopt`。儲存只會讀取目前的登入，不會改動它。
- **切換前一定先備份**目前的登入到 `~/.aiom/backups/<provider>/`（保留最近 10 份），可用 `aiom restore` 還原。
- 切換時會先把目前登入**同步回**使用中的帳號，因為各 CLI 會就地刷新 token，線上那份才是最新的。
- 如果目前登入的身分和使用中帳號不同（例如你直接在 CLI 裡登入了別的帳號），aiom **不會**用它覆寫既有帳號，只會備份並提出警告。
- 切換到已經在使用中的帳號不會做任何事，避免把刷新過的 token 換回舊的；需要強制還原存檔時加 `--force`。
- 所有寫入都是先寫暫存檔再 rename，其他程式不會讀到寫到一半的憑證。
- aiom 完全離線運作，不會把任何資料送到網路上。

切換後請重新啟動正在執行中的 CLI 工作階段，它們才會讀到新的登入。

## 已知限制

- **Claude Code + `CLAUDE_CONFIG_DIR`（macOS）**：設定此變數時 Claude Code 可能使用不同名稱的 Keychain 項目，aiom 目前只處理預設的 `Claude Code-credentials`。
- **Codex**：若在 `config.toml` 設定 `cli_auth_credentials_store = "keyring"`，憑證不在 `auth.json`，目前不支援。
- **OpenCode v2**：新版把新登入寫進 SQLite 而非 `auth.json`，目前只管理 `auth.json`。
- **Gemini CLI**：`settings.json` 若含註解（非純 JSON），aiom 會跳過其中的認證方式欄位並提出警告，不會改動該檔。
- Keychain 支援僅限 macOS；Linux / Windows 上使用各 CLI 的檔案型憑證。
- 第一次讀取其他程式建立的 Keychain 項目時，macOS 可能會跳出授權視窗。

## 開發

核心與命令列的測試：

```bash
cargo test
```

桌面 App 的測試：

```bash
cargo test -p aiom-app
```

App 自我檢查：實際開啟視窗、載入頁面、向後端取得資料，印出第一個畫面的文字後結束（成功時結束碼為 0）：

```bash
AIOM_SELFTEST=1 dist/AIOM.app/Contents/MacOS/AIOM
```

測試全部在暫存目錄與記憶體內的假 Keychain 上執行，不會碰到真實的家目錄或 Keychain。手動試玩時也可以這樣隔離：

```bash
HOME=/tmp/aiom-sandbox AIOM_NO_KEYCHAIN=1 cargo run -- status
```

原始碼結構：

| 檔案 | 內容 |
| --- | --- |
| `src/provider.rs` | 內建與自訂 provider 的定義 |
| `src/artifact.rs` | 三種憑證載體：整個檔案、JSON 欄位、Keychain 項目 |
| `src/ops.rs` | 儲存、切換、同步、登入、還原等操作 |
| `src/store.rs` | aiom 自己的資料目錄格式 |
| `src/secrets.rs` | Keychain 存取（透過系統的 `security` 工具） |
| `src/cli.rs` | 指令列介面 |
| `app/src/main.rs` | 桌面 App 的後端（Tauri 指令）；帶參數啟動時等同 `aiom` 命令列 |
| `app/ui/` | 桌面 App 的介面（HTML / CSS / JS，無需建置工具） |
| `scripts/bundle-macos.sh` | 編譯並組出 `dist/AIOM.app` |
| `scripts/make-icon.py` | 產生 App 圖示 |
