//! When scheduled backups are due, and the background thread that starts them.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local, NaiveTime, TimeZone, Utc};
use rand::RngExt;

use crate::config::{MaintenanceRole, ScheduleConfig};
use crate::controller::{ScheduledBackupOutcome, SharedController, lock};
use crate::db::HistoryDb;

/// Delay before running a daily slot that was missed while the app was not running,
/// so the backup does not start in the middle of signing in.
const CATCH_UP_DELAY_MINUTES: i64 = 2;

/// Delay before retrying a scheduled backup that timed out waiting for a repository lock.
const DEFERRED_RETRY_MINUTES: i64 = 15;

/// Maximum automatic retries after a scheduled backup is deferred by a lock timeout.
const MAX_DEFERRED_RETRIES: u8 = 2;

/// Maximum random jitter (seconds) added before starting a scheduled backup or retry.
const SCHEDULE_JITTER_MAX_SECS: u64 = 60;

/// When the next scheduled run is due after `now`.
pub(crate) fn next_run_after<Tz: TimeZone>(
    schedule: &ScheduleConfig,
    now: &DateTime<Tz>,
) -> DateTime<Utc> {
    let times = schedule.daily_times();
    if !times.is_empty() {
        next_daily(now, &times).with_timezone(&Utc)
    } else {
        let minutes = schedule.interval_minutes.clamp(
            crate::config::MIN_INTERVAL_MINUTES,
            crate::config::MAX_INTERVAL_MINUTES,
        ) as i64;
        now.with_timezone(&Utc) + chrono::Duration::minutes(minutes)
    }
}

/// First run after the app starts. If the latest daily slot passed with no successful
/// backup since (the PC was off at that time), run shortly after start instead of waiting.
pub(crate) fn first_run_at<Tz: TimeZone>(
    schedule: &ScheduleConfig,
    now: &DateTime<Tz>,
    last_success: Option<DateTime<Utc>>,
) -> DateTime<Utc> {
    let times = schedule.daily_times();
    if !times.is_empty()
        && let Some(slot) = last_daily_slot(now, &times)
        && last_success.is_none_or(|t| t < slot)
    {
        return now.with_timezone(&Utc) + chrono::Duration::minutes(CATCH_UP_DELAY_MINUTES);
    }
    next_run_after(schedule, now)
}

/// The first time in `times` (local time in `now`'s zone) strictly after `now`.
fn next_daily<Tz: TimeZone>(now: &DateTime<Tz>, times: &[NaiveTime]) -> DateTime<Tz> {
    let tz = now.timezone();
    let mut day = now.date_naive();
    loop {
        // A local time skipped by a DST jump has no instant that day; try later slots or the next day.
        if let Some(t) = times
            .iter()
            .filter_map(|&at| tz.from_local_datetime(&day.and_time(at)).earliest())
            .find(|t| t > now)
        {
            return t;
        }
        day = day
            .succ_opt()
            .expect("calendar date stays within chrono's range");
    }
}

/// The latest slot in `times` at or before `now`.
fn last_daily_slot<Tz: TimeZone>(now: &DateTime<Tz>, times: &[NaiveTime]) -> Option<DateTime<Tz>> {
    let tz = now.timezone();
    let today = now.date_naive();
    let mut latest = None;
    for d in [Some(today), today.pred_opt()].into_iter().flatten() {
        for &at in times {
            if let Some(t) = tz.from_local_datetime(&d.and_time(at)).earliest()
                && t <= *now
                && latest.as_ref().is_none_or(|prev| t > *prev)
            {
                latest = Some(t);
            }
        }
    }
    latest
}

/// Whether a restore check should run: none has run yet, or the last one is `days` old.
/// `days` of 0 turns the checks off.
pub(crate) fn verify_is_due(last: Option<DateTime<Utc>>, now: DateTime<Utc>, days: u32) -> bool {
    days > 0 && last.is_none_or(|at| now - at >= chrono::Duration::days(i64::from(days)))
}

