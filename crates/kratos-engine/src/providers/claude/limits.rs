//! Claude's account limits in Kratos's terms (D38): the windows a `rate_limit_event` and a
//! `get_usage` read report, and whether the account may run.
//!
//! The two sources differ. An event states what the server just applied, as a fraction and Unix
//! seconds. A read reports the usage endpoint's view, as a whole percent and ISO 8601 text, and
//! that endpoint may lag a refusal the server just made. So the one rule that joins them lives
//! here, as pure functions: a window an event rejected stays spent in every read until its reset
//! time has passed (see [`account_from_read`]).

use std::time::{Duration, SystemTime};

use kratos_claude_client::{
    LimitWindow, RateLimitInfo, RateLimitStatus, RateLimitType, UsageRead,
    UsageWindow as EventWindow,
};
use kratos_protocol::{AccountUsage, Allowance, Provider, Timestamp, UsageBucket, UsageWindow};

use crate::usage::timestamp_of_unix;

/// The bucket of the five-hour and weekly windows every Claude plan shares.
pub(super) const UNIFIED: &str = "unified";
const FIVE_HOUR_MINUTES: i64 = 300;
const SEVEN_DAY_MINUTES: i64 = 10_080;
/// A fraction such as 0.29 times 100 lands just under 29, so a floor needs this much slack.
const ROUNDING_SLACK: f64 = 1e-6;

/// A whole percent from 0 to 100, or nothing for a value that is not a usage: NaN, infinite, or
/// negative. Never zero for a value that is missing.
fn whole_percent(percent: f64) -> Option<i32> {
    if !percent.is_finite() || percent < 0.0 {
        return None;
    }
    Some((percent + ROUNDING_SLACK).floor().min(100.0) as i32)
}

/// The percent of an event's fraction: 0.29 is 29, 0.996 is 99, and 1.0 is 100.
pub(super) fn percent_of_fraction(fraction: f64) -> Option<i32> {
    whole_percent(fraction * 100.0)
}

/// The percent of a read's utilization, which is already a percent.
pub(super) fn percent_of_percent(percent: f64) -> Option<i32> {
    whole_percent(percent)
}

/// A reset a read reports as ISO 8601 with an offset, such as `2030-01-01T05:00:00.123456+00:00`,
/// written back in the engine's own form, in UTC with milliseconds. `Timestamp` reads only a zero
/// offset, so any other is applied here.
pub(super) fn timestamp_of_iso(text: &str) -> Option<Timestamp> {
    let instant = match Timestamp::try_from(text.to_owned()) {
        Ok(parsed) => parsed.instant(),
        Err(_) => instant_with_offset(text)?,
    };
    Timestamp::at(instant).ok()
}

fn instant_with_offset(text: &str) -> Option<SystemTime> {
    let time_starts = text.find('T')?;
    let split = text.rfind(['+', '-']).filter(|&at| at > time_starts)?;
    let (clock, offset) = text.split_at(split);
    let local = Timestamp::try_from(format!("{clock}Z")).ok()?;
    let (sign, digits) = offset.split_at(1);
    let (hours, minutes) = digits.split_once(':')?;
    let seconds = hours
        .parse::<u64>()
        .ok()?
        .checked_mul(3600)?
        .checked_add(minutes.parse::<u64>().ok()?.checked_mul(60)?)?;
    let shift = Duration::from_secs(seconds);
    if sign == "+" {
        local.instant().checked_sub(shift)
    } else {
        local.instant().checked_add(shift)
    }
}

fn window_of_read(window: &LimitWindow, minutes: i64) -> Option<UsageWindow> {
    Some(UsageWindow {
        used_percent: percent_of_percent(window.utilization?)?,
        window_minutes: Some(minutes),
        resets_at: window.resets_at.as_deref().and_then(timestamp_of_iso),
    })
}

