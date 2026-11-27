//! Provider account: status, expiry date and connections from Xtream
//! `get_user_info`, checked every 6 hours and shown on the dashboard. A warning
//! appears when the subscription ends within WARN_DAYS (30, as in Python).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::catalog::now_secs;
use crate::integrations::{as_text, user_info};
use crate::settings as s;
use crate::web::AppState;

pub const WARN_DAYS: i64 = 30;
const CHECK_EVERY_SECS: i64 = 6 * 3600;
const KEY: &str = "provider_account";

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Account {
    pub status: String,
    /// Unix time; None = unlimited or not sent.
    pub exp_date: Option<i64>,
    pub max_connections: String,
    pub active_cons: String,
    pub is_trial: bool,
    pub checked: i64,
    /// Last check failed (the other fields are from the last good check).
    pub error: String,
}

/// What the pages show.
pub struct View {
    pub status: String,
    pub expires: String,
    pub days_left: Option<i64>,
    pub connections: String,
    pub checked: String,
    pub error: String,
    /// 0 = fine, 1 = ends within WARN_DAYS, 2 = expired or not active.
    pub level: u8,
}

impl View {
    pub fn warning(&self) -> Option<String> {
        match (self.level, self.days_left) {
            (2, Some(d)) if d < 0 => Some(format!(
                "Your provider subscription expired on {}.",
                self.expires
            )),
            (2, _) => Some(format!(
                "Your provider account status is '{}'.",
                self.status
            )),
            (1, Some(0)) => Some("Your provider subscription ends today.".into()),
            (1, Some(1)) => Some(format!(
                "Your provider subscription ends tomorrow ({}).",
                self.expires
            )),
            (1, Some(d)) => Some(format!(
                "Your provider subscription ends in {d} days ({}).",
                self.expires
            )),
            _ => None,
        }
    }
}

pub async fn load(db: &sqlx::SqlitePool) -> Option<Account> {
    let raw = s::get(db, KEY).await.ok().flatten()?;
    serde_json::from_str(&raw).ok()
}

pub async fn view(db: &sqlx::SqlitePool) -> Option<View> {
    let a = load(db).await?;
    let days_left = a.exp_date.map(|t| {
        let today = crate::localtime::today();
        let end = crate::localtime::date(t);
        days_between(&today, &end)
    });
    let active = a.status.eq_ignore_ascii_case("active");
    let level = match days_left {
        Some(d) if d < 0 => 2,
        _ if !active && !a.status.is_empty() => 2,
        Some(d) if d <= WARN_DAYS => 1,
        _ => 0,
    };
    Some(View {
        expires: a
            .exp_date
            .map(crate::localtime::date)
            .unwrap_or_else(|| "no end date".into()),
        connections: match (a.active_cons.as_str(), a.max_connections.as_str()) {
            ("", "") => String::new(),
            (act, max) => format!(
                "{} of {} in use",
                if act.is_empty() { "?" } else { act },
                if max.is_empty() { "?" } else { max }
            ),
        },
        status: if a.is_trial {
            format!("{} (trial)", a.status)
        } else {
            a.status.clone()
        },
        checked: crate::catalog::ago(a.checked),
        error: a.error,
        days_left,
        level,
    })
}

/// Days from `a` to `b` (both YYYY-MM-DD).
fn days_between(a: &str, b: &str) -> i64 {
    let (Ok(a), Ok(b)) = (
        a.parse::<jiff::civil::Date>(),
        b.parse::<jiff::civil::Date>(),
    ) else {
        return 0;
    };
    (b - a).get_days() as i64
}

fn field(info: &Value, k: &str) -> String {
    info.get(k)
        .map(as_text)
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

/// Ask the provider now and store the answer (or the error, keeping old data).
pub async fn check(st: &AppState) -> anyhow::Result<()> {
    let Some(x) = crate::catalog::load_xtream(st).await? else {
        return Ok(());
    };
    let mut a = load(&st.db).await.unwrap_or_default();
    match tokio::time::timeout(Duration::from_secs(20), user_info(&st.http, &x)).await {
        Ok(Ok(info)) => {
            a = Account {
                status: field(&info, "status"),
                exp_date: field(&info, "exp_date")
                    .parse::<i64>()
                    .ok()
                    .filter(|t| *t > 0),
                max_connections: field(&info, "max_connections"),
                active_cons: field(&info, "active_cons"),
                is_trial: field(&info, "is_trial") == "1",
                checked: now_secs(),
                error: String::new(),
            };
        }
        Ok(Err(e)) => a.error = e,
        Err(_) => a.error = "no answer within 20 seconds".into(),
    }
    if a.checked == 0 {
        a.checked = now_secs();
    }
    s::set(&st.db, KEY, &serde_json::to_string(&a)?).await?;
    Ok(())
}

/// Scheduler hook: check every 6 hours.
pub async fn maybe_check(st: &AppState) -> anyhow::Result<()> {
    if !st.is_setup_done() {
        return Ok(());
    }
    if load(&st.db)
        .await
        .is_some_and(|a| now_secs() - a.checked < CHECK_EVERY_SECS)
    {
        return Ok(());
    }
    check(st).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days() {
        assert_eq!(days_between("2026-11-27", "2026-12-27"), 30);
        assert_eq!(days_between("2026-11-27", "2026-11-26"), -1);
        assert_eq!(days_between("2026-12-31", "2027-01-01"), 1);
    }

    #[test]
    fn warnings() {
        let v = |level, days_left| View {
            status: "Active".into(),
            expires: "2026-12-01".into(),
            days_left,
            connections: String::new(),
            checked: String::new(),
            error: String::new(),
            level,
        };
        assert_eq!(v(0, Some(200)).warning(), None);
        assert_eq!(
            v(1, Some(4)).warning().unwrap(),
            "Your provider subscription ends in 4 days (2026-12-01)."
        );
        assert_eq!(
            v(1, Some(0)).warning().unwrap(),
            "Your provider subscription ends today."
        );
        assert!(v(2, Some(-3)).warning().unwrap().contains("expired"));
    }
}
