//! What sessions spent, added up across sessions.
//!
//! Harnesses report usage differently: Claude and ACP agents send each
//! turn's tokens, Codex and pi a running total (pi starts over for a new
//! session), and every one of them a running total for cost. [`Spending`]
//! turns a session's [`Usage`] events into one running total, kept in
//! quarter-hour buckets so it can be split into days in any time zone.
//! [`roll_up`] adds up many sessions' [`UsageRecord`]s per account and per
//! day, with the newest quota each account reported. Nothing here asks a
//! provider: it is all what the harnesses reported themselves.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::event::{Quota, QuotaWindow, Usage};
use crate::harness::Harness;

/// Length of a [`Spending`] bucket. Every UTC offset in use is a multiple
/// of it, so buckets fall into whole local days.
pub const BUCKET_MS: u64 = 15 * 60 * 1000;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Tokens and cost spent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Spend {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
}

impl Spend {
    pub fn add(&mut self, other: &Spend) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        self.cost_usd += other.cost_usd;
    }

    pub fn is_zero(&self) -> bool {
        *self == Spend::default()
    }
}

/// Whether a harness's [`Usage`] values are per turn or running totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Counting {
    PerTurn,
    Total,
}

/// How `harness` counts (tokens, cost).
fn counting(harness: Harness) -> (Counting, Counting) {
    match harness {
        Harness::Claude | Harness::Cursor | Harness::Copilot | Harness::Opencode => {
            (Counting::PerTurn, Counting::Total)
        }
        Harness::Codex | Harness::Pi => (Counting::Total, Counting::Total),
    }
}

/// Growth of a running total since it was last seen. A total that went
/// down started over (pi's new session), so all of it is new.
fn growth_u64(last: &mut u64, value: u64) -> u64 {
    let old = std::mem::replace(last, value);
    if value < old { value } else { value - old }
}

fn growth_f64(last: &mut f64, value: f64) -> f64 {
    let old = std::mem::replace(last, value);
    if value < old { value } else { value - old }
}

/// One session's spending over time, from its [`Usage`] events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spending {
    harness: Harness,
    /// The last running totals seen, for harnesses that send them.
    last: Spend,
    /// Bucket start (ms since the epoch) → what was spent in it.
    buckets: BTreeMap<u64, Spend>,
}

impl Spending {
    pub fn new(harness: Harness) -> Self {
        Self {
            harness,
            last: Spend::default(),
            buckets: BTreeMap::new(),
        }
    }

    /// Counts one [`Usage`] event, reported at `at_ms`.
    pub fn record(&mut self, usage: &Usage, at_ms: u64) {
        let (tokens, cost) = counting(self.harness);
        let token = |last: &mut u64, value: Option<u64>| match (value, tokens) {
            (None, _) => 0,
            (Some(v), Counting::PerTurn) => v,
            (Some(v), Counting::Total) => growth_u64(last, v),
        };
        let last = &mut self.last;
        let spent = Spend {
            input_tokens: token(&mut last.input_tokens, usage.input_tokens),
            output_tokens: token(&mut last.output_tokens, usage.output_tokens),
            cache_read_tokens: token(&mut last.cache_read_tokens, usage.cache_read_tokens),
            cache_write_tokens: token(&mut last.cache_write_tokens, usage.cache_write_tokens),
            cost_usd: match (usage.cost_usd, cost) {
                (None, _) => 0.0,
                (Some(c), Counting::PerTurn) => c,
                (Some(c), Counting::Total) => growth_f64(&mut last.cost_usd, c),
            },
        };
        if spent.is_zero() {
            return;
        }
        let start = at_ms - at_ms % BUCKET_MS;
        self.buckets.entry(start).or_default().add(&spent);
    }

    pub fn harness(&self) -> Harness {
        self.harness
    }

    /// Everything spent so far.
    pub fn total(&self) -> Spend {
        let mut total = Spend::default();
        for s in self.buckets.values() {
            total.add(s);
        }
        total
    }

    /// What was spent per [`BUCKET_MS`] bucket, oldest first.
    pub fn buckets(&self) -> Vec<Bucket> {
        self.buckets
            .iter()
            .map(|(&start_ms, &spend)| Bucket { start_ms, spend })
            .collect()
    }
}

/// What was spent in one [`BUCKET_MS`] window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bucket {
    /// Start of the window, ms since the Unix epoch.
    pub start_ms: u64,
    pub spend: Spend,
}