/// The account after a full `get_usage` read, with the windows an earlier event rejected kept
/// spent until their reset has passed at `now`.
///
/// The allowance is `Blocked` when a unified window is at 100 percent, `Allowed` when both unified
/// windows are reported below it, and `Unavailable` otherwise: plan limits do not apply, the read
/// holds none, or a unified window is missing. Missing information is never permission.
pub(super) fn account_from_read(
    read: &UsageRead,
    plan: Option<String>,
    previous: Option<&AccountUsage>,
    now: SystemTime,
    read_at: Timestamp,
) -> AccountUsage {
    let limits = read
        .rate_limits
        .as_ref()
        .filter(|_| read.rate_limits_available);
    let windows = |minutes, pick: fn(&kratos_claude_client::RateLimits) -> &Option<LimitWindow>| {
        limits
            .and_then(|limits| pick(limits).as_ref())
            .and_then(|window| window_of_read(window, minutes))
    };
    let mut buckets = Vec::new();
    let (five, seven) = (
        windows(FIVE_HOUR_MINUTES, |limits| &limits.five_hour),
        windows(SEVEN_DAY_MINUTES, |limits| &limits.seven_day),
    );
    if five.is_some() || seven.is_some() {
        buckets.push(bucket(UNIFIED, five, seven));
    }
    for (name, window) in [
        (
            "seven_day_opus",
            windows(SEVEN_DAY_MINUTES, |limits| &limits.seven_day_opus),
        ),
        (
            "seven_day_sonnet",
            windows(SEVEN_DAY_MINUTES, |limits| &limits.seven_day_sonnet),
        ),
    ] {
        if window.is_some() {
            buckets.push(bucket(name, None, window));
        }
    }
    if let Some(previous) = previous {
        keep_rejections(&mut buckets, previous, now);
    }
    let allowance = read_allowance(read, &buckets);
    AccountUsage {
        provider: Provider::Claude,
        account: None,
        plan,
        allowance,
        buckets,
        read_at,
    }
}

fn bucket(
    limit_id: &str,
    primary: Option<UsageWindow>,
    secondary: Option<UsageWindow>,
) -> UsageBucket {
    UsageBucket {
        limit_id: limit_id.to_owned(),
        name: None,
        primary,
        secondary,
        reached: None,
    }
}

fn read_allowance(read: &UsageRead, buckets: &[UsageBucket]) -> Allowance {
    let unified = buckets.iter().find(|bucket| bucket.limit_id == UNIFIED);
    let windows = |bucket: &UsageBucket| [bucket.primary.clone(), bucket.secondary.clone()];
    let is_spent = unified.is_some_and(|bucket| {
        windows(bucket)
            .iter()
            .flatten()
            .any(|window| window.used_percent >= 100)
    });
    let reason = if is_spent {
        return Allowance::Blocked;
    } else if !read.rate_limits_available {
        "Claude's plan limits do not apply to this session"
    } else if read.rate_limits.is_none() {
        "Claude did not report the plan's limits"
    } else if !unified.is_some_and(|bucket| bucket.primary.is_some() && bucket.secondary.is_some())
    {
        "Claude did not report both the five-hour and the weekly window"
    } else {
        return Allowance::Allowed;
    };
    Allowance::Unavailable {
        reason: reason.to_owned(),
    }
}

/// Puts back every window the earlier snapshot holds as rejected and still spent at `now`. A
/// window with no reset time is not kept: nothing would ever say it had passed.
fn keep_rejections(buckets: &mut Vec<UsageBucket>, previous: &AccountUsage, now: SystemTime) {
    let is_kept = |window: &&UsageWindow| {
        window.used_percent >= 100
            && window
                .resets_at
                .as_ref()
                .is_some_and(|reset| reset.instant() > now)
    };
    for earlier in previous
        .buckets
        .iter()
        .filter(|bucket| bucket.reached.is_some())
    {
        let position = match buckets
            .iter()
            .position(|bucket| bucket.limit_id == earlier.limit_id)
        {
            Some(position) => position,
            None => {
                buckets.push(bucket(&earlier.limit_id, None, None));
                buckets.len() - 1
            }
        };
        let current = &mut buckets[position];
        let primary = earlier.primary.iter().find(is_kept);
        let secondary = earlier.secondary.iter().find(is_kept);
        if primary.is_some() || secondary.is_some() {
            current.reached = earlier.reached.clone();
        }
        if let Some(window) = primary {
            current.primary = Some(window.clone());
        }
        if let Some(window) = secondary {
            current.secondary = Some(window.clone());
        }
    }
    buckets.retain(|bucket| bucket.primary.is_some() || bucket.secondary.is_some());
}

