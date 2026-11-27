//! Formatting for times, sizes and durations shown in the GUI (Japanese locale, local time).

use chrono::{DateTime, Datelike, Local, Timelike};

pub fn parse_local(ts: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(ts.trim())
        .ok()
        .map(|dt| dt.with_timezone(&Local))
}

/// Age split into a figure and its unit, so the figure can be set in a larger face.
pub fn age_parts(then: DateTime<Local>, now: DateTime<Local>) -> (String, &'static str) {
    let secs = (now - then).num_seconds().max(0);
    if secs < 60 {
        ("たった今".to_owned(), "")
    } else if secs < 3_600 {
        ((secs / 60).to_string(), "分前")
    } else if secs < 86_400 {
        ((secs / 3_600).to_string(), "時間前")
    } else {
        ((secs / 86_400).to_string(), "日前")
    }
}

/// "たった今" / "12分前" / "3時間前" / "2日前"; a date once it is a week old.
pub fn relative(then: DateTime<Local>, now: DateTime<Local>) -> String {
    if (now - then).num_days() >= 7 {
        return month_day(then, now);
    }
    let (n, unit) = age_parts(then, now);
    format!("{n}{unit}")
}

/// "まもなく" / "あと12分" / "あと3時間" / "あと2日".
pub fn until(then: DateTime<Local>, now: DateTime<Local>) -> String {
    let secs = (then - now).num_seconds();
    if secs < 60 {
        "まもなく".to_owned()
    } else if secs < 3_600 {
        format!("あと{}分", (secs + 59) / 60)
    } else if secs < 86_400 {
        format!("あと{}時間", secs / 3_600)
    } else {
        format!("あと{}日", secs / 86_400)
    }
}

pub fn clock(dt: DateTime<Local>) -> String {
    format!("{}:{:02}", dt.hour(), dt.minute())
}

fn month_day(dt: DateTime<Local>, now: DateTime<Local>) -> String {
    if dt.year() == now.year() {
        format!("{}月{}日", dt.month(), dt.day())
    } else {
        format!("{}年{}月{}日", dt.year(), dt.month(), dt.day())
    }
}

/// "今日 20:50" / "昨日 20:50" / "11月5日 20:50".
pub fn day_time(dt: DateTime<Local>, now: DateTime<Local>) -> String {
    let days = (now.date_naive() - dt.date_naive()).num_days();
    let day = match days {
        0 => "今日".to_owned(),
        1 => "昨日".to_owned(),
        _ => month_day(dt, now),
    };
    format!("{day} {}", clock(dt))
}

pub fn weekday(date: &impl Datelike) -> &'static str {
    ["月", "火", "水", "木", "金", "土", "日"][date.weekday().num_days_from_monday() as usize]
}

pub fn duration_ms(ms: u64) -> String {
    let secs = ms / 1_000;
    if ms < 10_000 {
        format!("{:.1}秒", ms as f64 / 1_000.0)
    } else if secs < 60 {
        format!("{secs}秒")
    } else if secs < 3_600 {
        format!("{}分{}秒", secs / 60, secs % 60)
    } else {
        format!("{}時間{}分", secs / 3_600, secs % 3_600 / 60)
    }
}

/// "15分ごと" / "1時間ごと" / "1時間30分ごと" / "1日ごと".
pub fn interval(minutes: u64) -> String {
    let m = minutes.max(1);
    if m.is_multiple_of(1_440) {
        format!("{}日ごと", m / 1_440)
    } else if m >= 60 && m.is_multiple_of(60) {
        format!("{}時間ごと", m / 60)
    } else if m > 60 {
        format!("{}時間{}分ごと", m / 60, m % 60)
    } else {
        format!("{m}分ごと")
    }
}

/// "毎日 5:00" / "毎日 5:00・17:00" for a daily schedule ("HH:MM"), otherwise the interval.
pub fn schedule(daily_at: &[String], interval_minutes: u64) -> String {
    if daily_at.is_empty() {
        return interval(interval_minutes);
    }
    let times: Vec<&str> = daily_at
        .iter()
        .map(|s| {
            let at = s.as_str();
            at.strip_prefix('0').filter(|s| s.len() == 4).unwrap_or(at)
        })
        .collect();
    format!("毎日 {}", times.join("・"))
}

