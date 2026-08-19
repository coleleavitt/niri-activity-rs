use std::ops::Range;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

use super::EventInterval;
use super::input::{GranularInput, proportional_between};
use crate::config::Config;
use crate::db::normalize_input_offsets;
use crate::error::Error;

pub(super) fn apply(
    conn: &Connection,
    config: &Config,
    events: Vec<EventInterval>,
) -> Result<Vec<EventInterval>, Error> {
    let Some(first_start) = events.iter().map(|event| event.start_ms).min() else {
        return Ok(events);
    };
    let Some(last_end) = events.iter().map(EventInterval::end_ms).max() else {
        return Ok(events);
    };
    let active_width =
        i64::try_from(config.idle_threshold_secs.saturating_mul(1_000)).unwrap_or(i64::MAX);
    let non_idle_width =
        i64::try_from(config.deep_idle_secs.saturating_mul(1_000)).unwrap_or(i64::MAX);
    let evidence_start = first_start.saturating_sub(non_idle_width);
    let start = timestamp(evidence_start)?;
    let end = timestamp(last_end)?;

    let mut stmt = conn.prepare(
        "WITH selected AS (
             SELECT id, timestamp,
                    active_ms + COALESCE(passive_ms, 0) + idle_ms AS total_ms,
                    input_offsets, keystrokes, mouse_clicks, scroll_events
               FROM events
              WHERE timestamp >= ?1 AND timestamp < ?2
             UNION ALL
             SELECT id, timestamp,
                    active_ms + COALESCE(passive_ms, 0) + idle_ms AS total_ms,
                    input_offsets, keystrokes, mouse_clicks, scroll_events
               FROM events
              WHERE id = (
                    SELECT id FROM events
                     WHERE timestamp < ?1
                     ORDER BY timestamp DESC, id DESC
                     LIMIT 1
              )
         )
         SELECT timestamp, total_ms, input_offsets, keystrokes, mouse_clicks, scroll_events
           FROM selected
          ORDER BY timestamp, id",
    )?;

    let rows = stmt.query_map(params![start, end], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<Vec<u8>>>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
        ))
    })?;
    let mut points = Vec::new();
    let mut unknown = Vec::new();
    for row in rows {
        let (start, total, blob, keys, clicks, scrolls) = row?;
        let row_start = parse_timestamp(&start)?;
        let duration = total.max(0);
        let row_end = row_start.saturating_add(duration);
        match blob {
            Some(blob) if blob.len() % 4 == 0 => {
                let Some(offsets) = normalize_input_offsets(&blob, duration) else {
                    unknown.push(row_start..row_end);
                    continue;
                };
                if offsets.is_empty() && keys.saturating_add(clicks).saturating_add(scrolls) > 0 {
                    unknown.push(row_start..row_end);
                } else {
                    points.extend(offsets.into_iter().map(|offset| {
                        let offset = i64::from(offset).min(duration.saturating_sub(1));
                        row_start.saturating_add(offset.max(0))
                    }));
                }
            }
            _ => unknown.push(row_start..row_end),
        }
    }

    let active = Ranges::from_points(&points, active_width);
    let non_idle = Ranges::from_points(&points, non_idle_width);
    let unknown = Ranges::from_ranges(unknown);
    let mut humanized = Vec::new();
    for event in events {
        let start = event.start_ms;
        let end = event.end_ms();
        let mut boundaries = vec![start, end];
        active.add_boundaries(start, end, &mut boundaries);
        non_idle.add_boundaries(start, end, &mut boundaries);
        unknown.add_boundaries(start, end, &mut boundaries);
        boundaries.sort_unstable();
        boundaries.dedup();
        let mut segments = Vec::new();
        let mut source_is_unknown = false;
        for bounds in boundaries.windows(2) {
            let mut segment = event.slice(bounds[0], bounds[1]);
            if unknown.intersects(segment.start_ms, segment.end_ms()) {
                source_is_unknown = true;
            } else {
                let total = segment.total_ms();
                if active.contains(segment.start_ms) {
                    segment.active_ms = total;
                    segment.passive_ms = 0;
                    segment.idle_ms = 0;
                } else if non_idle.contains(segment.start_ms) {
                    segment.active_ms = 0;
                    segment.passive_ms = total;
                    segment.idle_ms = 0;
                } else {
                    segment.active_ms = 0;
                    segment.passive_ms = 0;
                    segment.idle_ms = total;
                }
            }
            segments.push(segment);
        }
        if !source_is_unknown {
            redistribute_input(&event, &mut segments);
        }
        humanized.extend(segments);
    }
    Ok(humanized)
}