/// Whether the daily maintenance window (`schedule.maintenance_at`, e.g. `"05:00"`) is due:
/// the latest occurrence of that local slot is at or before `now` and no maintenance has completed
/// since that slot.
pub(crate) fn maintenance_is_due<Tz: TimeZone>(
    last_maintenance: Option<DateTime<Utc>>,
    now: &DateTime<Tz>,
    maintenance_at: NaiveTime,
) -> bool {
    let Some(slot) = last_daily_slot(now, &[maintenance_at]) else {
        return false;
    };
    let slot_utc = slot.with_timezone(&Utc);
    last_maintenance.is_none_or(|at| at < slot_utc)
}

/// Whether backups are overdue: none has succeeded for `hours`. `hours` of 0 turns this off.
pub(crate) fn backup_is_overdue(
    last_success: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    hours: u32,
) -> bool {
    hours > 0 && last_success.is_none_or(|at| now - at >= chrono::Duration::hours(i64::from(hours)))
}

/// Start time of the newest successful backup that was not a dry run.
pub(crate) fn last_success_at(db: &HistoryDb) -> Option<DateTime<Utc>> {
    db.recent_backup_runs(200)
        .ok()?
        .into_iter()
        .find(|r| !r.dry_run && r.status == "success")
        .and_then(|r| DateTime::parse_from_rfc3339(&r.started_at).ok())
        .map(|t| t.with_timezone(&Utc))
}

/// Whether a scheduled slot (`due_at`, stored in UTC) corresponds to the local `maintenance_at`
/// clock time (e.g. `05:00`).
pub(crate) fn slot_matches_maintenance<Tz: TimeZone>(
    due_at: Option<DateTime<Utc>>,
    maintenance_at: NaiveTime,
    tz: &Tz,
) -> bool {
    due_at.is_some_and(|at| at.with_timezone(tz).time() == maintenance_at)
}

/// Whether `now_time` falls within the 1-hour local window starting at `maintenance_at`
/// (e.g. `05:00`..`06:00`), used for standalone primary maintenance when either all targets are
/// standby or `daily_at` does not include `maintenance_at`.
pub(crate) fn within_maintenance_window(now_time: NaiveTime, maintenance_at: NaiveTime) -> bool {
    let delta = now_time.signed_duration_since(maintenance_at);
    delta >= chrono::Duration::zero() && delta < chrono::Duration::hours(1)
}

/// Whether the scheduler should invoke standalone daily maintenance on this idle tick:
/// - `maintenanceRole == Primary`
/// - `maintenanceAt` is configured and `now_time` is inside `[maintenanceAt, maintenanceAt + 1h)`
/// - No deferred backup retry is pending
/// - Either all targets are standby OR `daily_at` does not share the exact `maintenanceAt` clock
///   slot (so a primary PC with `dailyAt: ["17:00"]` and `maintenanceAt: "05:00"` still runs
///   maintenance at `05:00`, while `dailyAt: ["05:00", "17:00"]` lets the `05:00` backup run first).
pub(crate) fn should_run_standalone_maintenance(
    cfg: &crate::config::AppConfig,
    now_time: NaiveTime,
    has_pending_retry: bool,
) -> bool {
    if cfg.maintenance_role != MaintenanceRole::Primary || has_pending_retry {
        return false;
    }
    let Some(maint_slot) = cfg.schedule.maintenance_time() else {
        return false;
    };
    if !within_maintenance_window(now_time, maint_slot) {
        return false;
    }
    !cfg.has_active_backup_targets() || !cfg.schedule.daily_times().contains(&maint_slot)
}

