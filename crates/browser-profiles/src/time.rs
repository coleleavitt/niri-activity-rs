//! Timestamp conversion.
//!
//! Browser history databases use three different epochs and two different
//! precisions, sometimes within the same file: Firefox stores
//! `moz_historyvisits.visit_date` in microseconds but
//! `moz_places_metadata.created_at` in milliseconds. Every conversion lives
//! here so a caller cannot pick the wrong one by accident.

use chrono::{DateTime, Utc};

/// Microseconds between 1601-01-01 (WebKit epoch) and 1970-01-01 (Unix epoch).
const WEBKIT_EPOCH_OFFSET_MICROS: i64 = 11_644_473_600_000_000;

/// Microseconds since the Unix epoch. Zero means "unset", not 1970.
pub fn unix_micros(micros: i64) -> Option<DateTime<Utc>> {
    if micros == 0 {
        return None;
    }
    DateTime::from_timestamp_micros(micros)
}

/// Milliseconds since the Unix epoch. Zero means "unset", not 1970.
pub fn unix_millis(millis: i64) -> Option<DateTime<Utc>> {
    if millis == 0 {
        return None;
    }
    DateTime::from_timestamp_micros(millis.checked_mul(1000)?)
}

/// Microseconds since 1601-01-01, as used by every Chromium timestamp.
pub fn webkit_micros(micros: i64) -> Option<DateTime<Utc>> {
    if micros == 0 {
        return None;
    }
    DateTime::from_timestamp_micros(micros.checked_sub(WEBKIT_EPOCH_OFFSET_MICROS)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEW_YEAR_2026: &str = "2026-01-01T00:00:00+00:00";

    #[test]
    fn unix_micros_converts() {
        let dt = unix_micros(1_767_225_600_000_000).expect("valid");
        assert_eq!(dt.to_rfc3339(), NEW_YEAR_2026);
    }

    #[test]
    fn unix_millis_converts() {
        let dt = unix_millis(1_767_225_600_000).expect("valid");
        assert_eq!(dt.to_rfc3339(), NEW_YEAR_2026);
    }

    #[test]
    fn webkit_micros_converts() {
        let dt = webkit_micros(WEBKIT_EPOCH_OFFSET_MICROS + 1_767_225_600_000_000).expect("valid");
        assert_eq!(dt.to_rfc3339(), NEW_YEAR_2026);
    }

    #[test]
    fn zero_is_unset_in_every_encoding() {
        assert_eq!(unix_micros(0), None);
        assert_eq!(unix_millis(0), None);
        assert_eq!(webkit_micros(0), None);
    }

    #[test]
    fn overflow_does_not_panic() {
        assert_eq!(unix_millis(i64::MAX), None);
        assert_eq!(webkit_micros(i64::MIN), None);
    }
}