/// What a `rate_limit_event` states: the allowance the server just applied, and the windows it
/// names. `rejected` marks the named window spent at 100 percent and the bucket as reached.
pub(super) fn account_from_event(
    info: &RateLimitInfo,
    plan: Option<String>,
    read_at: Timestamp,
) -> AccountUsage {
    let allowance = match &info.status {
        RateLimitStatus::Allowed | RateLimitStatus::AllowedWarning => Allowance::Allowed,
        RateLimitStatus::Rejected => Allowance::Blocked,
        RateLimitStatus::Unrecognized(status) => Allowance::Unavailable {
            reason: format!("Claude reported the unrecognized rate limit status {status:?}"),
        },
    };
    AccountUsage {
        provider: Provider::Claude,
        account: None,
        plan,
        allowance,
        buckets: event_buckets(info),
        read_at,
    }
}

/// The window the event reports on: spent when rejected, else at the event's own utilization.
fn reported_window(info: &RateLimitInfo, minutes: Option<i64>) -> Option<UsageWindow> {
    let used_percent = if info.status == RateLimitStatus::Rejected {
        100
    } else {
        percent_of_fraction(info.utilization?)?
    };
    Some(UsageWindow {
        used_percent,
        window_minutes: minutes,
        resets_at: info.resets_at.and_then(timestamp_of_unix),
    })
}

/// The event's window, with the reset of the unified window it replaces when the event has none.
fn overlay(unified: Option<UsageWindow>, reported: Option<UsageWindow>) -> Option<UsageWindow> {
    match reported {
        None => unified,
        Some(mut window) => {
            if window.resets_at.is_none() {
                window.resets_at = unified.and_then(|unified| unified.resets_at);
            }
            Some(window)
        }
    }
}

fn event_buckets(info: &RateLimitInfo) -> Vec<UsageBucket> {
    let windows = info.unified_windows.as_ref();
    let unified_window = |window: Option<&EventWindow>, minutes| {
        let window = window?;
        Some(UsageWindow {
            used_percent: percent_of_fraction(window.utilization?)?,
            window_minutes: Some(minutes),
            resets_at: window.resets_at.and_then(timestamp_of_unix),
        })
    };
    let mut five = unified_window(
        windows.and_then(|w| w.five_hour.as_ref()),
        FIVE_HOUR_MINUTES,
    );
    let mut seven = unified_window(
        windows.and_then(|w| w.seven_day.as_ref()),
        SEVEN_DAY_MINUTES,
    );
    let kind = info.rate_limit_type.as_ref();
    let is_rejected = info.status == RateLimitStatus::Rejected;
    match kind {
        Some(RateLimitType::FiveHour) => {
            five = overlay(five, reported_window(info, Some(FIVE_HOUR_MINUTES)));
        }
        Some(RateLimitType::SevenDay) => {
            seven = overlay(seven, reported_window(info, Some(SEVEN_DAY_MINUTES)));
        }
        _ => {}
    }
    let mut buckets = Vec::new();
    if five.is_some() || seven.is_some() {
        let mut unified = bucket(UNIFIED, five, seven);
        if is_rejected
            && matches!(
                kind,
                Some(RateLimitType::FiveHour | RateLimitType::SevenDay)
            )
        {
            unified.reached = kind.map(|kind| kind.as_str().to_owned());
        }
        buckets.push(unified);
    }
    buckets.extend(own_bucket(info, kind));
    buckets
}

/// The bucket of a limit that is not the unified pair. Opus and Sonnet are weekly; an overage or
/// a limit the crate does not know has no window length, so its plan always waits for the user.
fn own_bucket(info: &RateLimitInfo, kind: Option<&RateLimitType>) -> Option<UsageBucket> {
    let is_rejected = info.status == RateLimitStatus::Rejected;
    let (name, minutes) = match kind {
        Some(
            weekly @ (RateLimitType::SevenDayOpus
            | RateLimitType::SevenDaySonnet
            | RateLimitType::SevenDayOverageIncluded),
        ) => (weekly.as_str(), Some(SEVEN_DAY_MINUTES)),
        Some(other @ (RateLimitType::Overage | RateLimitType::Unrecognized(_))) => {
            (other.as_str(), None)
        }
        None if is_rejected => ("unknown", None),
        _ => return None,
    };
    let window = reported_window(info, minutes)?;
    let mut own = if minutes.is_some() {
        bucket(name, None, Some(window))
    } else {
        bucket(name, Some(window), None)
    };
    if is_rejected {
        own.reached = Some(name.to_owned());
    }
    Some(own)
}

