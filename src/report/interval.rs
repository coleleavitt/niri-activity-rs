use std::str::FromStr;

use chrono::{DateTime, FixedOffset, Local, Utc};
use rusqlite::{Connection, params};

use crate::config::{Category, Config};
use crate::error::Error;

mod input;
mod presence;
use input::{GranularInput, proportional_between};

const MS_PER_MINUTE: i64 = 60_000;

#[derive(Debug, Clone)]
pub(super) struct EventInterval {
    pub(super) source_start_ms: i64,
    pub(super) start_ms: i64,
    pub(super) app_id: String,
    pub(super) title: String,
    pub(super) category: Category,
    pub(super) project: Option<String>,
    pub(super) active_ms: i64,
    pub(super) passive_ms: i64,
    pub(super) idle_ms: i64,
    pub(super) agent_ms: Option<i64>,
    pub(super) keystrokes: i64,
    pub(super) mouse_clicks: i64,
    pub(super) scroll_events: i64,
    pub(super) mouse_distance: i64,
    pub(super) granular: GranularInput,
    pub(super) jiggler_detected: bool,
}

impl EventInterval {
    pub(super) fn total_ms(&self) -> i64 {
        self.active_ms
            .saturating_add(self.passive_ms)
            .saturating_add(self.idle_ms)
    }

    pub(super) fn end_ms(&self) -> i64 {
        self.start_ms.saturating_add(self.total_ms())
    }

    pub(super) fn local_start(&self, config: &Config) -> Option<DateTime<FixedOffset>> {
        let utc = DateTime::<Utc>::from_timestamp_millis(self.start_ms)?;
        if let Some(timezone) = config.timezone {
            Some(utc.with_timezone(&timezone).fixed_offset())
        } else {
            Some(utc.with_timezone(&Local).fixed_offset())
        }
    }

    pub(super) fn minute_slices(&self) -> Vec<Self> {
        let mut slices = Vec::new();
        let end_ms = self.end_ms();
        let mut cursor = self.start_ms;
        while cursor < end_ms {
            let next_boundary = cursor
                .div_euclid(MS_PER_MINUTE)
                .saturating_add(1)
                .saturating_mul(MS_PER_MINUTE);
            let slice_end = next_boundary.min(end_ms);
            slices.push(self.slice(cursor, slice_end));
            cursor = slice_end;
        }
        slices
    }

    fn clip(self, window_start_ms: i64, window_end_ms: i64) -> Option<Self> {
        let clip_start = self.start_ms.max(window_start_ms);
        let clip_end = self.end_ms().min(window_end_ms);
        (clip_start < clip_end).then(|| self.slice(clip_start, clip_end))
    }

    fn slice(&self, slice_start_ms: i64, slice_end_ms: i64) -> Self {
        let total_ms = self.total_ms();
        let start_offset = slice_start_ms.saturating_sub(self.start_ms);
        let end_offset = slice_end_ms.saturating_sub(self.start_ms);
        Self {
            source_start_ms: self.source_start_ms,
            start_ms: slice_start_ms,
            app_id: self.app_id.clone(),
            title: self.title.clone(),
            category: self.category,
            project: self.project.clone(),
            active_ms: proportional_between(self.active_ms, start_offset, end_offset, total_ms),
            passive_ms: proportional_between(self.passive_ms, start_offset, end_offset, total_ms),
            idle_ms: proportional_between(self.idle_ms, start_offset, end_offset, total_ms),
            agent_ms: self
                .agent_ms
                .map(|agent_ms| proportional_between(agent_ms, start_offset, end_offset, total_ms)),
            keystrokes: proportional_between(self.keystrokes, start_offset, end_offset, total_ms),
            mouse_clicks: proportional_between(
                self.mouse_clicks,
                start_offset,
                end_offset,
                total_ms,
            ),
            scroll_events: proportional_between(
                self.scroll_events,
                start_offset,
                end_offset,
                total_ms,
            ),
            mouse_distance: proportional_between(
                self.mouse_distance,
                start_offset,
                end_offset,
                total_ms,
            ),
            granular: self.granular.slice(start_offset, end_offset, total_ms),
            jiggler_detected: self.jiggler_detected,
        }
    }
}

