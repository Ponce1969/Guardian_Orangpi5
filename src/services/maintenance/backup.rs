//! PostgreSQL backup: the pure parts.
//!
//! Three decisions in this file are worth stating because each one removes a
//! class of bug rather than handling it:
//!
//! - the dump name carries an ISO-8601 basic timestamp, which sorts correctly as
//!   a plain string, so retention never parses a date and cannot be wrong about a
//!   timezone or a locale;
//! - the timestamp is read from a fixed-width suffix, so a database name
//!   containing dashes cannot be mistaken for the separator;
//! - the set of files to delete is computed by a pure function that only ever
//!   returns names this daemon created. Everything else in the directory is
//!   invisible to it.
//!
//! The process itself lives in the service that consumes these; nothing here
//! touches a filesystem.

use chrono::NaiveDateTime;

/// Prefix every backup file this daemon writes.
pub const BACKUP_PREFIX: &str = "guardianrs-";

/// Suffix every backup file this daemon writes.
pub const BACKUP_SUFFIX: &str = ".dump";

/// `YYYYMMDDTHHMMSS`, the exact width the timestamp occupies.
const TIMESTAMP_FORMAT: &str = "%Y%m%dT%H%M%S";
const TIMESTAMP_LEN: usize = 15;

/// The name of the backup for `database` at `now`.
///
/// The result sorts lexicographically in chronological order, which is what
/// makes retention a string comparison instead of date arithmetic.
pub fn backup_filename(database: &str, now: NaiveDateTime) -> String {
    format!(
        "{BACKUP_PREFIX}{database}-{}{BACKUP_SUFFIX}",
        now.format(TIMESTAMP_FORMAT)
    )
}

/// Read the timestamp back out of a backup name.
///
/// The timestamp is a fixed-width suffix, so a database name containing dashes
/// is never confused with the separator. A name that does not carry a valid
/// timestamp is not ours.
fn timestamp_of(name: &str) -> Option<NaiveDateTime> {
    let stem = name
        .strip_prefix(BACKUP_PREFIX)?
        .strip_suffix(BACKUP_SUFFIX)?;

    if stem.len() < TIMESTAMP_LEN {
        return None;
    }

    // `get` rather than a byte slice: it yields None on a non-boundary instead of
    // panicking, so a corrupted or hostile filename can never take the daemon
    // down while it is deciding what to delete.
    let stamp = stem.get(stem.len() - TIMESTAMP_LEN..)?;

    NaiveDateTime::parse_from_str(stamp, TIMESTAMP_FORMAT).ok()
}

/// Which backup files to delete so that at most `keep` remain.
///
/// Only names carrying our prefix and a valid timestamp are considered; anything
/// else in the directory belongs to someone else and is never returned. The
/// newest backup is always kept, even if `keep` is zero, because a retention
/// rule that can delete the file just written is not a retention rule.
pub fn backups_to_prune(names: &[String], keep: usize) -> Vec<String> {
    // A retention rule that can delete the file it just wrote is not a rule.
    let keep = keep.max(1);

    let mut ours: Vec<(NaiveDateTime, &String)> = names
        .iter()
        .filter_map(|name| timestamp_of(name).map(|when| (when, name)))
        .collect();

    if ours.len() <= keep {
        return Vec::new();
    }

    ours.sort_by_key(|(when, _)| *when);

    let excess = ours.len() - keep;
    ours.into_iter()
        .take(excess)
        .map(|(_, name)| name.clone())
        .collect()
}

