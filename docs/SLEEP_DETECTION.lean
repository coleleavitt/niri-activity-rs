# Sleep & Presence Detection — Algorithm Specification
# File: docs/SLEEP_DETECTION.lean  (design spec, not executable Lean yet)
# Author: automated analysis of activity.db, Aug 2026
# Status: DRAFT. The live Away-ordering fix is implemented in src/watcher.rs.
#         Persisted sleep reclassification is intentionally NOT implemented: it
#         needs boundary-safe interval splitting and report/export conservation.

## 0. Problem statement

The tracker records one `events` row per session flush with fields:
  timestamp (UTC), app_id, active_ms, passive_ms, idle_ms, keystrokes, mouse_clicks,
  agent_ms, input_offsets (BLOB of per-event ms offsets).

Two failures inflate "screen time" while the user is asleep:

  (B1) Agent-keeps-alive: an AI coding agent writes session files all night. The live
       state machine (watcher.rs::compute_activity_state) returns Active whenever
       `agent_active`, BEFORE testing the Away threshold. So the machine never goes
       Away and every 5-min periodic flush records 5 more minutes of PASSIVE presence.
       Evidence: Wed 2026-08-19, 01:18–07:17 local, 0 keystrokes, ~6.0h logged as
       passive; 87 of 90 overnight rows have agent_ms == passive_ms.

  (B2) No sleep model at all: long no-input stretches are only ever "idle" or "away"
       via a fixed 30-min threshold, with no notion of a sustained sleep block, no
       bridging of brief awakenings, and no circadian prior.

Ground truth from the DB (user self-reports poor/fragmented sleep):
  - Main sleep is usually ONE block 5.5–13h, anchored 20:00–03:00 start, 06:00–10:00 wake.
  - Fragmented nights exist: Wed 08-19 = 01:18→03:53→05:21→07:17 with 1–2 keystroke
    stirs between blocks (rolled over / checked phone). These must be treated as ONE
    sleep, not three.
  - Naps / long away daytime blocks exist (e.g. 08-04 afternoon) and must NOT be
    mislabeled as the main sleep, but SHOULD still be excluded from "active presence".

## 1. Design goals

  G1. Do not count sleep as screen/presence/active/productive time.
  G2. Agent activity may be recorded as `agent_ms` (machine worked), but agent activity
      ALONE (no human input for a long time) must NOT keep the user "present".
  G3. Robust to fragmented sleep: bridge brief awakenings.
  G4. Robust to the runaway title-flush bug: decisions key off INPUT (keystrokes,
      mouse_clicks, input_offsets), never off row count or focus churn.
  G5. Deterministic and explainable: every reclassified minute traces to a rule.
  G6. Config-driven thresholds; no magic numbers in code.

## 2. Data model for detection

Reduce events to a 1-minute PRESENCE grid over the report window (local time):

  for each minute m:
    input_count[m]   = sum(keystrokes + mouse_clicks) whose input_offset lands in m
    active_ms[m]     = active_ms attributed to m (from input_offsets replay)
    agent_ms[m]      = agent_ms attributed to m
    has_human[m]     = input_count[m] > 0

Using input_offsets (not the coarse row) gives true per-minute human input and is
immune to B1/runaway because those rows carry 0 human input.

## 3. Core algorithm — "Sustained Rest with Awakening Bridge" (SRAB)

Derived from actigraphy practice (Cole–Kripke / Sadeh use short epochs + rescoring;
van Hees HDCZA finds the longest sustained-inactivity bout anchored to a circadian
window). SRAB adapts the "longest sustained inactivity bout + rescore short awakenings"
idea to keyboard/mouse input.

Parameters (defaults, all config-overridable under [sleep]):
  REST_GAP_MIN        = 25   # min minutes with no human input to start a rest gap
  WAKE_BRIDGE_MIN     = 25   # an awakening shorter than this may be bridged
  WAKE_BRIDGE_EVENTS  = 8    # ...and with <= this many input events (a stir, not waking)
  MIN_SLEEP_MIN       = 180  # a rest period must reach 3h to be labeled SLEEP
  NAP_MIN             = 45    # rest 45min..MIN_SLEEP counts as NAP (excluded from active,
                             #   but not from "day")
  NIGHT_ANCHOR        = 20:00..11:00  # main-sleep search window (soft prior, not a hard gate)

Procedure (per report, over the local window):
  1. Extract sorted human-input timestamps from input_offsets (fallback: row timestamp
     if a row has input but no offsets blob).
  2. Form REST gaps: consecutive human-input times separated by >= REST_GAP_MIN.
  3. Bridge: merge rest[j] into rest[j-1] iff the awakening between them satisfies
     BOTH awake_duration <= WAKE_BRIDGE_MIN AND awake_events <= WAKE_BRIDGE_EVENTS.
     (This is why Wed 08-19's 1–2 keystroke stirs merge into one 6.0h sleep.)
  4. Label each merged rest R = [start,end]:
       dur = end - start
       if dur >= MIN_SLEEP_MIN                    -> SLEEP
       elif dur >= NAP_MIN                          -> NAP
       else                                         -> BREAK (short idle, keep as idle)
  5. Main sleep = the SLEEP block whose midpoint is nearest 03:30 local AND that
     overlaps NIGHT_ANCHOR; others are secondary sleeps/naps.

## 4. Persistence safety requirements (not implemented)

A future persisted sleep bucket must be fail-closed. It must preserve the physical row
span separately from counted human presence:

  span_ms     = active_ms + passive_ms + idle_ms + sleep_ms
  presence_ms = active_ms + passive_ms + idle_ms

It must split partially overlapping rows at exact half-open boundaries, conserve all
state and input counters, retain agent overlap through reports and every export, and
skip any legacy/ambiguous row that cannot be split exactly. Detection must use the same
validated [sleep] configuration as reports and require bounding input evidence on both
sides of a candidate. Until those invariants are implemented and tested, no command may
rewrite production activity rows based on inferred sleep.

## 5. Live daemon fix (watcher.rs) — stop B1 at the source

compute_activity_state ordering MUST become:
    if idle_duration_ms > away_ms            -> Away        # human-away wins
    else if agent_active && idle < away_ms   -> Active      # agent counts only while
                                                            #   human recently present
    else if idle > deep_idle                 -> Idle
    else if idle > idle_ms                    -> Passive
    else                                       -> Active
i.e. AWAY is tested BEFORE the agent override. Agent activity may still be recorded to
agent_ms, but it may not hold presence past away_threshold_secs of no human input.
Additionally: gate `jfc_streaming_title_active` behind idle_duration < away_ms so a
stale spinner title cannot pin Active forever.

## 6. Exploratory evidence (not ground truth)

An Aug 2–19 activity.db exploration produced a 9.2h median candidate rest block and
merged the fragmented Aug 19 gaps into one 01:18–07:17 candidate. These are heuristic
results, not verified sleep ground truth, and must not authorize destructive repair.

## 7. Open questions / future

  - Personalize NIGHT_ANCHOR from the user's own 30-day sleep-onset distribution.
  - Weight by mouse_distance and scroll for micro-activity (currently ks+mc only).
  - Consider Cole–Kripke epoch scoring if we later store finer 30s activity counts.
  - Optionally cross-check with logind suspend log and (opt-in) a phone/wearable feed.
