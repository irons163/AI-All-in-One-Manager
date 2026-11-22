//! How much of each saved account's usage limits is used, asked from the
//! provider's own service with that account's saved credentials. Read only:
//! the credentials are read in memory and never refreshed or written here.

use crate::ctx::Ctx;
use crate::ops;
use crate::provider::{Provider, UsageApi};
use crate::store::{self, Meta};
use crate::util;
use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

/// ChatGPT's endpoint the Codex CLI and app use for their usage display.
pub const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const TIMEOUT_SECS: u64 = 15;

/// One rolling limit, such as "5 hours" or "a week".
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Window {
    /// Length of the window in minutes (300 = 5 hours, 10080 = a week).
    pub minutes: u64,
    pub used_percent: f64,
    /// When the window resets, as Unix seconds.
    pub resets_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Usage {
    /// The subscription, e.g. "plus" or "pro".
    pub plan: Option<String>,
    pub windows: Vec<Window>,
    /// Whether the account is out of quota right now.
    pub limit_reached: bool,
    /// Rate-limit reset vouchers the account holds: each one resets a used
    /// up limit once (redeemed in the Codex app).
    pub reset_vouchers: Option<Vouchers>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Vouchers {
    pub available: u64,
    /// How many can be used right now (only when a limit is used up).
    pub usable_now: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AccountUsage {
    Ok { usage: Usage },
    /// The saved login has expired or was signed out. Switching to the
    /// account and using it once makes the CLI refresh it.
    Expired,
    /// The account has no credentials usage can be asked with (e.g. an API
    /// key login).
    Unavailable { reason: String },
    Failed { error: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountReport {
    pub name: String,
    pub active: bool,
    #[serde(flatten)]
    pub usage: AccountUsage,
}

/// What a query needs, gathered up front so the network calls can run in
/// parallel without the context.
enum Query {
    Codex { token: String, account_id: Option<String> },
    Done(AccountUsage),
}

/// Usage of every saved account of `p`, in account order. `None` when the
/// provider has no known usage service.
pub fn accounts(ctx: &Ctx, p: &Provider) -> Result<Option<Vec<AccountReport>>> {
    let Some(api) = &p.usage else {
        return Ok(None);
    };
    let metas = store::list(ctx, &p.id)?;
    let active = store::active(ctx, &p.id)?;
    let queries: Vec<Query> = metas
        .iter()
        .map(|meta| prepare(ctx, p, api, meta, active.as_deref()))
        .collect();
    let url = ctx
        .var("AIOM_CODEX_USAGE_URL")
        .unwrap_or(CODEX_USAGE_URL)
        .to_string();
    let results: Vec<AccountUsage> = std::thread::scope(|scope| {
        let handles: Vec<_> = queries
            .into_iter()
            .map(|query| {
                let url = url.clone();
                scope.spawn(move || match query {
                    Query::Done(result) => result,
                    Query::Codex { token, account_id } => {
                        codex(&url, &token, account_id.as_deref())
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|_| AccountUsage::Failed {
                    error: "internal error".into(),
                })
            })
            .collect()
    });
    Ok(Some(
        metas
            .into_iter()
            .zip(results)
            .map(|(meta, usage)| AccountReport {
                active: active.as_deref() == Some(meta.name.as_str()),
                name: meta.name,
                usage,
            })
            .collect(),
    ))
}

fn prepare(ctx: &Ctx, p: &Provider, api: &UsageApi, meta: &Meta, active: Option<&str>) -> Query {
    match api {
        UsageApi::Codex { artifact } => {
            let data = match ops::account_artifact(ctx, p, meta, active, artifact) {
                Ok(Some(data)) => data,
                Ok(None) => {
                    return Query::Done(AccountUsage::Unavailable {
                        reason: "no saved login".into(),
                    })
                }
                Err(err) => {
                    return Query::Done(AccountUsage::Failed {
                        error: format!("{err:#}"),
                    })
                }
            };
            codex_query(&data, util::now_secs())
        }
    }
}

fn codex_query(auth_json: &[u8], now: u64) -> Query {
    let Ok(auth) = serde_json::from_slice::<Value>(auth_json) else {
        return Query::Done(AccountUsage::Failed {
            error: "the saved auth.json is not valid JSON".into(),
        });
    };
    let Some(token) = auth
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    else {
        return Query::Done(AccountUsage::Unavailable {
            reason: "not a ChatGPT login (API key logins have no usage limits to show)".into(),
        });
    };
    if util::jwt_number(token, "exp").is_some_and(|exp| exp <= now) {
        return Query::Done(AccountUsage::Expired);
    }
    Query::Codex {
        token: token.to_string(),
        account_id: auth
            .pointer("/tokens/account_id")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

fn codex(url: &str, token: &str, account_id: Option<&str>) -> AccountUsage {
    let auth = format!("Bearer {token}");
    let mut headers = vec![
        ("Authorization", auth.as_str()),
        ("Accept", "application/json"),
        ("User-Agent", "aiom"),
    ];
    if let Some(id) = account_id {
        headers.push(("ChatGPT-Account-Id", id));
    }
    match util::http_get(url, &headers, TIMEOUT_SECS) {
        Ok((200, body)) => match serde_json::from_slice::<Value>(&body) {
            Ok(json) => AccountUsage::Ok {
                usage: parse_codex(&json),
            },
            Err(_) => AccountUsage::Failed {
                error: "the usage service answered with something other than JSON".into(),
            },
        },
        Ok((401, _)) => AccountUsage::Expired,
        Ok((code, _)) => AccountUsage::Failed {
            error: format!("the usage service answered HTTP {code}"),
        },
        Err(err) => AccountUsage::Failed {
            error: format!("{err:#}"),
        },
    }
}

pub fn parse_codex(json: &Value) -> Usage {
    let limits = &json["rate_limit"];
    let windows = ["primary_window", "secondary_window"]
        .iter()
        .filter_map(|key| {
            let w = limits.get(*key).filter(|w| w.is_object())?;
            Some(Window {
                minutes: w["limit_window_seconds"].as_u64()? / 60,
                used_percent: w["used_percent"].as_f64()?,
                resets_at: w["reset_at"].as_u64(),
            })
        })
        .collect();
    let credits = &json["rate_limit_reset_credits"];
    Usage {
        plan: json["plan_type"].as_str().map(str::to_string),
        windows,
        limit_reached: limits["limit_reached"].as_bool().unwrap_or(false),
        reset_vouchers: credits["available_count"].as_u64().map(|available| Vouchers {
            available,
            usable_now: credits["applicable_available_count"].as_u64().unwrap_or(0),
        }),
    }
}

/// "5h", "week", "30d": a short name for a window length.
pub fn window_name(minutes: u64) -> String {
    match minutes {
        10_080 => "week".into(),
        m if m % 1440 == 0 => format!("{}d", m / 1440),
        m if m % 60 == 0 => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jwt(exp: u64) -> String {
        let b64 = |v: &Value| {
            const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let bytes = v.to_string().into_bytes();
            let mut out = String::new();
            for chunk in bytes.chunks(3) {
                let n = chunk.iter().fold(0u32, |n, &b| (n << 8) | b as u32) << (8 * (3 - chunk.len()));
                for i in 0..=chunk.len() {
                    out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
                }
            }
            out
        };
        format!("{}.{}.sig", b64(&json!({"alg": "none"})), b64(&json!({"exp": exp})))
    }

    #[test]
    fn parses_the_codex_answer() {
        let usage = parse_codex(&json!({
            "plan_type": "plus",
            "rate_limit": {
                "allowed": false,
                "limit_reached": true,
                "primary_window": {"used_percent": 12, "limit_window_seconds": 18000, "reset_at": 100},
                "secondary_window": {"used_percent": 100, "limit_window_seconds": 604800, "reset_at": 200}
            },
            "rate_limit_reset_credits": {"available_count": 3, "applicable_available_count": 1}
        }));
        assert_eq!(usage.reset_vouchers, Some(Vouchers { available: 3, usable_now: 1 }));
        assert_eq!(usage.plan.as_deref(), Some("plus"));
        assert!(usage.limit_reached);
        assert_eq!(
            usage.windows,
            [
                Window { minutes: 300, used_percent: 12.0, resets_at: Some(100) },
                Window { minutes: 10_080, used_percent: 100.0, resets_at: Some(200) },
            ]
        );
        let empty = parse_codex(&json!({"plan_type": "free", "rate_limit": {"secondary_window": null}}));
        assert!(empty.windows.is_empty() && !empty.limit_reached);
        assert_eq!(empty.reset_vouchers, None);
    }

    #[test]
    fn expired_and_api_key_logins_are_not_queried() {
        let auth = json!({"tokens": {"access_token": jwt(1_000), "account_id": "acct"}}).to_string();
        assert!(matches!(codex_query(auth.as_bytes(), 999), Query::Codex { .. }));
        assert!(matches!(codex_query(auth.as_bytes(), 1_000), Query::Done(AccountUsage::Expired)));
        let key = json!({"OPENAI_API_KEY": "sk-test", "tokens": null}).to_string();
        assert!(matches!(
            codex_query(key.as_bytes(), 0),
            Query::Done(AccountUsage::Unavailable { .. })
        ));
    }

    #[test]
    fn names_windows() {
        assert_eq!(window_name(300), "5h");
        assert_eq!(window_name(10_080), "week");
        assert_eq!(window_name(43_200), "30d");
        assert_eq!(window_name(90), "90m");
    }
}