#[cfg(test)]
mod tests {
    use kratos_claude_client::{LimitWindow, RateLimits};

    use super::*;

    fn stamp(text: &str) -> Timestamp {
        Timestamp::try_from(text.to_owned()).unwrap()
    }

    fn at(text: &str) -> SystemTime {
        stamp(text).instant()
    }

    #[test]
    fn an_events_fraction_is_floored_to_a_whole_percent_with_float_slack() {
        // Catches 0.29 read as 28 because 0.29 times 100 is 28.999999999999996, and 0.996 rounded
        // up to 100, which would mark a window that still has room as spent.
        assert_eq!(percent_of_fraction(0.29), Some(29));
        assert_eq!(percent_of_fraction(0.53), Some(53));
        assert_eq!(percent_of_fraction(0.996), Some(99));
        assert_eq!(percent_of_fraction(1.0), Some(100));
        assert_eq!(percent_of_fraction(0.0), Some(0));
        assert_eq!(percent_of_fraction(1.4), Some(100));
    }

    #[test]
    fn a_reads_percent_is_clamped_and_a_value_that_is_no_usage_is_missing_not_zero() {
        // Catches a NaN, a negative, or an infinity counted as 0 percent, which would read a
        // broken answer as an account with room.
        assert_eq!(percent_of_percent(19.0), Some(19));
        assert_eq!(percent_of_percent(99.9), Some(99));
        assert_eq!(percent_of_percent(100.0), Some(100));
        assert_eq!(percent_of_percent(250.0), Some(100));
        assert_eq!(percent_of_percent(f64::NAN), None);
        assert_eq!(percent_of_percent(-1.0), None);
        assert_eq!(percent_of_percent(f64::INFINITY), None);
        assert_eq!(percent_of_fraction(f64::NEG_INFINITY), None);
    }

    #[test]
    fn the_measured_reset_text_with_microseconds_and_an_offset_is_read() {
        // Catches the reset of every get_usage read being dropped because Timestamp reads only
        // the Z form, which would leave a spent window with no time to wait for.
        let measured = timestamp_of_iso("2030-01-01T05:00:00.123456+00:00").unwrap();
        assert_eq!(measured.as_str(), "2030-01-01T05:00:00.123Z");
        let ahead = timestamp_of_iso("2030-01-01T07:00:00+02:00").unwrap();
        assert_eq!(ahead.as_str(), "2030-01-01T05:00:00.000Z");
        let behind = timestamp_of_iso("2029-12-31T23:30:00-05:30").unwrap();
        assert_eq!(behind.as_str(), "2030-01-01T05:00:00.000Z");
        let zulu = timestamp_of_iso("2030-01-01T05:00:00.000Z").unwrap();
        assert_eq!(zulu.as_str(), "2030-01-01T05:00:00.000Z");
        assert_eq!(timestamp_of_iso("tomorrow"), None);
        assert_eq!(timestamp_of_iso("2030-01-01"), None);
    }

    #[test]
    fn a_time_that_cannot_be_represented_is_missing_and_never_a_panic() {
        // Catches a CLI that sends milliseconds, or any huge number, killing the actor with an
        // overflow in time arithmetic, which would stop every Claude thread.
        assert_eq!(timestamp_of_unix(u64::MAX), None);
        assert_eq!(timestamp_of_unix(1_792_531_800_000), None);
        assert_eq!(
            timestamp_of_iso("2030-01-01T05:00:00+18446744073709551615:00"),
            None
        );
        assert_eq!(
            timestamp_of_iso("2030-01-01T05:00:00-00:18446744073709551615"),
            None
        );
    }

    #[test]
    fn an_events_unix_seconds_are_seconds_not_milliseconds() {
        // Catches a reset read as milliseconds, which would put it in January 1970.
        assert_eq!(
            timestamp_of_unix(1_792_531_800).unwrap().as_str(),
            "2026-10-20T21:30:00.000Z"
        );
    }

    const SOON: &str = "2030-01-01T05:00:00Z";
    const LATER: &str = "2030-01-06T12:00:00Z";
    const BEFORE: &str = "2030-01-01T04:58:00Z";

