use std::time::Duration;

use crossterm::event::{self, KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Padding, Paragraph, Row, Table, Tabs};
use ratatui::{DefaultTerminal, Frame};

use crate::error::Error;
use crate::fmt::{fmt_distance, fmt_duration, fmt_duration_compact, pct};
use crate::report::{
    App, MetricsData, ReportData, TimeRange, TimelineData, TodayData, query_metrics_range,
    query_report_range, query_timeline, query_today,
};
use crate::theme::{THEME, category_style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Dashboard,
    Timeline,
    Apps,
    Report,
}

const TABS: &[Tab] = &[Tab::Dashboard, Tab::Timeline, Tab::Apps, Tab::Report];

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Dashboard => " Dashboard ",
            Tab::Timeline => " Timeline ",
            Tab::Apps => " Apps ",
            Tab::Report => " Report ",
        }
    }

    fn index(self) -> usize {
        // Compile-time assertion: TABS length must match variant count
        const _: () = assert!(
            TABS.len() == 4,
            "TABS array length must match Tab variant count"
        );
        TABS.iter().position(|&t| t == self).unwrap_or(0)
    }
}

#[allow(dead_code)]
struct TuiApp {
    tab: Tab,
    range: TimeRange,
    today: Option<TodayData>,
    metrics: Option<MetricsData>,
    timeline: Option<TimelineData>,
    report: Option<ReportData>,
    mouse_dpi: f64,
    schedule_start: String,
    schedule_end: String,
    quit: bool,
}

impl TuiApp {
    fn load(range: TimeRange) -> Result<Self, Error> {
        let app = App::open()?;
        let today = query_today(&app).ok();
        let metrics = query_metrics_range(&app, range.clone()).ok();
        let timeline = query_timeline(&app, 0, 15).ok();
        let report = query_report_range(&app, range.clone()).ok();
        let mouse_dpi = app.config.mouse_dpi;
        let schedule_start = app.config.schedule.start.clone();
        let schedule_end = app.config.schedule.end.clone();

        Ok(Self {
            tab: Tab::Dashboard,
            range,
            today,
            metrics,
            timeline,
            report,
            mouse_dpi,
            schedule_start,
            schedule_end,
            quit: false,
        })
    }

    fn reload(&mut self) {
        if let Ok(app) = App::open() {
            self.today = query_today(&app).ok();
            self.metrics = query_metrics_range(&app, self.range.clone()).ok();
            self.timeline = query_timeline(&app, 0, 15).ok();
            self.report = query_report_range(&app, self.range.clone()).ok();
        }
    }

    fn next_tab(&mut self) {
        let idx = self.tab.index();
        let next = (idx + 1) % TABS.len();
        self.tab = TABS[next];
    }

    fn prev_tab(&mut self) {
        let idx = self.tab.index();
        let prev = if idx == 0 { TABS.len() - 1 } else { idx - 1 };
        self.tab = TABS[prev];
    }
}

#[allow(dead_code)]
pub fn run_tui(days: u32) -> Result<(), Error> {
    run_tui_range(TimeRange::Days(days))
}

pub fn run_tui_range(range: TimeRange) -> Result<(), Error> {
    let mut app = TuiApp::load(range)?;
    let terminal = ratatui::init();
    let result = run_loop(&mut app, terminal);
    ratatui::restore();
    result
}

fn run_loop(app: &mut TuiApp, mut terminal: DefaultTerminal) -> Result<(), Error> {
    loop {
        terminal
            .draw(|frame| render(app, frame))
            .map_err(|e| Error::Io(e))?;
        if app.quit {
            return Ok(());
        }
        let timeout = Duration::from_millis(250);
        if event::poll(timeout)? {
            match event::read() {
                Ok(crossterm::event::Event::Key(key)) => handle_key(app, key),
                Ok(_) => {}
                Err(e) => return Err(Error::Io(e)),
            }
        }
    }
}

fn handle_key(app: &mut TuiApp, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => app.next_tab(),
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => app.prev_tab(),
        KeyCode::Char('r') => app.reload(),
        _ => {}
    }
}