/// One session's usage, as [`roll_up`] takes it. Clients keep these for
/// sessions that are gone, so the day's totals still count them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub session_id: String,
    pub harness: Harness,
    pub buckets: Vec<Bucket>,
    /// The newest quota the session reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<Quota>,
    /// When it reported `quota`, ms since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_at_ms: Option<u64>,
}

/// Spending on one account (or plan, when the harness names no account).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountUsage {
    pub harness: Harness,
    pub account: Option<String>,
    pub plan: Option<String>,
    pub sessions: u32,
    pub spend: Spend,
    /// The newest reading of each quota window any of its sessions reported.
    pub quota: Option<Quota>,
}

/// Spending on one local day.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayUsage {
    /// `YYYY-MM-DD` in the time zone [`roll_up`] was given.
    pub date: String,
    pub spend: Spend,
    /// Spend per harness that day, in [`Harness`] order.
    pub harnesses: Vec<HarnessSpend>,
}

/// What one harness spent.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HarnessSpend {
    pub harness: Harness,
    pub spend: Spend,
}

/// Spending across sessions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageRollup {
    pub total: Spend,
    pub accounts: Vec<AccountUsage>,
    /// Oldest first.
    pub days: Vec<DayUsage>,
}

/// Adds `records` up per account and per day. `utc_offset_minutes` is the
/// local time zone's offset (e.g. 120 for UTC+2) that days are split in.
pub fn roll_up(records: &[UsageRecord], utc_offset_minutes: i32) -> UsageRollup {
    let mut out = UsageRollup::default();
    let mut accounts: Vec<Account> = Vec::new();
    let mut days: BTreeMap<i64, (Spend, BTreeMap<u8, HarnessSpend>)> = BTreeMap::new();
    let offset = i64::from(utc_offset_minutes) * 60 * 1000;
    for r in records {
        let mut spend = Spend::default();
        for b in &r.buckets {
            spend.add(&b.spend);
            let day = (b.start_ms as i64 + offset).div_euclid(DAY_MS);
            let (all, per) = days.entry(day).or_default();
            all.add(&b.spend);
            per.entry(harness_order(r.harness))
                .or_insert(HarnessSpend {
                    harness: r.harness,
                    spend: Spend::default(),
                })
                .spend
                .add(&b.spend);
        }
        out.total.add(&spend);
        let (account, plan) = r
            .quota
            .as_ref()
            .map_or((None, None), |q| (q.account.clone(), q.plan.clone()));
        // Sessions name the account, or else only the plan they are on.
        let same = |a: &Account| {
            a.usage.harness == r.harness
                && a.usage.account == account
                && (account.is_some() || a.usage.plan == plan)
        };
        let i = match accounts.iter().position(same) {
            Some(i) => i,
            None => {
                accounts.push(Account {
                    usage: AccountUsage {
                        harness: r.harness,
                        account: account.clone(),
                        plan: plan.clone(),
                        sessions: 0,
                        spend: Spend::default(),
                        quota: None,
                    },
                    windows: BTreeMap::new(),
                    plan_at: 0,
                });
                accounts.len() - 1
            }
        };
        let a = &mut accounts[i];
        a.usage.sessions += 1;
        a.usage.spend.add(&spend);
        if let Some(q) = &r.quota {
            let at = r.quota_at_ms.unwrap_or(0);
            if plan.is_some() && at >= a.plan_at {
                a.usage.plan = plan;
                a.plan_at = at;
            }
            for w in &q.windows {
                if a.windows.get(&w.id).is_none_or(|(seen, _)| *seen <= at) {
                    a.windows.insert(w.id.clone(), (at, w.clone()));
                }
            }
            a.usage.quota.get_or_insert_with(Quota::default);
        }
    }
    out.accounts = accounts
        .into_iter()
        .map(|mut a| {
            if let Some(q) = &mut a.usage.quota {
                q.account.clone_from(&a.usage.account);
                q.plan.clone_from(&a.usage.plan);
                q.windows = a.windows.into_values().map(|(_, w)| w).collect();
            }
            a.usage
        })
        .collect();
    out.days = days
        .into_iter()
        .map(|(day, (spend, per))| DayUsage {
            date: civil_date(day),
            spend,
            harnesses: per.into_values().collect(),
        })
        .collect();
    out
}

/// An account being added up, with when each quota window was read.
struct Account {
    usage: AccountUsage,
    windows: BTreeMap<String, (u64, QuotaWindow)>,
    plan_at: u64,
}