    fn read(five: Option<(f64, &str)>, seven: Option<(f64, &str)>) -> UsageRead {
        let window = |given: Option<(f64, &str)>| {
            given.map(|(utilization, resets_at)| LimitWindow {
                utilization: Some(utilization),
                resets_at: Some(resets_at.to_owned()),
            })
        };
        UsageRead {
            subscription_type: Some("pro".into()),
            rate_limits_available: true,
            rate_limits: Some(RateLimits {
                five_hour: window(five),
                seven_day: window(seven),
                ..RateLimits::default()
            }),
        }
    }

    fn account_at(read: &UsageRead, previous: Option<&AccountUsage>, now: &str) -> AccountUsage {
        account_from_read(read, Some("pro".into()), previous, at(now), stamp(now))
    }

    fn windows(account: &AccountUsage) -> (Option<i32>, Option<i32>) {
        let unified = account.buckets.iter().find(|b| b.limit_id == UNIFIED);
        let percent = |window: Option<&UsageWindow>| window.map(|w| w.used_percent);
        (
            percent(unified.and_then(|b| b.primary.as_ref())),
            percent(unified.and_then(|b| b.secondary.as_ref())),
        )
    }

    #[test]
    fn a_read_is_blocked_at_100_allowed_below_and_unavailable_without_both_windows() {
        // Catches missing information becoming permission: a window absent, a read without limits,
        // or an API-key session counted as an account with room.
        let spent = account_at(&read(Some((100.0, SOON)), Some((7.0, LATER))), None, BEFORE);
        let weekly_spent = account_at(
            &read(Some((19.0, SOON)), Some((100.0, LATER))),
            None,
            BEFORE,
        );
        let free = account_at(&read(Some((99.9, SOON)), Some((7.0, LATER))), None, BEFORE);
        let no_week = account_at(&read(Some((19.0, SOON)), None), None, BEFORE);
        let mut no_limits = read(None, None);
        no_limits.rate_limits = None;
        let mut api_key = read(Some((19.0, SOON)), Some((7.0, LATER)));
        api_key.rate_limits_available = false;

        assert_eq!(spent.allowance, Allowance::Blocked);
        assert_eq!(weekly_spent.allowance, Allowance::Blocked);
        assert_eq!(free.allowance, Allowance::Allowed);
        assert_eq!(windows(&free), (Some(99), Some(7)));
        for unavailable in [
            &no_week,
            &account_at(&no_limits, None, BEFORE),
            &account_at(&api_key, None, BEFORE),
        ] {
            assert!(
                matches!(unavailable.allowance, Allowance::Unavailable { .. }),
                "{unavailable:?}"
            );
        }
    }

    #[test]
    fn the_five_hour_window_is_primary_300_minutes_and_the_week_secondary_10080() {
        // Catches the windows swapped, which the scheduler would read as a weekly stop.
        let account = account_at(&read(Some((19.0, SOON)), Some((7.0, LATER))), None, BEFORE);

        let unified = &account.buckets[0];
        assert_eq!(unified.primary.as_ref().unwrap().window_minutes, Some(300));
        assert_eq!(
            unified.secondary.as_ref().unwrap().window_minutes,
            Some(10_080)
        );
        assert_eq!(
            unified
                .primary
                .as_ref()
                .and_then(|window| window.resets_at.as_ref())
                .map(Timestamp::instant),
            Some(at(SOON))
        );
        assert_eq!(account.account, None);
    }

    fn rejected_five_hour_event() -> AccountUsage {
        let info: RateLimitInfo = serde_json::from_value(serde_json::json!({
            "status": "rejected", "rateLimitType": "five_hour", "resetsAt": 1_893_474_000_u64,
        }))
        .unwrap();
        account_from_event(&info, Some("pro".into()), stamp(BEFORE))
    }