fn render(app: &TuiApp, frame: &mut Frame) {
    let area = frame.area();
    let layout = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ]);
    let [title_area, content_area, footer_area] = area.layout(&layout);

    render_title_bar(app, title_area, frame);
    render_tab_content(app, content_area, frame);
    render_footer(footer_area, frame);
}

fn render_title_bar(app: &TuiApp, area: Rect, frame: &mut Frame) {
    let layout = Layout::horizontal([Constraint::Min(0), Constraint::Length(50)]);
    let [title, tabs_area] = area.layout(&layout);

    let title_span = Span::styled(" niri-activity-rs", THEME.title);
    frame.render_widget(title_span, title);

    let tab_titles: Vec<&str> = TABS.iter().map(|t| t.label()).collect();
    let tabs = Tabs::new(tab_titles)
        .style(THEME.tab_inactive)
        .highlight_style(THEME.tab_active)
        .select(app.tab.index())
        .divider("")
        .padding("", "");
    frame.render_widget(tabs, tabs_area);
}

fn render_footer(area: Rect, frame: &mut Frame) {
    let keys = [("Tab/←→", "Switch"), ("r", "Reload"), ("q", "Quit")];
    let spans: Vec<Span> = keys
        .iter()
        .flat_map(|(key, desc)| {
            vec![
                Span::styled(format!(" {} ", key), THEME.key_hint),
                Span::styled(format!(" {} ", desc), THEME.key_label),
            ]
        })
        .collect();
    let line = Line::from(spans).centered();
    frame.render_widget(line, area);
}

fn render_tab_content(app: &TuiApp, area: Rect, frame: &mut Frame) {
    match app.tab {
        Tab::Dashboard => render_dashboard(app, area, frame),
        Tab::Timeline => render_timeline(app, area, frame),
        Tab::Apps => render_apps(app, area, frame),
        Tab::Report => render_report(app, area, frame),
    }
}

// ── Dashboard Tab ─────────────────────────────────────────

fn render_dashboard(app: &TuiApp, area: Rect, frame: &mut Frame) {
    let layout = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(5),
        Constraint::Min(0),
    ]);
    let [gauges_area, metrics_area, today_area] = area.layout(&layout);

    render_productivity_gauges(app, gauges_area, frame);
    render_metrics_summary(app, metrics_area, frame);
    render_today_table(app, today_area, frame);
}

fn render_productivity_gauges(app: &TuiApp, area: Rect, frame: &mut Frame) {
    let Some(ref m) = app.metrics else {
        frame.render_widget(Paragraph::new("No data").centered(), area);
        return;
    };

    let total = m.total_ms.max(1);
    let prod_ratio = m.productive_ms as f64 / total as f64;
    let unprod_ratio = m.unproductive_ms as f64 / total as f64;
    let active_ratio = m.productive_active_ms as f64 / m.productive_ms.max(1) as f64;

    let layout = Layout::horizontal([
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
    ]);
    let [g1, g2, g3] = area.layout(&layout);

    let gauge1 = Gauge::default()
        .block(
            Block::new()
                .borders(Borders::ALL)
                .border_style(THEME.border)
                .title(Line::from(" Productive ").style(THEME.productive)),
        )
        .gauge_style(THEME.gauge_productive)
        .ratio(prod_ratio.clamp(0.0, 1.0))
        .label(format!("{:.0}%", prod_ratio * 100.0));
    frame.render_widget(gauge1, g1);

    let gauge2 = Gauge::default()
        .block(
            Block::new()
                .borders(Borders::ALL)
                .border_style(THEME.border)
                .title(Line::from(" Unproductive ").style(THEME.unproductive)),
        )
        .gauge_style(THEME.gauge_unproductive)
        .ratio(unprod_ratio.clamp(0.0, 1.0))
        .label(format!("{:.0}%", unprod_ratio * 100.0));
    frame.render_widget(gauge2, g2);

    let gauge3 = Gauge::default()
        .block(
            Block::new()
                .borders(Borders::ALL)
                .border_style(THEME.border)
                .title(Line::from(" Active (of prod) ").style(THEME.accent)),
        )
        .gauge_style(THEME.gauge_productive)
        .ratio(active_ratio.clamp(0.0, 1.0))
        .label(format!("{:.0}%", active_ratio * 100.0));
    frame.render_widget(gauge3, g3);
}