fn harness_order(h: Harness) -> u8 {
    match h {
        Harness::Claude => 0,
        Harness::Codex => 1,
        Harness::Cursor => 2,
        Harness::Copilot => 3,
        Harness::Pi => 4,
        Harness::Opencode => 5,
    }
}

/// `YYYY-MM-DD` for a count of days since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil_date(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tokens(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            ..Usage::default()
        }
    }

    fn cost(c: f64) -> Usage {
        Usage {
            cost_usd: Some(c),
            ..Usage::default()
        }
    }

    #[test]
    fn per_turn_tokens_add_up_and_running_cost_counts_once() {
        let mut s = Spending::new(Harness::Claude);
        s.record(
            &Usage {
                cost_usd: Some(0.03),
                ..tokens(18, 335)
            },
            0,
        );
        // A context-only update spends nothing.
        s.record(
            &Usage {
                context_used: Some(9),
                ..Usage::default()
            },
            1,
        );
        s.record(
            &Usage {
                cost_usd: Some(0.05),
                ..tokens(10, 75)
            },
            2,
        );
        let t = s.total();
        assert_eq!((t.input_tokens, t.output_tokens), (28, 410));
        assert!((t.cost_usd - 0.05).abs() < 1e-9);
    }

    #[test]
    fn running_totals_count_growth_and_survive_a_restart() {
        let mut s = Spending::new(Harness::Pi);
        s.record(&tokens(100, 10), 0);
        s.record(&tokens(150, 30), 1);
        s.record(&tokens(150, 30), 2);
        // pi's new session starts its totals over.
        s.record(&tokens(20, 5), 3);
        let t = s.total();
        assert_eq!((t.input_tokens, t.output_tokens), (170, 35));
    }

    #[test]
    fn days_follow_the_time_zone_and_accounts_keep_the_newest_quota() {
        // 2026-10-14 23:00 UTC, and an hour later.
        let late = 1_792_018_800_000;
        let mut a = Spending::new(Harness::Codex);
        a.record(&tokens(100, 0), late);
        a.record(&tokens(300, 0), late + 3_600_000);
        let mut b = Spending::new(Harness::Codex);
        b.record(&tokens(50, 0), late);
        let mut c = Spending::new(Harness::Claude);
        c.record(&cost(1.5), late);

        let window = |pct: f64| QuotaWindow {
            id: "primary".into(),
            used_percent: Some(pct),
            ..QuotaWindow::default()
        };
        let quota = |pct: f64| Quota {
            account: None,
            plan: Some("pro".into()),
            windows: vec![window(pct)],
        };
        let records = [
            UsageRecord {
                session_id: "a".into(),
                harness: Harness::Codex,
                buckets: a.buckets(),
                quota: Some(quota(40.0)),
                quota_at_ms: Some(late + 3_600_000),
            },
            UsageRecord {
                session_id: "b".into(),
                harness: Harness::Codex,
                buckets: b.buckets(),
                quota: Some(quota(30.0)),
                quota_at_ms: Some(late),
            },
            UsageRecord {
                session_id: "c".into(),
                harness: Harness::Claude,
                buckets: c.buckets(),
                quota: None,
                quota_at_ms: None,
            },
        ];
        let utc = roll_up(&records, 0);
        assert_eq!(utc.total.input_tokens, 350);
        let dates: Vec<_> = utc
            .days
            .iter()
            .map(|d| (d.date.as_str(), d.spend.input_tokens))
            .collect();
        assert_eq!(dates, [("2026-10-14", 150), ("2026-10-15", 200)]);
        assert_eq!(utc.days[0].harnesses[0].harness, Harness::Claude);

        let oslo = roll_up(&records, 120);
        assert_eq!(oslo.days.len(), 1);
        assert_eq!(oslo.days[0].date, "2026-10-15");

        assert_eq!(utc.accounts.len(), 2);
        let codex = &utc.accounts[0];
        assert_eq!((codex.sessions, codex.spend.input_tokens), (2, 350));
        let q = codex.quota.as_ref().unwrap();
        assert_eq!(q.plan.as_deref(), Some("pro"));
        assert_eq!(q.windows[0].used_percent, Some(40.0));
        assert!(utc.accounts[1].quota.is_none());
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(-1), "1969-12-31");
        assert_eq!(civil_date(20_741), "2026-10-15");
        assert_eq!(civil_date(11_016), "2000-02-29");
    }
}