/// Replace every occurrence of `secret` with a fixed marker.
///
/// An empty secret is returned untouched rather than rewriting the text between
/// every character, which is what a naive replacement would do.
pub fn redact(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text.to_string();
    }

    text.replace(secret, "***")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(year, month, day)
            .and_then(|date| date.and_hms_opt(hour, minute, second))
            .expect("valid test timestamp")
    }

    // === backup_filename ===

    #[test]
    fn a_filename_carries_prefix_database_and_timestamp() {
        let name = backup_filename("bookgraph", at(2026, 3, 7, 3, 30, 0));

        assert_eq!(name, "guardianrs-bookgraph-20260307T033000.dump");
    }

    #[test]
    fn a_single_digit_component_is_zero_padded() {
        let name = backup_filename("db", at(2026, 1, 2, 3, 4, 5));

        assert_eq!(name, "guardianrs-db-20260102T030405.dump");
    }

    /// A database name containing a dash must not confuse the timestamp
    /// extraction, which is why the suffix is a fixed width rather than a split.
    #[test]
    fn a_dashed_database_name_round_trips_through_the_timestamp() {
        let when = at(2026, 3, 7, 3, 30, 0);
        let name = backup_filename("postgres-main-db", when);

        assert_eq!(timestamp_of(&name), Some(when));
    }

    /// A multi-byte database name must round trip too. The extraction reads a
    /// fixed byte width, so it has to stay on a character boundary.
    #[test]
    fn a_multibyte_database_name_round_trips_through_the_timestamp() {
        let when = at(2026, 3, 7, 3, 30, 0);
        let name = backup_filename("datos-añejos", when);

        assert_eq!(timestamp_of(&name), Some(when));
    }

    /// Names sort as strings in chronological order: that is the whole reason
    /// the format was chosen.
    #[test]
    fn names_sort_chronologically_as_strings() {
        let older = backup_filename("db", at(2026, 3, 7, 3, 30, 0));
        let newer = backup_filename("db", at(2026, 3, 8, 3, 30, 0));

        assert!(older < newer);
    }

    #[test]
    fn a_name_without_our_suffix_has_no_timestamp() {
        assert_eq!(timestamp_of("guardianrs-db-20260307T033000.sql"), None);
        assert_eq!(timestamp_of("random-file.dump"), None);
        assert_eq!(
            timestamp_of("guardianrs-db-20260307T033000.dump"),
            Some(at(2026, 3, 7, 3, 30, 0))
        );
    }

    #[test]
    fn a_name_with_a_malformed_timestamp_has_no_timestamp() {
        assert_eq!(timestamp_of("guardianrs-db-notatimestamp.dump"), None);
        assert_eq!(timestamp_of("guardianrs-db-20261332T033000.dump"), None);
    }

    // === backups_to_prune ===

    fn backups(count: usize) -> Vec<String> {
        (0..count)
            .map(|day| backup_filename("db", at(2026, 3, 1 + day as u32, 3, 30, 0)))
            .collect()
    }

    #[test]
    fn fewer_backups_than_the_limit_prune_nothing() {
        assert!(backups_to_prune(&backups(3), 7).is_empty());
    }

    #[test]
    fn exactly_at_the_limit_prunes_nothing() {
        assert!(backups_to_prune(&backups(7), 7).is_empty());
    }

    #[test]
    fn over_the_limit_prunes_the_oldest_first() {
        let all = backups(10);

        let pruned = backups_to_prune(&all, 7);

        assert_eq!(pruned.len(), 3);
        assert_eq!(pruned, all[..3].to_vec(), "the three oldest go");
    }

    /// The order files arrive in is the filesystem's, not ours.
    #[test]
    fn pruning_orders_by_timestamp_not_by_input_order() {
        let mut shuffled = backups(9);
        shuffled.reverse();

        let pruned = backups_to_prune(&shuffled, 7);

        assert_eq!(pruned.len(), 2);
        assert!(pruned.contains(&backup_filename("db", at(2026, 3, 1, 3, 30, 0))));
        assert!(pruned.contains(&backup_filename("db", at(2026, 3, 2, 3, 30, 0))));
    }

    /// A directory shared with anything else must stay safe.
    #[test]
    fn unrelated_files_are_never_returned() {
        let mut all = backups(9);
        all.push("important.tar.gz".to_string());
        all.push("guardianrs-other-20260301T033000.sql".to_string());
        all.push("guardianrs-broken-.dump".to_string());

        let pruned = backups_to_prune(&all, 7);

        assert_eq!(pruned.len(), 2);
        assert!(!pruned.iter().any(|name| name == "important.tar.gz"));
        assert!(!pruned.iter().any(|name| name.ends_with(".sql")));
        assert!(!pruned.iter().any(|name| name == "guardianrs-broken-.dump"));
    }

    /// A retention rule that can delete the file it just wrote is not a rule.
    #[test]
    fn a_zero_limit_still_keeps_the_newest() {
        let all = backups(3);

        let pruned = backups_to_prune(&all, 0);

        assert_eq!(pruned.len(), 2);
        assert!(!pruned.contains(&all[2]), "the newest backup must survive");
    }

    #[test]
    fn an_empty_directory_prunes_nothing() {
        assert!(backups_to_prune(&[], 7).is_empty());
    }

    // === redact ===

    #[test]
    fn a_secret_inside_the_text_is_masked() {
        assert_eq!(
            redact("connection failed for password s3cret", "s3cret"),
            "connection failed for password ***"
        );
    }

    #[test]
    fn every_occurrence_of_the_secret_is_masked() {
        assert_eq!(redact("s3cret s3cret s3cret", "s3cret"), "*** *** ***");
    }

    /// An empty secret would otherwise rewrite the text between every character.
    #[test]
    fn an_empty_secret_leaves_the_text_alone() {
        let text = "pg_dump: error al conectar";

        assert_eq!(redact(text, ""), text);
    }

    #[test]
    fn text_without_the_secret_is_unchanged() {
        let text = "pg_dump: warning: no privileges were granted";

        assert_eq!(redact(text, "s3cret"), text);
    }
}