fn render_metrics_summary(app: &TuiApp, area: Rect, frame: &mut Frame) {
    let Some(ref m) = app.metrics else {
        return;
    };

    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(THEME.border)
        .title(
            Line::from(format!(
                " Metrics ({} day{}) ",
                m.days,
                if m.days == 1 { "" } else { "s" }
            ))
            .style(THEME.header),
        )
        .padding(Padding::horizontal(1));

    let lines = vec![
        Line::from(vec![
            Span::raw("Total: "),
            Span::styled(fmt_duration(m.total_ms), THEME.value),
            Span::raw("  "),
            Span::styled(
                format!("Prod: {}", fmt_duration(m.productive_ms)),
                THEME.productive,
            ),
            Span::raw("  "),
            Span::styled(
                format!("Unprod: {}", fmt_duration(m.unproductive_ms)),
                THEME.unproductive,
            ),
            Span::raw("  "),
            Span::styled(
                format!("Neutral: {}", fmt_duration(m.neutral_ms)),
                THEME.neutral,
            ),
        ]),
        Line::from(vec![
            Span::raw("Productive ─ "),
            Span::styled(
                format!("Active: {}", fmt_duration(m.productive_active_ms)),
                THEME.productive,
            ),
            Span::raw("  "),
            Span::styled(
                format!("Passive: {}", fmt_duration(m.productive_passive_ms)),
                THEME.neutral,
            ),
            Span::raw("  "),
            Span::styled(
                format!("Idle: {}", fmt_duration(m.productive_idle_ms)),
                THEME.value_dim,
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_today_table(app: &TuiApp, area: Rect, frame: &mut Frame) {
    let Some(ref today) = app.today else {
        frame.render_widget(Paragraph::new("No activity today").centered(), area);
        return;
    };

    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(THEME.border)
        .title(Line::from(format!(" Today ({}) ", today.date)).style(THEME.header));

    let header = Row::new(vec![
        Cell::from("Application").style(THEME.table_header),
        Cell::from("Category").style(THEME.table_header),
        Cell::from("Time").style(THEME.table_header),
    ]);

    let rows: Vec<Row> = today
        .rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let base = if i % 2 == 0 {
                THEME.table_row
            } else {
                THEME.table_row_alt
            };
            let cat_style = category_style(r.category);
            let h = r.total_ms / 3_600_000;
            let m = (r.total_ms % 3_600_000) / 60_000;
            let s = (r.total_ms % 60_000) / 1000;
            Row::new(vec![
                Cell::from(r.app_id.clone()).style(cat_style),
                Cell::from(format!("{}", r.category)).style(cat_style),
                Cell::from(format!("{}h {:02}m {:02}s", h, m, s)).style(THEME.value),
            ])
            .style(base)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Min(25),
            Constraint::Length(14),
            Constraint::Length(12),
        ],
    )
    .header(header)
    .block(block);

    frame.render_widget(table, area);
}

// ── Timeline Tab ──────────────────────────────────────────

fn render_timeline(app: &TuiApp, area: Rect, frame: &mut Frame) {
    let Some(ref tl) = app.timeline else {
        frame.render_widget(Paragraph::new("No timeline data").centered(), area);
        return;
    };

    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(THEME.border)
        .title(
            Line::from(format!(" Timeline {} ({}min) ", tl.date, tl.bucket_min))
                .style(THEME.header),
        );

    let header = Row::new(vec![
        Cell::from("Time").style(THEME.table_header),
        Cell::from("Bar").style(THEME.table_header),
        Cell::from("Duration").style(THEME.table_header),
        Cell::from("Keys").style(THEME.table_header),
        Cell::from("App").style(THEME.table_header),
        Cell::from("Status").style(THEME.table_header),
    ]);

    let rows: Vec<Row> = tl
        .buckets
        .iter()
        .map(|b| {
            let total = b.productive_ms + b.neutral_ms + b.unproductive_ms;
            if total == 0 {
                return Row::new(vec![Cell::from(""); 6]);
            }

            let idle_total = b.idle_ms;
            let idle_pct = if total > 0 {
                idle_total as f64 / total as f64
            } else {
                0.0
            };

            let bar_width = 20usize;
            let prod_chars =
                (b.productive_ms as f64 / total as f64 * bar_width as f64).round() as usize;
            let neut_chars =
                (b.neutral_ms as f64 / total as f64 * bar_width as f64).round() as usize;
            let unp_chars = bar_width.saturating_sub(prod_chars + neut_chars);

            let bar_line = Line::from(vec![
                Span::styled("█".repeat(prod_chars), THEME.bar_productive),
                Span::styled("█".repeat(neut_chars), THEME.bar_neutral),
                Span::styled("█".repeat(unp_chars), THEME.bar_unproductive),
            ]);

            let status = if idle_pct > 0.8 {
                Cell::from("AFK").style(THEME.warning)
            } else if idle_pct > 0.5 {
                Cell::from("idle").style(THEME.neutral)
            } else {
                Cell::from("").style(THEME.value_dim)
            };

            Row::new(vec![
                Cell::from(format!("{:02}:{:02}", b.hour, b.minute)).style(THEME.value_dim),
                Cell::from(bar_line),
                Cell::from(fmt_duration_compact(total)).style(THEME.value),
                Cell::from(format!("{}", b.keystrokes)).style(THEME.value_dim),
                Cell::from(b.dominant_app.clone()).style(THEME.accent),
                status,
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Length(22),
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Min(15),
            Constraint::Length(6),
        ],
    )
    .header(header)
    .block(block);

    frame.render_widget(table, area);
}

// ── Apps Tab ──────────────────────────────────────────────

fn render_apps(app: &TuiApp, area: Rect, frame: &mut Frame) {
    let Some(ref rpt) = app.report else {
        frame.render_widget(Paragraph::new("No app data").centered(), area);
        return;
    };

    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(THEME.border)
        .title(Line::from(" Top Applications ").style(THEME.header));

    let total_ms = rpt.total_ms.max(1);

    let header = Row::new(vec![
        Cell::from("Application").style(THEME.table_header),
        Cell::from("Bar").style(THEME.table_header),
        Cell::from("Time").style(THEME.table_header),
        Cell::from("Keys").style(THEME.table_header),
        Cell::from("Clicks").style(THEME.table_header),
        Cell::from("Category").style(THEME.table_header),
    ]);

    let mut rows: Vec<Row> = Vec::new();

    for (row_idx, group) in rpt.top_apps.iter().enumerate() {
        let base = if row_idx.is_multiple_of(2) {
            THEME.table_row
        } else {
            THEME.table_row_alt
        };

        if group.children.len() == 1 {
            let a = &group.children[0];
            let bar_len = (a.total_ms as f64 / total_ms as f64 * 20.0).round() as usize;
            let bar_span = Span::styled("█".repeat(bar_len), category_style(a.category));

            rows.push(
                Row::new(vec![
                    Cell::from(a.app_id.clone()).style(category_style(a.category)),
                    Cell::from(Line::from(bar_span)),
                    Cell::from(fmt_duration(a.total_ms)).style(THEME.value),
                    Cell::from(format!("{}", a.keys)).style(THEME.value_dim),
                    Cell::from(format!("{}", a.clicks)).style(THEME.value_dim),
                    Cell::from(format!("{}", a.category)).style(category_style(a.category)),
                ])
                .style(base),
            );
        } else {
            let bar_len = (group.total_ms as f64 / total_ms as f64 * 20.0).round() as usize;
            let mut prod_ms: i64 = 0;
            let mut neutral_ms: i64 = 0;
            let mut unprod_ms: i64 = 0;
            for child in &group.children {
                match child.category {
                    crate::config::Category::Productive => prod_ms += child.total_ms,
                    crate::config::Category::Neutral => neutral_ms += child.total_ms,
                    crate::config::Category::Unproductive => unprod_ms += child.total_ms,
                }
            }
            let total = (prod_ms + neutral_ms + unprod_ms).max(1);
            let prod_chars = (prod_ms as f64 / total as f64 * bar_len as f64).round() as usize;
            let neutral_chars =
                (neutral_ms as f64 / total as f64 * bar_len as f64).round() as usize;
            let unprod_chars = bar_len.saturating_sub(prod_chars + neutral_chars);
            let bar_line = Line::from(vec![
                Span::styled(
                    "█".repeat(prod_chars),
                    category_style(crate::config::Category::Productive),
                ),
                Span::styled(
                    "█".repeat(neutral_chars),
                    category_style(crate::config::Category::Neutral),
                ),
                Span::styled(
                    "█".repeat(unprod_chars),
                    category_style(crate::config::Category::Unproductive),
                ),
            ]);

            rows.push(
                Row::new(vec![
                    Cell::from(group.app_id.clone()).style(THEME.value),
                    Cell::from(bar_line),
                    Cell::from(fmt_duration(group.total_ms)).style(THEME.value),
                    Cell::from(format!("{}", group.keys)).style(THEME.value_dim),
                    Cell::from(format!("{}", group.clicks)).style(THEME.value_dim),
                    Cell::from("mixed").style(THEME.value_dim),
                ])
                .style(base),
            );

            for (i, child) in group.children.iter().enumerate() {
                let connector = if i == group.children.len() - 1 {
                    "└─ "
                } else {
                    "├─ "
                };
                let sub_base = base;
                rows.push(
                    Row::new(vec![
                        Cell::from(format!("  {}{}", connector, child.category))
                            .style(category_style(child.category)),
                        Cell::from(""),
                        Cell::from(fmt_duration(child.total_ms)).style(THEME.value_dim),
                        Cell::from(format!("{}", child.keys)).style(THEME.value_dim),
                        Cell::from(format!("{}", child.clicks)).style(THEME.value_dim),
                        Cell::from(""),
                    ])
                    .style(sub_base),
                );
            }
        }
    }

    let table = Table::new(
        rows,
        [
            Constraint::Min(20),
            Constraint::Length(22),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(14),
        ],
    )
    .header(header)
    .block(block);

    frame.render_widget(table, area);
}

// ── Report Tab ────────────────────────────────────────────

fn render_report(app: &TuiApp, area: Rect, frame: &mut Frame) {
    let Some(ref rpt) = app.report else {
        frame.render_widget(Paragraph::new("No report data").centered(), area);
        return;
    };

    let layout = Layout::vertical([
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Min(0),
    ]);
    let [overview_area, categories_area, bottom_area] = area.layout(&layout);

    render_overview(app, rpt, overview_area, frame);
    render_categories(rpt, categories_area, frame);

    let bottom_layout = Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]);
    let [daily_area, peaks_area] = bottom_area.layout(&bottom_layout);

    render_daily(rpt, daily_area, frame);
    render_peaks(rpt, peaks_area, frame);
}

fn render_overview(app: &TuiApp, rpt: &ReportData, area: Rect, frame: &mut Frame) {
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(THEME.border)
        .title(
            Line::from(format!(" Overview ({} → {}) ", rpt.since_str, rpt.now_str))
                .style(THEME.header),
        )
        .padding(Padding::horizontal(1));

    let total = rpt.total_ms.max(1);
    let lines = vec![
        Line::from(vec![
            Span::raw("Total: "),
            Span::styled(fmt_duration(rpt.total_ms), THEME.value),
            Span::raw("   Active: "),
            Span::styled(
                format!(
                    "{} ({})",
                    fmt_duration(rpt.active_ms),
                    pct(rpt.active_ms, total)
                ),
                THEME.productive,
            ),
            Span::raw("   Passive: "),
            Span::styled(fmt_duration(rpt.passive_ms), THEME.neutral),
            Span::raw("   Idle: "),
            Span::styled(fmt_duration(rpt.idle_ms), THEME.value_dim),
        ]),
        Line::from(vec![
            Span::raw("Switches: "),
            Span::styled(format!("{}", rpt.total_events), THEME.value),
            Span::raw("   Keys: "),
            Span::styled(format!("{}", rpt.total_keys), THEME.value),
            Span::raw("   Clicks: "),
            Span::styled(format!("{}", rpt.total_clicks), THEME.value),
            Span::raw("   Scroll: "),
            Span::styled(format!("{}", rpt.total_scroll), THEME.value),
            Span::raw("   Mouse: "),
            Span::styled(fmt_distance(rpt.total_distance, app.mouse_dpi), THEME.value),
        ]),
        if rpt.jiggler_count > 0 {
            Line::from(vec![Span::styled(
                format!("⚠ {} jiggler events detected", rpt.jiggler_count),
                THEME.warning,
            )])
        } else {
            Line::from("")
        },
    ];

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_categories(rpt: &ReportData, area: Rect, frame: &mut Frame) {
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(THEME.border)
        .title(Line::from(" Productivity ").style(THEME.header));

    let total = rpt.total_ms.max(1);

    let header = Row::new(vec![
        Cell::from("Category").style(THEME.table_header),
        Cell::from("Bar").style(THEME.table_header),
        Cell::from("Time").style(THEME.table_header),
        Cell::from("Active %").style(THEME.table_header),
    ]);

    let rows: Vec<Row> = rpt
        .categories
        .iter()
        .map(|c| {
            let filled = (c.total_ms as f64 / total as f64 * 25.0).round() as usize;
            let bar_span = Span::styled("█".repeat(filled), category_style(c.category));

            Row::new(vec![
                Cell::from(format!("{}", c.category)).style(category_style(c.category).bold()),
                Cell::from(Line::from(bar_span)),
                Cell::from(fmt_duration(c.total_ms)).style(THEME.value),
                Cell::from(pct(c.active_ms, c.total_ms)).style(THEME.value_dim),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Length(27),
            Constraint::Length(10),
            Constraint::Min(10),
        ],
    )
    .header(header)
    .block(block);

    frame.render_widget(table, area);
}

fn render_daily(rpt: &ReportData, area: Rect, frame: &mut Frame) {
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(THEME.border)
        .title(Line::from(" Daily Breakdown ").style(THEME.header));

    let header = Row::new(vec![
        Cell::from("Date").style(THEME.table_header),
        Cell::from("Time").style(THEME.table_header),
        Cell::from("Active").style(THEME.table_header),
        Cell::from("Keys").style(THEME.table_header),
    ]);

    let rows: Vec<Row> = rpt
        .daily
        .iter()
        .map(|d| {
            let active_pct = if d.total_ms > 0 {
                d.active_ms as f64 / d.total_ms as f64 * 100.0
            } else {
                0.0
            };
            let active_style = if active_pct >= 60.0 {
                THEME.productive
            } else if active_pct >= 30.0 {
                THEME.neutral
            } else {
                THEME.unproductive
            };

            Row::new(vec![
                Cell::from(d.date.clone()).style(THEME.value_dim),
                Cell::from(fmt_duration(d.total_ms)).style(THEME.value),
                Cell::from(pct(d.active_ms, d.total_ms)).style(active_style),
                Cell::from(format!("{}", d.keystrokes)).style(THEME.value_dim),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Min(6),
        ],
    )
    .header(header)
    .block(block);

    frame.render_widget(table, area);
}

fn render_peaks(rpt: &ReportData, area: Rect, frame: &mut Frame) {
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(THEME.border)
        .title(Line::from(" Peak Hours ").style(THEME.header));

    let max_ms = rpt
        .peak_hours
        .first()
        .map(|h| h.total_ms)
        .unwrap_or(1)
        .max(1);

    let header = Row::new(vec![
        Cell::from("Hour").style(THEME.table_header),
        Cell::from("Bar").style(THEME.table_header),
        Cell::from("Time").style(THEME.table_header),
        Cell::from("Keys").style(THEME.table_header),
    ]);

    let rows: Vec<Row> = rpt
        .peak_hours
        .iter()
        .map(|h| {
            let bar_len = (h.total_ms as f64 / max_ms as f64 * 15.0).round() as usize;
            Row::new(vec![
                Cell::from(format!("{:02}:00", h.hour)).style(THEME.value_dim),
                Cell::from(Span::styled("█".repeat(bar_len), THEME.accent)),
                Cell::from(fmt_duration(h.total_ms)).style(THEME.value),
                Cell::from(format!("{}", h.keystrokes)).style(THEME.value_dim),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(7),
            Constraint::Length(17),
            Constraint::Length(10),
            Constraint::Min(6),
        ],
    )
    .header(header)
    .block(block);

    frame.render_widget(table, area);
}