fn redistribute_input(source: &EventInterval, segments: &mut [EventInterval]) {
    let total_active = segments
        .iter()
        .filter(|segment| segment.active_ms > 0)
        .map(EventInterval::total_ms)
        .sum::<i64>();
    if total_active <= 0 {
        return;
    }
    let mut cursor = 0i64;
    for segment in segments {
        if segment.active_ms <= 0 {
            segment.keystrokes = 0;
            segment.mouse_clicks = 0;
            segment.scroll_events = 0;
            segment.mouse_distance = 0;
            segment.granular = GranularInput::default();
            continue;
        }
        let end = cursor.saturating_add(segment.total_ms());
        segment.keystrokes = proportional_between(source.keystrokes, cursor, end, total_active);
        segment.mouse_clicks = proportional_between(source.mouse_clicks, cursor, end, total_active);
        segment.scroll_events =
            proportional_between(source.scroll_events, cursor, end, total_active);
        segment.mouse_distance =
            proportional_between(source.mouse_distance, cursor, end, total_active);
        segment.granular = source.granular.slice(cursor, end, total_active);
        cursor = end;
    }
}

#[derive(Default)]
struct Ranges(Vec<Range<i64>>);

impl Ranges {
    fn from_points(points: &[i64], width: i64) -> Self {
        let ranges = points
            .iter()
            .copied()
            .map(|point| point..point.saturating_add(width));
        Self::from_ranges(ranges)
    }

    fn from_ranges(ranges: impl IntoIterator<Item = Range<i64>>) -> Self {
        let mut ranges = ranges
            .into_iter()
            .filter(|range| range.start < range.end)
            .collect::<Vec<_>>();
        ranges.sort_by_key(|range| range.start);
        let mut merged: Vec<Range<i64>> = Vec::new();
        for range in ranges {
            if let Some(last) = merged.last_mut()
                && range.start <= last.end
            {
                last.end = last.end.max(range.end);
            } else {
                merged.push(range);
            }
        }
        Self(merged)
    }

    #[cfg(test)]
    fn overlap(&self, start: i64, end: i64) -> i64 {
        let first = self.0.partition_point(|range| range.end <= start);
        self.0[first..]
            .iter()
            .take_while(|range| range.start < end)
            .map(|range| range.end.min(end).saturating_sub(range.start.max(start)))
            .sum()
    }

    fn intersects(&self, start: i64, end: i64) -> bool {
        let first = self.0.partition_point(|range| range.end <= start);
        self.0.get(first).is_some_and(|range| range.start < end)
    }

    fn contains(&self, point: i64) -> bool {
        let first = self.0.partition_point(|range| range.end <= point);
        self.0.get(first).is_some_and(|range| range.start <= point)
    }

    fn add_boundaries(&self, start: i64, end: i64, boundaries: &mut Vec<i64>) {
        let first = self.0.partition_point(|range| range.end <= start);
        for range in self.0[first..].iter().take_while(|range| range.start < end) {
            boundaries.push(range.start.max(start));
            boundaries.push(range.end.min(end));
        }
    }
}

fn parse_timestamp(value: &str) -> Result<i64, Error> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.timestamp_millis())
        .map_err(|error| Error::NiriError(format!("invalid event timestamp {value:?}: {error}")))
}

fn timestamp(value: i64) -> Result<String, Error> {
    DateTime::<Utc>::from_timestamp_millis(value)
        .map(|value| value.to_rfc3339())
        .ok_or_else(|| Error::NiriError(format!("invalid event timestamp milliseconds: {value}")))
}

#[cfg(test)]
mod tests {
    use super::Ranges;

    #[test]
    fn presence_windows_are_half_open() {
        let ranges = Ranges::from_points(&[1_000], 120_000);

        assert_eq!(ranges.overlap(1_000, 121_000), 120_000);
        assert_eq!(ranges.overlap(121_000, 122_000), 0);
    }

    #[test]
    fn overlapping_presence_windows_merge_without_double_counting() {
        let ranges = Ranges::from_points(&[1_000, 60_000], 120_000);

        assert_eq!(ranges.overlap(0, 200_000), 179_000);
    }
}