fn sleep_with_quit_check(controller: &SharedController, secs: u64) -> bool {
    for _ in 0..secs {
        if controller.is_quit_requested() {
            return true;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    controller.is_quit_requested()
}

fn schedule_deferred_retry(
    outcome: ScheduledBackupOutcome,
    deferred_retries: &mut u8,
    deferred_retry_at: &mut Option<DateTime<Utc>>,
) {
    match outcome {
        ScheduledBackupOutcome::Deferred => {
            if *deferred_retries < MAX_DEFERRED_RETRIES {
                *deferred_retries += 1;
                let retry_jitter = rand::rng().random_range(0..=SCHEDULE_JITTER_MAX_SECS) as i64;
                *deferred_retry_at = Some(
                    Utc::now()
                        + chrono::Duration::minutes(DEFERRED_RETRY_MINUTES)
                        + chrono::Duration::seconds(retry_jitter),
                );
            } else {
                *deferred_retries = 0;
                *deferred_retry_at = None;
            }
        }
        ScheduledBackupOutcome::Success
        | ScheduledBackupOutcome::Failed
        | ScheduledBackupOutcome::SkippedNoActiveTargets => {
            *deferred_retries = 0;
            *deferred_retry_at = None;
        }
    }
}

/// Spawn the background periodic backup scheduler thread.
pub fn spawn_scheduler_thread(controller: Arc<SharedController>) {
    std::thread::Builder::new()
        .name("restic-tray-scheduler".to_owned())
        .spawn(move || {
            let mut deferred_retries: u8 = 0;
            let mut deferred_retry_at: Option<DateTime<Utc>> = None;
            let mut pending_maint_slot = false;

            let startup_cfg = controller.get_config();
            if startup_cfg.schedule.enabled
                && startup_cfg.schedule.run_on_startup
                && startup_cfg.is_storage_configured()
                && controller
                    .load_secrets()
                    .map(|s| s.readiness().is_complete_for(startup_cfg.storage_backend))
                    .unwrap_or(false)
                && startup_cfg.has_active_backup_targets()
            {
                let outcome = controller.execute_scheduled_backup("startup");
                schedule_deferred_retry(outcome, &mut deferred_retries, &mut deferred_retry_at);
                // Startup run is not a dedicated maintenanceAt slot; only run maintenance if no
                // dedicated maintenanceAt slot is configured.
                let allow_maint = startup_cfg.schedule.maintenance_time().is_none();
                controller.after_scheduled_backup(outcome, allow_maint);
                // The startup run stands in for a pending catch-up run.
                *lock(&controller.next_run_at) =
                    Some(next_run_after(&startup_cfg.schedule, &Local::now()));
            }

            loop {
                std::thread::sleep(Duration::from_secs(5));
                if controller.is_quit_requested() {
                    break;
                }

                let cfg = controller.get_config();
                if !cfg.schedule.enabled {
                    deferred_retries = 0;
                    deferred_retry_at = None;
                    pending_maint_slot = false;
                    continue;
                }

                // Compare wall-clock time: a run that came due while the PC slept starts on wake.
                let now = Local::now();
                let now_utc = now.with_timezone(&Utc);
                let retry_due = deferred_retry_at.is_some_and(|at| now_utc >= at);
                if retry_due {
                    deferred_retry_at = None;
                }

                let (regular_due, due_at) = {
                    let mut next = lock(&controller.next_run_at);
                    let prev = *next;
                    let due = prev.is_some_and(|at| now >= at);
                    if due || prev.is_none() {
                        *next = Some(next_run_after(&cfg.schedule, &now));
                    }
                    (due, prev)
                };

                if regular_due || retry_due {
                    // Apply 0–60s jitter ONCE: retry_due already had jitter applied when scheduled.
                    if regular_due && !retry_due {
                        let jitter = rand::rng().random_range(0..=SCHEDULE_JITTER_MAX_SECS);
                        if sleep_with_quit_check(&controller, jitter) {
                            break;
                        }
                    }

                    let cfg = controller.get_config();
                    let storage_ready = cfg.is_storage_configured()
                        && controller
                            .load_secrets()
                            .map(|s| s.readiness().is_complete_for(cfg.storage_backend))
                            .unwrap_or(false);

                    let outcome = if !storage_ready {
                        ScheduledBackupOutcome::Failed
                    } else if !cfg.has_active_backup_targets() {
                        ScheduledBackupOutcome::SkippedNoActiveTargets
                    } else {
                        controller.execute_scheduled_backup("scheduler")
                    };

                    schedule_deferred_retry(outcome, &mut deferred_retries, &mut deferred_retry_at);

                    let slot_is_maint = match cfg.schedule.maintenance_time() {
                        None => true,
                        Some(maint_slot) => {
                            let matched_regular =
                                regular_due && slot_matches_maintenance(due_at, maint_slot, &Local);
                            if regular_due {
                                pending_maint_slot = matched_regular;
                            }
                            matched_regular || (retry_due && pending_maint_slot)
                        }
                    };
                    if outcome != ScheduledBackupOutcome::Deferred || deferred_retry_at.is_none() {
                        pending_maint_slot = false;
                    }
                    controller.after_scheduled_backup(outcome, slot_is_maint);
                } else if should_run_standalone_maintenance(
                    &cfg,
                    now.time(),
                    deferred_retry_at.is_some(),
                ) && cfg.is_storage_configured()
                    && controller
                        .load_secrets()
                        .map(|s| s.readiness().is_complete_for(cfg.storage_backend))
                        .unwrap_or(false)
                {
                    // Run standalone daily maintenance within [maintenanceAt, maintenanceAt + 1h)
                    // when either all targets are standby OR `dailyAt` does not include `maintenanceAt`.
                    controller.run_due_maintenance_if_primary(true);
                }
            }
        })
        .expect("failed to spawn scheduler thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    fn jst(s: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(s).expect("valid test timestamp")
    }

    fn daily(at: &str) -> ScheduleConfig {
        ScheduleConfig {
            daily_at: vec![at.to_owned()],
            ..ScheduleConfig::default()
        }
    }

    fn daily_many(times: &[&str]) -> ScheduleConfig {
        ScheduleConfig {
            daily_at: times.iter().map(|s| (*s).to_owned()).collect(),
            ..ScheduleConfig::default()
        }
    }

    #[test]
    fn next_run_after_daily_is_today_before_the_slot_and_tomorrow_after_it() {
        let schedule = daily("05:00");
        assert_eq!(
            next_run_after(&schedule, &jst("2026-11-07T04:59:00+09:00")),
            jst("2026-11-07T05:00:00+09:00")
        );
        assert_eq!(
            next_run_after(&schedule, &jst("2026-11-07T05:00:00+09:00")),
            jst("2026-11-08T05:00:00+09:00")
        );
    }

    #[test]
    fn next_run_after_multiple_daily_times_picks_the_earliest_upcoming_slot() {
        let schedule = daily_many(&["05:00", "17:00"]);
        assert_eq!(
            next_run_after(&schedule, &jst("2026-11-07T04:59:00+09:00")),
            jst("2026-11-07T05:00:00+09:00")
        );
        assert_eq!(
            next_run_after(&schedule, &jst("2026-11-07T05:00:00+09:00")),
            jst("2026-11-07T17:00:00+09:00")
        );
        assert_eq!(
            next_run_after(&schedule, &jst("2026-11-07T17:00:00+09:00")),
            jst("2026-11-08T05:00:00+09:00")
        );
    }

    #[test]
    fn first_run_at_catches_up_when_the_latest_slot_has_no_success_after_it() {
        let schedule = daily("05:00");
        let now = jst("2026-11-07T09:30:00+09:00");
        let catch_up = jst("2026-11-07T09:32:00+09:00");
        let yesterday_run = jst("2026-11-06T05:00:10+09:00").with_timezone(&Utc);
        assert_eq!(first_run_at(&schedule, &now, None), catch_up);
        assert_eq!(first_run_at(&schedule, &now, Some(yesterday_run)), catch_up);
    }

    #[test]
    fn first_run_at_multiple_daily_slots_checks_the_most_recent_slot() {
        let schedule = daily_many(&["05:00", "17:00"]);
        let morning_run = jst("2026-11-07T05:00:10+09:00").with_timezone(&Utc);
        // At 12:00, the 05:00 slot succeeded, so wait for 17:00.
        assert_eq!(
            first_run_at(
                &schedule,
                &jst("2026-11-07T12:00:00+09:00"),
                Some(morning_run)
            ),
            jst("2026-11-07T17:00:00+09:00")
        );
        // At 19:00, the 17:00 slot was missed (last success was 05:00), so catch up in 2 minutes.
        assert_eq!(
            first_run_at(
                &schedule,
                &jst("2026-11-07T19:00:00+09:00"),
                Some(morning_run)
            ),
            jst("2026-11-07T19:02:00+09:00")
        );
    }

    #[test]
    fn first_run_at_waits_for_the_next_slot_when_the_latest_one_succeeded() {
        let schedule = daily("05:00");
        let today_run = jst("2026-11-07T05:00:05+09:00").with_timezone(&Utc);
        assert_eq!(
            first_run_at(
                &schedule,
                &jst("2026-11-07T09:30:00+09:00"),
                Some(today_run)
            ),
            jst("2026-11-08T05:00:00+09:00")
        );
        // Before today's slot, the latest slot is yesterday's.
        let yesterday_run = jst("2026-11-06T05:00:05+09:00").with_timezone(&Utc);
        assert_eq!(
            first_run_at(
                &schedule,
                &jst("2026-11-07T03:00:00+09:00"),
                Some(yesterday_run)
            ),
            jst("2026-11-07T05:00:00+09:00")
        );
    }

    #[test]
    fn verify_is_due_when_never_run_or_older_than_the_interval() {
        let now = jst("2026-11-11T17:00:00+09:00").with_timezone(&Utc);
        let days_ago = |d: i64| Some(now - chrono::Duration::days(d));
        assert!(verify_is_due(None, now, 7));
        assert!(verify_is_due(days_ago(7), now, 7));
        assert!(!verify_is_due(days_ago(6), now, 7));
        assert!(!verify_is_due(None, now, 0));
    }

    #[test]
    fn backup_is_overdue_after_the_configured_hours_without_a_success() {
        let now = jst("2026-11-11T17:00:00+09:00").with_timezone(&Utc);
        let hours_ago = |h: i64| Some(now - chrono::Duration::hours(h));
        assert!(backup_is_overdue(None, now, 48));
        assert!(backup_is_overdue(hours_ago(48), now, 48));
        assert!(!backup_is_overdue(hours_ago(47), now, 48));
        assert!(!backup_is_overdue(hours_ago(500), now, 0));
    }

    #[test]
    fn first_run_at_uses_the_interval_without_a_daily_time() {
        let schedule = ScheduleConfig::default();
        assert_eq!(
            first_run_at(&schedule, &jst("2026-11-07T09:30:00+09:00"), None),
            jst("2026-11-07T10:30:00+09:00")
        );
    }

    #[test]
    fn maintenance_is_due_runs_once_per_daily_slot_and_skips_afternoon_backups() {
        let slot = NaiveTime::from_hms_opt(5, 0, 0).unwrap();
        let morning_now = jst("2026-11-26T05:01:00+09:00");
        let yesterday_done = jst("2026-11-25T05:02:00+09:00").with_timezone(&Utc);
        let today_done = jst("2026-11-26T05:02:00+09:00").with_timezone(&Utc);
        let afternoon_now = jst("2026-11-26T17:01:00+09:00");

        assert!(maintenance_is_due(None, &morning_now, slot));
        assert!(maintenance_is_due(Some(yesterday_done), &morning_now, slot));
        assert!(!maintenance_is_due(Some(today_done), &afternoon_now, slot));

        let tz = FixedOffset::east_opt(9 * 3600).unwrap();
        let morning_utc = jst("2026-11-26T05:00:00+09:00").with_timezone(&Utc);
        let afternoon_utc = jst("2026-11-26T17:00:00+09:00").with_timezone(&Utc);
        assert!(slot_matches_maintenance(Some(morning_utc), slot, &tz));
        assert!(!slot_matches_maintenance(Some(afternoon_utc), slot, &tz));
        assert!(within_maintenance_window(
            NaiveTime::from_hms_opt(5, 30, 0).unwrap(),
            slot
        ));
        assert!(!within_maintenance_window(
            NaiveTime::from_hms_opt(17, 0, 0).unwrap(),
            slot
        ));

        // Primary PC with active target, dailyAt=["17:00"], maintenanceAt="05:00"
        // runs standalone maintenance during 05:00..06:00 and not at 17:00.
        let mut cfg = crate::config::AppConfig {
            maintenance_role: MaintenanceRole::Primary,
            schedule: ScheduleConfig {
                daily_at: vec!["17:00".to_owned()],
                maintenance_at: Some("05:00".to_owned()),
                ..ScheduleConfig::default()
            },
            ..Default::default()
        };
        let mut active_target =
            crate::config::BackupTarget::new("World".to_owned(), std::path::Path::new(r"D:\World"));
        active_target.project_key = Some("world".to_owned());
        cfg.targets.push(active_target);
        assert!(should_run_standalone_maintenance(
            &cfg,
            NaiveTime::from_hms_opt(5, 5, 0).unwrap(),
            false
        ));
        assert!(!should_run_standalone_maintenance(
            &cfg,
            NaiveTime::from_hms_opt(17, 5, 0).unwrap(),
            false
        ));
        // When dailyAt ALSO includes 05:00 and active targets exist, the 05:00 scheduled backup
        // branch drives backup -> maintenance in order instead of standalone racing ahead.
        cfg.schedule.daily_at = vec!["05:00".to_owned(), "17:00".to_owned()];
        assert!(!should_run_standalone_maintenance(
            &cfg,
            NaiveTime::from_hms_opt(5, 5, 0).unwrap(),
            false
        ));
        // If all targets are standby, standalone maintenance still runs at 05:00.
        cfg.targets[0].writer_state = crate::config::WriterState::Standby;
        assert!(should_run_standalone_maintenance(
            &cfg,
            NaiveTime::from_hms_opt(5, 5, 0).unwrap(),
            false
        ));
        // Non-primary never runs standalone maintenance.
        cfg.maintenance_role = MaintenanceRole::None;
        assert!(!should_run_standalone_maintenance(
            &cfg,
            NaiveTime::from_hms_opt(5, 5, 0).unwrap(),
            false
        ));
    }

    #[test]
    fn next_run_after_interval_stays_in_future_at_min_max_and_overflow_bounds() {
        let now = jst("2026-11-07T09:30:00+09:00");
        let now_utc = now.with_timezone(&Utc);

        let zero = ScheduleConfig {
            interval_minutes: 0,
            ..ScheduleConfig::default()
        };
        assert_eq!(
            next_run_after(&zero, &now),
            now_utc + chrono::Duration::minutes(1)
        );

        let min = ScheduleConfig {
            interval_minutes: crate::config::MIN_INTERVAL_MINUTES,
            ..ScheduleConfig::default()
        };
        assert_eq!(
            next_run_after(&min, &now),
            now_utc + chrono::Duration::minutes(1)
        );

        let max = ScheduleConfig {
            interval_minutes: crate::config::MAX_INTERVAL_MINUTES,
            ..ScheduleConfig::default()
        };
        assert_eq!(
            next_run_after(&max, &now),
            now_utc + chrono::Duration::minutes(10_080)
        );

        let huge = ScheduleConfig {
            interval_minutes: u64::MAX,
            ..ScheduleConfig::default()
        };
        let scheduled = next_run_after(&huge, &now);
        assert!(scheduled > now_utc);
        assert_eq!(scheduled, now_utc + chrono::Duration::minutes(10_080));
    }
}
