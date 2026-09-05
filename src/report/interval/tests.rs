use super::*;
use crate::db::{init_db, run_migrations};

fn event(start_ms: i64, active_ms: i64, passive_ms: i64, idle_ms: i64) -> EventInterval {
    EventInterval {
        source_start_ms: start_ms,
        start_ms,
        app_id: "code".to_string(),
        title: String::new(),
        category: Category::Productive,
        project: None,
        active_ms,
        passive_ms,
        idle_ms,
        agent_ms: Some(active_ms),
        keystrokes: 101,
        mouse_clicks: 11,
        scroll_events: 7,
        mouse_distance: 503,
        granular: GranularInput::default(),
        jiggler_detected: false,
    }
}

#[test]
fn clipping_uses_half_open_boundaries() {
    let event = event(60_000, 60_000, 0, 0);

    assert!(event.clone().clip(120_000, 180_000).is_none());
    assert!(event.clone().clip(0, 60_000).is_none());
    assert_eq!(
        event.clip(90_000, 180_000).expect("overlap").total_ms(),
        30_000
    );
}

#[test]
fn minute_slices_conserve_duration_and_counters() {
    let event = event(30_000, 40_000, 20_000, 60_000);

    let slices = event.minute_slices();

    assert_eq!(slices.len(), 3);
    assert_eq!(
        slices.iter().map(EventInterval::total_ms).sum::<i64>(),
        120_000
    );
    assert_eq!(
        slices.iter().map(|slice| slice.active_ms).sum::<i64>(),
        40_000
    );
    assert_eq!(
        slices.iter().map(|slice| slice.passive_ms).sum::<i64>(),
        20_000
    );
    assert_eq!(
        slices.iter().map(|slice| slice.idle_ms).sum::<i64>(),
        60_000
    );
    assert_eq!(
        slices.iter().map(|slice| slice.keystrokes).sum::<i64>(),
        101
    );
}

#[test]
fn loader_includes_the_single_overlapping_predecessor() {
    let mut conn = Connection::open_in_memory().expect("database");
    init_db(&conn).expect("schema");
    run_migrations(&mut conn, &Config::default()).expect("migrations");
    conn.execute(
        "INSERT INTO events (timestamp, app_id, title, category, active_ms, idle_ms)
         VALUES ('2026-01-01T23:59:00+00:00', 'code', '', 'productive', 120000, 0)",
        [],
    )
    .expect("event");

    let events = load_overlapping(
        &conn,
        "2026-01-02T00:00:00+00:00",
        "2026-01-03T00:00:00+00:00",
    )
    .expect("load");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].start_ms, 1_767_312_000_000);
    assert_eq!(events[0].total_ms(), 60_000);
}

#[test]
fn minute_slices_telescope_each_presence_dimension_with_tiny_counts() {
    let event = event(59_999, 1, 1, 1);

    let slices = event.minute_slices();

    assert_eq!(slices.len(), 2);
    assert_eq!(slices.iter().map(|slice| slice.active_ms).sum::<i64>(), 1);
    assert_eq!(slices.iter().map(|slice| slice.passive_ms).sum::<i64>(), 1);
    assert_eq!(slices.iter().map(|slice| slice.idle_ms).sum::<i64>(), 1);
}

#[test]
fn complementary_clips_telescope_each_presence_dimension() {
    let event = event(0, 1, 1, 1);

    let left = event.clone().clip(0, 1).expect("left clip");
    let right = event.clone().clip(1, 3).expect("right clip");

    assert_eq!(left.active_ms + right.active_ms, event.active_ms);
    assert_eq!(left.passive_ms + right.passive_ms, event.passive_ms);
    assert_eq!(left.idle_ms + right.idle_ms, event.idle_ms);
}