pub(super) fn load_overlapping(
    conn: &Connection,
    window_start: &str,
    window_end: &str,
) -> Result<Vec<EventInterval>, Error> {
    let window_start_ms = parse_timestamp(window_start)?;
    let window_end_ms = parse_timestamp(window_end)?;
    if window_end_ms <= window_start_ms {
        return Ok(Vec::new());
    }

    let mut stmt = conn.prepare(
        "SELECT id, timestamp, app_id, COALESCE(title,''), category, active_ms, COALESCE(passive_ms,0), idle_ms,
                agent_ms, keystrokes, mouse_clicks, scroll_events, mouse_distance,
                jiggler_detected, project, backspace_count, modifier_count, left_clicks,
                right_clicks, middle_clicks, scroll_up, scroll_down, scroll_horizontal
           FROM events
          WHERE timestamp >= ?1 AND timestamp < ?2
         UNION ALL
         SELECT id, timestamp, app_id, COALESCE(title,''), category, active_ms, COALESCE(passive_ms,0), idle_ms,
                agent_ms, keystrokes, mouse_clicks, scroll_events, mouse_distance,
                jiggler_detected, project, backspace_count, modifier_count, left_clicks,
                right_clicks, middle_clicks, scroll_up, scroll_down, scroll_horizontal
           FROM events
          WHERE id = (
                SELECT id FROM events
                 WHERE timestamp < ?1
                 ORDER BY timestamp DESC, id DESC
                 LIMIT 1
          )
         ORDER BY timestamp, id",
    )?;

    let rows = stmt.query_map(params![window_start, window_end], |row| {
        Ok((
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, Option<i64>>(8)?,
            row.get::<_, i64>(9)?,
            row.get::<_, i64>(10)?,
            row.get::<_, i64>(11)?,
            row.get::<_, i64>(12)?,
            row.get::<_, i64>(13)? != 0,
            row.get::<_, Option<String>>(14)?,
            row.get::<_, i64>(15)?,
            row.get::<_, i64>(16)?,
            row.get::<_, i64>(17)?,
            row.get::<_, i64>(18)?,
            row.get::<_, i64>(19)?,
            row.get::<_, i64>(20)?,
            row.get::<_, i64>(21)?,
            row.get::<_, i64>(22)?,
        ))
    })?;

    let mut events = Vec::new();
    for row in rows {
        let (
            timestamp,
            app_id,
            title,
            category,
            active_ms,
            passive_ms,
            idle_ms,
            agent_ms,
            keystrokes,
            mouse_clicks,
            scroll_events,
            mouse_distance,
            jiggler_detected,
            project,
            backspace_count,
            modifier_count,
            left_clicks,
            right_clicks,
            middle_clicks,
            scroll_up,
            scroll_down,
            scroll_horizontal,
        ) = row?;
        let source_start_ms = parse_timestamp(&timestamp)?;
        let event = EventInterval {
            source_start_ms,
            start_ms: source_start_ms,
            app_id,
            title,
            category: Category::from_str(&category).unwrap_or(Category::Neutral),
            project,
            active_ms: active_ms.max(0),
            passive_ms: passive_ms.max(0),
            idle_ms: idle_ms.max(0),
            agent_ms: agent_ms.map(|value| value.max(0)),
            keystrokes: keystrokes.max(0),
            mouse_clicks: mouse_clicks.max(0),
            scroll_events: scroll_events.max(0),
            mouse_distance: mouse_distance.max(0),
            granular: GranularInput {
                backspace_count: backspace_count.max(0),
                modifier_count: modifier_count.max(0),
                left_clicks: left_clicks.max(0),
                right_clicks: right_clicks.max(0),
                middle_clicks: middle_clicks.max(0),
                scroll_up: scroll_up.max(0),
                scroll_down: scroll_down.max(0),
                scroll_horizontal: scroll_horizontal.max(0),
            },
            jiggler_detected,
        };
        if let Some(clipped) = event.clip(window_start_ms, window_end_ms) {
            events.push(clipped);
        }
    }
    Ok(events)
}

pub(super) fn load_human_intervals(
    conn: &Connection,
    config: &Config,
    window_start: &str,
    window_end: &str,
) -> Result<Vec<EventInterval>, Error> {
    let events = load_overlapping(conn, window_start, window_end)?;
    presence::apply(conn, config, events)
}

fn parse_timestamp(value: &str) -> Result<i64, Error> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.timestamp_millis())
        .map_err(|error| Error::NiriError(format!("invalid event timestamp {value:?}: {error}")))
}

#[cfg(test)]
mod tests;