pub fn bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.0} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Thousands separators: 12345 -> "12,345".
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Size change with its sign, e.g. "-22.49 GiB" or "+219.2 MiB".
pub fn signed_bytes(delta: i64) -> String {
    format!("{}{}", sign(delta), bytes(delta.unsigned_abs()))
}

/// File count change with its sign, e.g. "-41,352".
pub fn signed_count(delta: i64) -> String {
    format!("{}{}", sign(delta), count(delta.unsigned_abs()))
}

/// ASCII `-`: the display font draws U+2212 low enough to read as an underscore.
fn sign(delta: i64) -> &'static str {
    match delta.signum() {
        1 => "+",
        -1 => "-",
        _ => "",
    }
}

/// Comma-separated input -> trimmed, non-empty items.
pub fn split_list(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// "restic 0.17.3 compiled with go1.23 on windows/amd64" -> "0.17.3".
pub fn restic_version(output: &str) -> Option<&str> {
    output
        .split_whitespace()
        .skip_while(|w| *w != "restic")
        .nth(1)
}

/// Who started a run or operation, as recorded in the history database.
pub fn actor(source: &str) -> &str {
    match source {
        "gui" => "手動",
        "scheduler" => "定期",
        "startup" => "起動時",
        "tray" => "トレイ",
        "mcp" => "MCP",
        other => other,
    }
}

pub fn operation(op: &str) -> &str {
    match op {
        "run_backup" => "バックアップ",
        "run_backup_failed" => "バックアップ失敗",
        "backup_deferred" => "バックアップ延期（ロック待機超過）",
        "backup_skipped_remote_newer" => "他 PC が最新のためスキップ",
        "backup_skipped_interrupted_handoff" => "引き継ぎ中断のためスキップ",
        "save_secrets" => "認証情報の保存",
        "authorize_google_drive" => "Google Drive 認証",
        "authorize_google_drive_failed" => "Google Drive 認証失敗",
        "cancel_backup" => "中止",
        "init_repository" => "リポジトリの作成",
        "init_repository_failed" => "リポジトリの作成に失敗",
        "check_repository" => "整合性チェック",
        "check_repository_failed" => "整合性チェックに失敗",
        "verify_repository" => "復元確認",
        "verify_repository_deferred" => "復元確認を次回へ持ち越し",
        "verify_repository_failed" => "復元確認（問題あり）",
        "unlock_repository" => "古いロックの解除",
        "unlock_repository_failed" => "古いロックの解除に失敗",
        "update_storage_config" => "保存先設定の変更",
        "add_backup_target" => "フォルダの追加",
        "remove_backup_target" => "フォルダの削除",
        "update_schedule" => "スケジュールの変更",
        "update_retention_policy" => "保持ルールの変更",
        "restore_snapshot" => "復元",
        "forget_prune" => "古い世代の整理",
        "forget_prune_deferred" => "整理を次回へ持ち越し",
        "forget_prune_failed" => "古い世代の整理に失敗",
        "forget_snapshot" => "スナップショットの削除",
        "forget_snapshot_failed" => "スナップショットの削除に失敗",
        "refresh_sync_status" => "同期状態の確認",
        "release_project" => "プロジェクトの引き渡し",
        "release_project_failed" => "プロジェクトの引き渡しに失敗",
        "takeover_project_preview" => "引き継ぎ内容の確認",
        "takeover_safety_backup" => "引き継ぎ前の退避バックアップ",
        "takeover_project" => "プロジェクトの引き継ぎ",
        "takeover_project_failed" => "プロジェクトの引き継ぎに失敗",
        "delete_r2_preview" => "R2 削除対象の確認",
        "delete_r2_prefix" => "R2 プレフィックスの削除",
        "delete_r2_prefix_failed" => "R2 プレフィックスの削除に失敗",
        "delete_r2_bucket" => "R2 バケットの削除",
        "delete_r2_bucket_emptied" => "R2 バケットを空化（本体削除は権限不足）",
        "delete_r2_bucket_failed" => "R2 バケットの削除に失敗",
        "delete_gdrive_preview" => "Google Drive 削除対象の確認",
        "delete_gdrive_repo" => "Google Drive バックアップの削除",
        "delete_gdrive_repo_failed" => "Google Drive バックアップの削除に失敗",
        other => other,
    }
}

/// Condenses a multi-line error for compact UI surfaces (overview banner, toasts).
/// Full verbatim output remains in History and MCP.
pub fn summarize_error_for_ui(message: &str, max_chars: usize) -> String {
    let trimmed = message.trim();
    if let Some((cut, _)) = trimmed.char_indices().nth(max_chars) {
        let mut out = trimmed[..cut].to_owned();
        out.push('…');
        out
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn at(h: u32, m: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 11, 7, h, m, 0)
            .single()
            .expect("unambiguous local time")
    }

    #[test]
    fn age_parts_picks_unit_at_each_boundary() {
        let now = at(20, 0);
        assert_eq!(
            age_parts(now - Duration::seconds(59), now),
            ("たった今".to_owned(), "")
        );
        assert_eq!(
            age_parts(now - Duration::minutes(12), now),
            ("12".to_owned(), "分前")
        );
        assert_eq!(
            age_parts(now - Duration::minutes(60), now),
            ("1".to_owned(), "時間前")
        );
        assert_eq!(
            age_parts(now - Duration::hours(49), now),
            ("2".to_owned(), "日前")
        );
    }

    #[test]
    fn relative_switches_to_a_date_after_a_week() {
        let now = at(20, 0);
        assert_eq!(relative(now - Duration::days(6), now), "6日前");
        assert_eq!(relative(now - Duration::days(7), now), "10月31日");
    }

    #[test]
    fn until_rounds_minutes_up_so_it_never_says_zero() {
        let now = at(20, 0);
        assert_eq!(until(now + Duration::seconds(30), now), "まもなく");
        assert_eq!(until(now + Duration::seconds(61), now), "あと2分");
        assert_eq!(until(now + Duration::hours(3), now), "あと3時間");
    }

    #[test]
    fn day_time_names_today_and_yesterday() {
        let now = at(20, 0);
        assert_eq!(day_time(at(8, 5), now), "今日 8:05");
        assert_eq!(day_time(at(8, 5) - Duration::days(1), now), "昨日 8:05");
        assert_eq!(day_time(at(8, 5) - Duration::days(3), now), "11月4日 8:05");
    }

    #[test]
    fn interval_uses_the_largest_whole_unit() {
        assert_eq!(interval(15), "15分ごと");
        assert_eq!(interval(60), "1時間ごと");
        assert_eq!(interval(90), "1時間30分ごと");
        assert_eq!(interval(1_440), "1日ごと");
    }

    #[test]
    fn schedule_shows_the_daily_time_without_a_leading_zero() {
        assert_eq!(schedule(&["05:00".to_owned()], 60), "毎日 5:00");
        assert_eq!(
            schedule(&["05:00".to_owned(), "17:00".to_owned()], 60),
            "毎日 5:00・17:00"
        );
        assert_eq!(schedule(&[], 60), "1時間ごと");
    }

    #[test]
    fn duration_and_count_format_for_reading() {
        assert_eq!(duration_ms(840), "0.8秒");
        assert_eq!(duration_ms(192_000), "3分12秒");
        assert_eq!(count(1_204_331), "1,204,331");
        assert_eq!(count(999), "999");
    }

    #[test]
    fn signed_formats_show_a_decrease_with_a_minus_sign() {
        assert_eq!(signed_bytes(-24_149_597_695), "-22.49 GiB");
        assert_eq!(signed_bytes(229_865_130), "+219.2 MiB");
        assert_eq!(signed_bytes(0), "0 B");
        assert_eq!(signed_count(-41_352), "-41,352");
        assert_eq!(signed_count(1_129), "+1,129");
    }

    #[test]
    fn restic_version_reads_the_word_after_restic() {
        assert_eq!(
            restic_version("restic 0.17.3 compiled with go1.23.1 on windows/amd64"),
            Some("0.17.3")
        );
        assert_eq!(restic_version("garbage"), None);
    }

    #[test]
    fn split_list_drops_blanks() {
        assert_eq!(split_list(" *.log, ,tmp/ "), vec!["*.log", "tmp/"]);
    }

    #[test]
    fn summarize_error_for_ui_keeps_plain_errors_and_truncates() {
        assert_eq!(summarize_error_for_ui("plain error", 280), "plain error");
        assert_eq!(summarize_error_for_ui("abcdefghij", 5), "abcde…");
    }
}