    #[test]
    fn a_window_an_event_rejected_stays_spent_in_a_read_until_its_reset_passes() {
        // Catches a lagging usage endpoint overriding the server's refusal: the read says 19
        // percent, the event said rejected, and the scheduler would send into a spent account.
        let rejected = rejected_five_hour_event();
        assert_eq!(rejected.buckets[0].reached.as_deref(), Some("five_hour"));
        let lagging = read(Some((19.0, SOON)), Some((7.0, LATER)));

        let before_the_reset = account_at(&lagging, Some(&rejected), "2030-01-01T04:59:59Z");
        let after_the_reset = account_at(&lagging, Some(&rejected), "2030-01-01T05:00:00Z");

        assert_eq!(before_the_reset.allowance, Allowance::Blocked);
        assert_eq!(windows(&before_the_reset), (Some(100), Some(7)));
        assert_eq!(
            before_the_reset.buckets[0].reached.as_deref(),
            Some("five_hour")
        );
        assert_eq!(after_the_reset.allowance, Allowance::Allowed);
        assert_eq!(windows(&after_the_reset), (Some(19), Some(7)));
        assert_eq!(after_the_reset.buckets[0].reached, None);
    }

    #[test]
    fn a_kept_rejection_survives_a_read_that_reports_nothing() {
        // Catches a failed-limits read wiping the memory of a refusal that has not reset.
        let rejected = rejected_five_hour_event();
        let mut empty = read(None, None);
        empty.rate_limits = None;

        let account = account_at(&empty, Some(&rejected), "2030-01-01T04:59:00Z");

        assert_eq!(account.allowance, Allowance::Blocked);
        assert_eq!(windows(&account), (Some(100), None));
    }

    #[test]
    fn an_event_states_the_allowance_and_the_unified_windows_with_the_recorded_values() {
        // Catches the event's fractions kept as fractions, and a warning read as a block.
        let info: RateLimitInfo = serde_json::from_value(serde_json::json!({
            "status": "allowed_warning", "rateLimitType": "five_hour", "resetsAt": 1_792_531_800_u64,
            "unifiedWindows": {
                "five_hour": { "utilization": 0.53, "resetsAt": 1_792_531_800_u64 },
                "seven_day": { "utilization": 0.17, "resetsAt": 1_792_638_000_u64 },
            },
        }))
        .unwrap();

        let account = account_from_event(&info, None, stamp(BEFORE));

        assert_eq!(account.allowance, Allowance::Allowed);
        assert_eq!(windows(&account), (Some(53), Some(17)));
        assert_eq!(account.buckets[0].reached, None);
    }

    #[test]
    fn a_weekly_rejection_marks_the_week_and_an_overage_or_new_limit_has_no_window_length() {
        // Catches a weekly stop planned as automatic, and an overage refusal planned as a
        // five-hour wait that would send into a limit nothing resets soon.
        let event = |kind: &str| -> AccountUsage {
            let info: RateLimitInfo = serde_json::from_value(serde_json::json!({
                "status": "rejected", "rateLimitType": kind, "resetsAt": 1_893_474_000_u64,
            }))
            .unwrap();
            account_from_event(&info, None, stamp(BEFORE))
        };

        let weekly = event("seven_day");
        let overage = event("overage");
        let opus = event("seven_day_opus");
        let new_kind = event("monthly");

        assert_eq!(windows(&weekly), (None, Some(100)));
        assert_eq!(
            weekly.buckets[0].secondary.as_ref().unwrap().window_minutes,
            Some(10_080)
        );
        assert_eq!(weekly.buckets[0].reached.as_deref(), Some("seven_day"));
        assert_eq!(overage.buckets[0].limit_id, "overage");
        assert_eq!(
            overage.buckets[0].primary.as_ref().unwrap().window_minutes,
            None
        );
        assert_eq!(overage.buckets[0].reached.as_deref(), Some("overage"));
        assert_eq!(opus.buckets[0].limit_id, "seven_day_opus");
        assert_eq!(
            opus.buckets[0].secondary.as_ref().unwrap().window_minutes,
            Some(10_080)
        );
        assert_eq!(new_kind.buckets[0].limit_id, "monthly");
        assert_eq!(new_kind.allowance, Allowance::Blocked);
    }

    #[test]
    fn a_status_the_crate_does_not_know_is_unavailable_with_the_value_in_the_reason() {
        // Catches an unknown status read as permission, and a reason that hides what the CLI said.
        let info: RateLimitInfo = serde_json::from_value(serde_json::json!({
            "status": "throttled", "rateLimitType": "five_hour",
        }))
        .unwrap();

        let account = account_from_event(&info, None, stamp(BEFORE));

        assert!(
            matches!(&account.allowance, Allowance::Unavailable { reason } if reason.contains("throttled")),
            "{:?}",
            account.allowance
        );
    }
}
