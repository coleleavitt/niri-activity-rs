//! Display functions for terminal output.

use owo_colors::OwoColorize;

use super::query::{query_metrics_range, query_report_range, query_timeline, query_today};
use super::types::{FatigueTrend, FlowQuality, GapType, ReportData};
use super::{App, MS_PER_HOUR, MS_PER_MIN, TimeRange};
use crate::config::Category;
use crate::error::Error;
use crate::fmt::{
    cat_bar, cat_bar_fractional, cat_colored, cat_label, colored_bar, fmt_distance, fmt_duration,
    fmt_duration_compact, pct, section_header, truncate,
};

/// Display today's activity breakdown by application in terminal format.
pub fn show_today(app: &App) -> Result<(), Error> {
    let data = query_today(app)?;
    println!(
        "{}\n",
        format!("Activity for today ({}):", data.date).cyan().bold()
    );
    println!(
        "{:<30} {:>12} {:>10}",
        "Application".bold(),
        "Category".bold(),
        "Time".bold()
    );
    println!("{}", "─".repeat(54).dimmed());
    for row in &data.rows {
        let h = row.total_ms / MS_PER_HOUR;
        let m = (row.total_ms % MS_PER_HOUR) / MS_PER_MIN;
        let s = (row.total_ms % MS_PER_MIN) / 1000;
        println!(
            "{:<30} {:>12} {}",
            cat_colored(row.category, &truncate(&row.app_id, 28)),
            cat_label(row.category),
            format!("{:>2}h {:>2}m {:>2}s", h, m, s).bold()
        );
    }
    Ok(())
}

/// Display productivity metrics for a date range in terminal format.
pub fn show_metrics_range(app: &App, range: TimeRange) -> Result<(), Error> {
    let m = query_metrics_range(app, range)?;
    println!(
        "{}\n",
        format!(
            "═══ Productivity Metrics ({} day{}) ═══",
            m.days,
            if m.days == 1 { "" } else { "s" }
        )
        .cyan()
        .bold()
    );
    println!(
        "Total Time:              {}",
        fmt_duration(m.total_ms).bold()
    );
    println!(
        "Productive Time:         {} {}",
        fmt_duration(m.productive_ms).green().bold(),
        pct(m.productive_ms, m.total_ms).dimmed()
    );
    println!(
        "Unproductive Time:       {} {}",
        fmt_duration(m.unproductive_ms).red().bold(),
        pct(m.unproductive_ms, m.total_ms).dimmed()
    );
    println!(
        "Undefined Time:          {} {}",
        fmt_duration(m.neutral_ms).yellow().bold(),
        pct(m.neutral_ms, m.total_ms).dimmed()
    );
    println!();
    println!(
        "Productive Active:       {} {}",
        fmt_duration(m.productive_active_ms).green(),
        pct(m.productive_active_ms, m.productive_ms).dimmed()
    );
    println!(
        "Productive Passive:      {} {}",
        fmt_duration(m.productive_passive_ms).yellow(),
        pct(m.productive_passive_ms, m.productive_ms).dimmed()
    );
    println!(
        "Productive Idle:         {} {}",
        fmt_duration(m.productive_idle_ms).dimmed(),
        pct(
            m.productive_idle_ms,
            m.productive_ms.saturating_add(m.productive_idle_ms)
        )
        .dimmed()
    );
    Ok(())
}

/// Display hourly activity timeline for the past N days in terminal format.
pub fn show_timeline(app: &App, days_back: u32, bucket_min: u32) -> Result<(), Error> {
    let data = query_timeline(app, days_back, bucket_min)?;
    if data.buckets.is_empty() {
        println!("No activity recorded for {}", data.date);
        return Ok(());
    }
    println!(
        "{}\n",
        format!(
            "═══ Timeline for {} ({}min buckets) ═══",
            data.date, data.bucket_min
        )
        .cyan()
        .bold()
    );
    let bar_width: usize = 20;
    for b in &data.buckets {
        let total = b
            .productive_ms
            .saturating_add(b.neutral_ms)
            .saturating_add(b.unproductive_ms);
        if total == 0 {
            continue;
        }
        let total_with_idle = total.saturating_add(b.idle_ms);
        let idle_pct = if total_with_idle > 0 {
            b.idle_ms as f64 / total_with_idle as f64
        } else {
            0.0
        };
        let (prod_frac, neutral_frac, unprod_frac) = if total > 0 {
            (
                b.productive_ms as f64 / total as f64,
                b.neutral_ms as f64 / total as f64,
                b.unproductive_ms as f64 / total as f64,
            )
        } else {
            (0.0, 0.0, 0.0)
        };
        let bar = colored_bar(prod_frac, neutral_frac, unprod_frac, bar_width);
        let idle_marker = if idle_pct > 0.8 {
            format!(" {}", "[AFK]".red().bold())
        } else if idle_pct > 0.5 {
            format!(" {}", "[mostly idle]".yellow())
        } else {
            String::new()
        };
        println!(
            "{} {} {:>6} {:>4} keys  {:<20}{}",
            format!("{:02}:{:02}", b.hour, b.minute).dimmed(),
            bar,
            fmt_duration_compact(total),
            b.keystrokes,
            truncate(&b.dominant_app, 20),
            idle_marker
        );
    }
    println!(
        "\n  {} productive  {} neutral  {} unproductive",
        "█".green(),
        "█".yellow(),
        "█".red()
    );
    Ok(())
}

/// Generate and display a comprehensive report for a date range in terminal
/// format.
pub fn generate_report_range(app: &App, range: TimeRange) -> Result<(), Error> {
    let data = query_report_range(app, range)?;
    println!(
        "{}",
        "╔══════════════════════════════════════════════════════╗"
            .cyan()
            .bold()
    );
    println!(
        "{}{}{}",
        "║".cyan().bold(),
        "           ACTIVITY REPORT                           "
            .white()
            .bold(),
        "║".cyan().bold()
    );
    println!(
        "{}  Period: {} → {}       {}",
        "║".cyan().bold(),
        data.since_str,
        data.now_str,
        "║".cyan().bold()
    );
    println!(
        "{}\n",
        "╚══════════════════════════════════════════════════════╝"
            .cyan()
            .bold()
    );
    if data.total_events == 0 {
        println!("No activity recorded for this period.");
        return Ok(());
    }
    println!(
        "{}",
        section_header("── Overview ──────────────────────────────────────────")
    );
    println!(
        "  Total Time:         {}",
        fmt_duration(data.total_ms).bold()
    );
    println!(
        "  Active:             {} {}",
        fmt_duration(data.active_ms).green().bold(),
        pct(data.active_ms, data.total_ms).dimmed()
    );
    println!(
        "  Passive:            {} {}",
        fmt_duration(data.passive_ms).yellow(),
        pct(data.passive_ms, data.total_ms).dimmed()
    );
    println!(
        "  Idle/AFK:           {} {}",
        fmt_duration(data.idle_ms).dimmed(),
        pct(data.idle_ms, data.total_ms).dimmed()
    );
    println!("  Focus Switches:     {}", data.total_events);
    println!("  Keystrokes:         {}", data.total_keys);
    println!("  Mouse Clicks:       {}", data.total_clicks);
    println!("  Scroll Events:      {}", data.total_scroll);
    println!(
        "  Mouse Travel:       {}",
        fmt_distance(data.total_distance, app.config.mouse_dpi)
    );
    if data.jiggler_count > 0 {
        println!(
            "  Jiggler Events:     {}",
            format!("{} (artificial input detected)", data.jiggler_count)
                .red()
                .bold()
        );
    }
    if app.config.goals.enabled {
        let productive_ms = data
            .categories
            .iter()
            .find(|c| c.category == Category::Productive)
            .map_or(0, |c| c.total_ms);
        println!(
            "\n{}",
            section_header("── Goals ─────────────────────────────────────────────")
        );
        if let Some(daily_goal) = app.config.goals.daily_ms() {
            #[allow(clippy::cast_possible_wrap)]
            let days = (data.daily.len() as i64).max(1);
            let daily_avg = productive_ms.checked_div(days).unwrap_or(0);
            let daily_pct = if daily_goal > 0 {
                (daily_avg as f64 / daily_goal as f64 * 100.0).min(999.0)
            } else {
                0.0
            };
            let bar_len = ((daily_pct / 5.0).round() as usize).min(20);
            let status = if daily_pct >= 100.0 {
                format!("{}%", daily_pct.round()).green().bold().to_string()
            } else if daily_pct >= 75.0 {
                format!("{}%", daily_pct.round()).yellow().to_string()
            } else {
                format!("{}%", daily_pct.round()).red().to_string()
            };
            println!(
                "  Daily Goal:         {}{} {} (avg {} / {})",
                "█".repeat(bar_len).green(),
                "░".repeat(20 - bar_len).dimmed(),
                status,
                fmt_duration(daily_avg).bold(),
                app.config.goals.daily
            );
        }
        if let Some(weekly_goal) = app.config.goals.weekly_ms() {
            let weekly_pct = if weekly_goal > 0 {
                (productive_ms as f64 / weekly_goal as f64 * 100.0).min(999.0)
            } else {
                0.0
            };
            let bar_len = ((weekly_pct / 5.0).round() as usize).min(20);
            let status = if weekly_pct >= 100.0 {
                format!("{}%", weekly_pct.round())
                    .green()
                    .bold()
                    .to_string()
            } else if weekly_pct >= 75.0 {
                format!("{}%", weekly_pct.round()).yellow().to_string()
            } else {
                format!("{}%", weekly_pct.round()).red().to_string()
            };
            println!(
                "  Weekly Goal:        {}{} {} ({} / {})",
                "█".repeat(bar_len).green(),
                "░".repeat(20 - bar_len).dimmed(),
                status,
                fmt_duration(productive_ms).bold(),
                app.config.goals.weekly
            );
        }
    }
    println!(
        "\n{}",
        section_header("── Productivity ──────────────────────────────────────")
    );
    for cat in &data.categories {
        let icon = match cat.category {
            Category::Productive => "●".green().to_string(),
            Category::Unproductive => "○".red().to_string(),
            Category::Neutral => "◌".yellow().to_string(),
        };
        let filled = if data.total_ms > 0 {
            (cat.total_ms as f64 / data.total_ms as f64 * 30.0).round() as usize
        } else {
            0
        };
        let agent_note = if cat.agent_ms > 0 {
            format!(
                " (agent: {} {})",
                fmt_duration(cat.agent_ms),
                pct(cat.agent_ms, cat.total_ms)
            )
        } else {
            String::new()
        };
        println!(
            "  {} {:<14} {} {} {}{}",
            icon,
            cat_label(cat.category),
            cat_bar(cat.category, filled),
            fmt_duration(cat.total_ms).bold(),
            format!("(active: {})", pct(cat.active_ms, cat.total_ms)).dimmed(),
            agent_note.cyan()
        );
    }

    if data.agent_ms > 0 || data.unmeasured_agent_ms > 0 {
        print_agent_summary(&data);
    }
    println!(
        "\n{}",
        section_header("── Top Applications ──────────────────────────────────")
    );
    const BAR_WIDTH: usize = 20;
    let max_app_ms = data.top_apps.first().map_or(1, |g| g.total_ms).max(1);
    for group in &data.top_apps {
        if group.children.len() == 1 {
            let row = &group.children[0];
            let frac_blocks =
                (row.total_ms as f64 / max_app_ms as f64 * BAR_WIDTH as f64).max(0.125);
            let bar = cat_bar_fractional(row.category, frac_blocks, BAR_WIDTH);
            let active_min = row.active_ms as f64 / MS_PER_MIN as f64;
            let keys_per_min = if active_min > 0.5 {
                format!("{:.0}/m", row.keys as f64 / active_min)
            } else {
                "-".to_string()
            };
            println!(
                "  {} {} {:>8}  {:>5} keys ({:>5}) {:>3} clicks  ({})",
                cat_colored(row.category, &format!("{:<22}", truncate(&row.app_id, 22))),
                bar,
                fmt_duration(row.total_ms).bold(),
                row.keys,
                keys_per_min.dimmed(),
                row.clicks,
                cat_label(row.category)
            );
        } else {
            let filled =
                (group.total_ms as f64 / max_app_ms as f64 * BAR_WIDTH as f64).round() as usize;
            let filled = filled.max(1);
            let active_min = group.active_ms as f64 / MS_PER_MIN as f64;
            let keys_per_min = if active_min > 0.5 {
                format!("{:.0}/m", group.keys as f64 / active_min)
            } else {
                "-".to_string()
            };
            let (mut prod_ms, mut neutral_ms, mut unprod_ms) = (0i64, 0i64, 0i64);
            for child in &group.children {
                match child.category {
                    Category::Productive => prod_ms += child.total_ms,
                    Category::Neutral => neutral_ms += child.total_ms,
                    Category::Unproductive => unprod_ms += child.total_ms,
                }
            }
            let (prod_frac, neutral_frac, unprod_frac) = if group.total_ms > 0 {
                (
                    prod_ms as f64 / group.total_ms as f64,
                    neutral_ms as f64 / group.total_ms as f64,
                    unprod_ms as f64 / group.total_ms as f64,
                )
            } else {
                (0.0, 0.0, 0.0)
            };
            let app_name = format!("{:<22}", truncate(&group.app_id, 22));
            let bar = format!(
                "{}{}",
                colored_bar(prod_frac, neutral_frac, unprod_frac, filled),
                " ".repeat(BAR_WIDTH - filled)
            );
            println!(
                "  {} {} {:>8}  {:>5} keys ({:>5}) {:>3} clicks",
                app_name.bold(),
                bar,
                fmt_duration(group.total_ms).bold(),
                group.keys,
                keys_per_min.dimmed(),
                group.clicks
            );
            for (i, child) in group.children.iter().enumerate() {
                let conn = if i == group.children.len() - 1 {
                    "└─"
                } else {
                    "├─"
                };
                let active_min = child.active_ms as f64 / MS_PER_MIN as f64;
                let kpm = if active_min > 0.5 {
                    format!("{:.0}/m", child.keys as f64 / active_min)
                } else {
                    "-".to_string()
                };
                let label = match child.category {
                    Category::Productive => "productive",
                    Category::Unproductive => "unproductive",
                    Category::Neutral => "neutral",
                };
                let dur = fmt_duration(child.total_ms);
                let gap = 54_usize.saturating_sub(7 + label.len() + dur.len());
                println!(
                    "    {} {}{}{}  {:>5} keys ({:>5}) {:>3} clicks",
                    conn.dimmed(),
                    cat_colored(child.category, label),
                    " ".repeat(gap),
                    dur.bold(),
                    child.keys,
                    kpm.dimmed(),
                    child.clicks
                );
            }
        }
    }
    if !data.projects.is_empty() {
        println!(
            "\n{}",
            section_header("── Top Projects ──────────────────────────────────────")
        );
        let max_proj_ms = data.projects.first().map_or(1, |p| p.total_ms).max(1);
        for proj in &data.projects {
            let filled = (proj.total_ms as f64 / max_proj_ms as f64 * BAR_WIDTH as f64)
                .round()
                .clamp(1.0, BAR_WIDTH as f64) as usize;
            let category_count = [proj.productive_ms, proj.neutral_ms, proj.unproductive_ms]
                .into_iter()
                .filter(|total| *total > 0)
                .count();
            let (name, bar, label) = if category_count == 1 {
                let category = if proj.productive_ms > 0 {
                    Category::Productive
                } else if proj.unproductive_ms > 0 {
                    Category::Unproductive
                } else {
                    Category::Neutral
                };
                (
                    cat_colored(category, &format!("{:<22}", truncate(&proj.project, 22))),
                    cat_bar_fractional(category, filled as f64, BAR_WIDTH),
                    cat_label(category),
                )
            } else {
                let total = proj.total_ms.max(1) as f64;
                let segmented = colored_bar(
                    proj.productive_ms as f64 / total,
                    proj.neutral_ms as f64 / total,
                    proj.unproductive_ms as f64 / total,
                    filled,
                );
                (
                    format!("{:<22}", truncate(&proj.project, 22))
                        .bold()
                        .to_string(),
                    format!("{}{}", segmented, " ".repeat(BAR_WIDTH - filled)),
                    "mixed".dimmed().to_string(),
                )
            };
            let active_min = proj.active_ms as f64 / MS_PER_MIN as f64;
            let keys_per_min = if active_min > 0.5 {
                format!("{:.0}/m", proj.keys as f64 / active_min)
            } else {
                "-".to_string()
            };
            println!(
                "  {} {} {:>8}  {:>5} keys ({:>5}) {:>3} clicks  ({})",
                name,
                bar,
                fmt_duration(proj.total_ms).bold(),
                proj.keys,
                keys_per_min.dimmed(),
                proj.clicks,
                label
            );
        }
    }
    println!(
        "\n{}",
        section_header("── Daily Breakdown ───────────────────────────────────")
    );
    for d in &data.daily {
        let active_pct_val = if d.total_ms > 0 {
            d.active_ms as f64 / d.total_ms as f64 * 100.0
        } else {
            0.0
        };
        let active_str = pct(d.active_ms, d.total_ms);
        let active_colored = if active_pct_val >= 60.0 {
            active_str.green().to_string()
        } else if active_pct_val >= 30.0 {
            active_str.yellow().to_string()
        } else {
            active_str.red().to_string()
        };
        println!(
            "  {}  {:>8}  active: {}  {:>5} keys  {} switches",
            d.date.dimmed(),
            fmt_duration(d.total_ms).bold(),
            active_colored,
            d.keystrokes,
            d.switches
        );
    }
    if let Some(sched) = &data.schedule {
        println!(
            "\n{}",
            section_header("── Schedule ──────────────────────────────────────────")
        );
        println!(
            "  {:<27} {:>8}  active: {:>6}  {:>5} keys",
            sched.work_label.green(),
            fmt_duration(sched.work_total_ms).bold(),
            pct(sched.work_active_ms, sched.work_total_ms).green(),
            sched.work_keys
        );
        println!(
            "  {:<27} {:>8}  active: {:>6}  {:>5} keys",
            "After Hours:".yellow(),
            fmt_duration(sched.after_total_ms).bold(),
            pct(sched.after_active_ms, sched.after_total_ms).yellow(),
            sched.after_keys
        );
    }
    if let Some(away) = &data.away
        && !away.summaries.is_empty()
    {
        println!(
            "\n{}",
            section_header("── Away Time ─────────────────────────────────────────")
        );
        for summary in &away.summaries {
            let (icon, label, vis) = match summary.gap_type {
                GapType::Sleep => ("◑".blue().to_string(), "Sleep:".blue().to_string(), 8),
                GapType::LongBreak => (
                    "◐".yellow().to_string(),
                    "Long Break:".yellow().to_string(),
                    13,
                ),
                GapType::ShortBreak => (
                    "◌".dimmed().to_string(),
                    "Short Break:".dimmed().to_string(),
                    14,
                ),
            };
            println!(
                "  {} {}{}{:>8} total  ({} × {} avg)",
                icon,
                label,
                " ".repeat(20 - vis),
                fmt_duration(summary.total_ms).bold(),
                summary.count,
                fmt_duration(summary.avg_ms).dimmed()
            );
        }
        println!(
            "  {}{}{:>8}",
            "Total Away:".dimmed(),
            " ".repeat(8),
            fmt_duration(away.total_away_ms).dimmed()
        );
    }
    println!(
        "\n{}",
        section_header("── Peak Hours ────────────────────────────────────────")
    );
    let max_hour_ms = data.peak_hours.first().map_or(1, |h| h.total_ms);
    for h in &data.peak_hours {
        let bar_len = (h.total_ms as f64 / max_hour_ms as f64 * 20.0).round() as usize;
        println!(
            "  {}  {} {:>8}  {:>5} keys",
            format!("{:02}:00", h.hour).dimmed(),
            "█".repeat(bar_len).cyan(),
            fmt_duration(h.total_ms).bold(),
            h.keystrokes
        );
    }
    if let Some(streaks) = &data.streaks
        && streaks.total_productive_streaks > 0
    {
        println!(
            "\n{}",
            section_header("── Focus Streaks ─────────────────────────────────────")
        );
        println!(
            "  Longest Streak:     {} in {}",
            fmt_duration(streaks.longest_productive_ms).green().bold(),
            streaks.longest_productive_app.bold()
        );
        println!(
            "  Average Streak:     {}",
            fmt_duration(streaks.avg_productive_streak_ms)
        );
        println!(
            "  Total Streaks:      {} (5+ min productive sessions)",
            streaks.total_productive_streaks
        );
        if !streaks.top_streaks.is_empty() {
            println!();
            for (i, streak) in streaks.top_streaks.iter().enumerate() {
                println!(
                    "  {:>3} {:>8}  {}  {} keys  {}",
                    format!("{}.", i + 1).dimmed(),
                    fmt_duration(streak.duration_ms).green(),
                    streak.start_time.dimmed(),
                    streak.keystrokes,
                    truncate(&streak.app_id, 20)
                );
            }
        }
    }
    if let Some(input) = &data.input_metrics
        && (input.backspace_count > 0
            || input.modifier_count > 0
            || input.left_clicks > 0
            || input.right_clicks > 0
            || input.middle_clicks > 0
            || input.scroll_up > 0
            || input.scroll_down > 0
            || input.scroll_horizontal > 0)
    {
        println!(
            "\n{}",
            section_header("── Input Metrics ─────────────────────────────────────")
        );
        println!(
            "  Backspace/Delete:   {} ({:.2}% of keystrokes)",
            input.backspace_count.to_string().bold(),
            input.backspace_rate
        );
        println!(
            "  Modifier Keys:      {} ({:.2}% of keystrokes)",
            input.modifier_count.to_string().bold(),
            input.modifier_rate
        );
        println!();
        println!(
            "  Mouse Clicks:       {} left, {} right, {} middle",
            input.left_clicks.to_string().bold(),
            input.right_clicks,
            input.middle_clicks
        );
        println!(
            "  Scroll Events:      {} up, {} down, {} horizontal",
            input.scroll_up.to_string().bold(),
            input.scroll_down,
            input.scroll_horizontal
        );
    }
    if let Some(flow) = &data.flow
        && flow.flow_sessions > 0
    {
        println!(
            "\n{}",
            section_header("── Flow State ────────────────────────────────────────")
        );
        let quality_str = match flow.dominant_quality {
            FlowQuality::Deep => flow.dominant_quality.label().green().bold().to_string(),
            FlowQuality::Moderate => flow.dominant_quality.label().green().to_string(),
            FlowQuality::Light => flow.dominant_quality.label().yellow().to_string(),
            FlowQuality::Shallow => flow.dominant_quality.label().dimmed().to_string(),
        };
        println!(
            "  Flow Quality:       {} (score: {})",
            quality_str, flow.overall_flow_score
        );
        println!(
            "  Total Flow Time:    {}",
            fmt_duration(flow.total_flow_ms).green().bold()
        );
        println!(
            "  Flow Sessions:      {} (avg {})",
            flow.flow_sessions,
            fmt_duration(flow.avg_flow_duration_ms)
        );
        println!(
            "  Peak Intensity:     {:.0} keys/min",
            flow.peak_keys_per_min
        );
        if flow.deep_flow_ms > 0 || flow.moderate_flow_ms > 0 || flow.light_flow_ms > 0 {
            println!();
            if flow.deep_flow_ms > 0 {
                println!(
                    "  Deep Flow:          {}",
                    fmt_duration(flow.deep_flow_ms).green().bold()
                );
            }
            if flow.moderate_flow_ms > 0 {
                println!(
                    "  Moderate Flow:      {}",
                    fmt_duration(flow.moderate_flow_ms).green()
                );
            }
            if flow.light_flow_ms > 0 {
                println!(
                    "  Light Flow:         {}",
                    fmt_duration(flow.light_flow_ms).yellow()
                );
            }
        }
        if !flow.top_sessions.is_empty() {
            println!();
            for (i, session) in flow.top_sessions.iter().enumerate() {
                let score_str = format!("[{}]", session.flow_score_0_to_100);
                let score_colored = match FlowQuality::from_score(session.flow_score_0_to_100) {
                    FlowQuality::Deep => score_str.green().bold().to_string(),
                    FlowQuality::Moderate => score_str.green().to_string(),
                    FlowQuality::Light => score_str.yellow().to_string(),
                    FlowQuality::Shallow => score_str.dimmed().to_string(),
                };
                println!(
                    "  {:>3} {:>8}  {} {}  {:.0} kpm  {}",
                    format!("{}.", i + 1).dimmed(),
                    fmt_duration(session.duration_ms).green(),
                    session.start_time.dimmed(),
                    score_colored,
                    session.keys_per_min,
                    truncate(&session.app_id, 18)
                );
            }
        }
    }
    if let Some(fatigue) = &data.fatigue
        && fatigue.trend != FatigueTrend::Insufficient
    {
        println!(
            "\n{}",
            section_header("── Fatigue Indicators ────────────────────────────────")
        );
        let trend_str = match fatigue.trend {
            FatigueTrend::Increasing => "↑ Increasing".red().to_string(),
            FatigueTrend::Stable => "→ Stable".green().to_string(),
            FatigueTrend::Decreasing => "↓ Decreasing".green().bold().to_string(),
            FatigueTrend::Insufficient => "Insufficient data".dimmed().to_string(),
        };
        println!("  Error Rate Trend:   {}", trend_str);
        println!(
            "  Early Session:      {:.2}% backspace rate",
            fatigue.early_error_rate
        );
        println!(
            "  Late Session:       {:.2}% backspace rate",
            fatigue.late_error_rate
        );
        if let Some(rec) = &fatigue.recommendation {
            println!();
            println!("  💡 {}", rec.cyan());
        }
    }
    println!();
    Ok(())
}

/// Display a comparison of productivity metrics across multiple time periods.
/// Report how much of the period had a coding agent working in parallel.
///
/// Deliberately reported beside the categories rather than folded into them:
/// an agent running does not make a film productive, but it does distinguish
/// waiting on a build from idle scrolling.
fn print_agent_summary(data: &ReportData) {
    println!(
        "\n{}",
        section_header("── Agent Activity ────────────────────────────────────")
    );

    let measured_ms = data.total_ms.saturating_sub(data.unmeasured_agent_ms);
    println!(
        "  Agent working:      {} {}",
        fmt_duration(data.agent_ms).cyan().bold(),
        format!("of {} measured", fmt_duration(measured_ms)).dimmed()
    );

    if data.agent_credited_ms > 0 {
        println!(
            "  Counted productive: {} {}",
            fmt_duration(data.agent_credited_ms).green().bold(),
            "agent time credited from other categories".dimmed()
        );
    }

    if data.unmeasured_agent_ms > 0 {
        println!(
            "  {}",
            format!(
                "{} recorded before agent tracking existed, excluded above",
                fmt_duration(data.unmeasured_agent_ms)
            )
            .dimmed()
        );
    }
}

pub fn show_comparison(app: &App, range: TimeRange) -> Result<(), Error> {
    let current_bounds = range.resolve(&app.config)?;
    let current_days = (current_bounds.end_date - current_bounds.start_date)
        .num_days()
        .saturating_add(1);
    let prev_end = current_bounds.start_date - chrono::Duration::days(1);
    let prev_start = prev_end - chrono::Duration::days(current_days - 1);
    let prev_range = TimeRange::DateRange(prev_start, prev_end);
    let current = query_metrics_range(app, range)?;
    let previous = query_metrics_range(app, prev_range)?;
    println!(
        "{}",
        "╔══════════════════════════════════════════════════════╗"
            .cyan()
            .bold()
    );
    println!(
        "{}{}{}",
        "║".cyan().bold(),
        "          PERIOD COMPARISON                           "
            .white()
            .bold(),
        "║".cyan().bold()
    );
    println!(
        "{}\n",
        "╚══════════════════════════════════════════════════════╝"
            .cyan()
            .bold()
    );
    println!(
        "  Current:  {} → {}",
        current_bounds.since_str.green(),
        current_bounds.now_str.green()
    );
    println!(
        "  Previous: {} → {}\n",
        format!("{} 00:00", prev_start).dimmed(),
        format!("{} 23:59", prev_end).dimmed()
    );
    fn delta_str(cur: i64, prev: i64) -> String {
        if prev == 0 {
            return if cur > 0 {
                "+∞".green().bold().to_string()
            } else {
                "—".dimmed().to_string()
            };
        }
        let pct = ((cur as f64 - prev as f64) / prev as f64) * 100.0;
        if pct > 0.0 {
            format!("+{:.1}%", pct).green().to_string()
        } else if pct < 0.0 {
            format!("{:.1}%", pct).red().to_string()
        } else {
            "0%".dimmed().to_string()
        }
    }
    fn delta_str_inv(cur: i64, prev: i64) -> String {
        if prev == 0 {
            return if cur > 0 {
                "+∞".red().bold().to_string()
            } else {
                "—".dimmed().to_string()
            };
        }
        let pct = ((cur as f64 - prev as f64) / prev as f64) * 100.0;
        if pct > 0.0 {
            format!("+{:.1}%", pct).red().to_string()
        } else if pct < 0.0 {
            format!("{:.1}%", pct).green().to_string()
        } else {
            "0%".dimmed().to_string()
        }
    }
    println!(
        "{:24} {:>12} {:>12} {:>10}",
        "Metric".bold(),
        "Current".bold(),
        "Previous".bold(),
        "Change".bold()
    );
    println!("{}", "─".repeat(60).dimmed());
    println!(
        "{:24} {:>12} {:>12} {:>10}",
        "Total Time",
        fmt_duration(current.total_ms),
        fmt_duration(previous.total_ms),
        delta_str(current.total_ms, previous.total_ms)
    );
    println!(
        "{:24} {:>12} {:>12} {:>10}",
        "Productive".green(),
        fmt_duration(current.productive_ms).green(),
        fmt_duration(previous.productive_ms),
        delta_str(current.productive_ms, previous.productive_ms)
    );
    println!(
        "{:24} {:>12} {:>12} {:>10}",
        "Unproductive".red(),
        fmt_duration(current.unproductive_ms).red(),
        fmt_duration(previous.unproductive_ms),
        delta_str_inv(current.unproductive_ms, previous.unproductive_ms)
    );
    println!(
        "{:24} {:>12} {:>12} {:>10}",
        "Neutral".yellow(),
        fmt_duration(current.neutral_ms).yellow(),
        fmt_duration(previous.neutral_ms),
        delta_str(current.neutral_ms, previous.neutral_ms)
    );
    let cur_ratio = if current.total_ms > 0 {
        (current.productive_ms as f64 / current.total_ms as f64 * 100.0).round() as i64
    } else {
        0
    };
    let prev_ratio = if previous.total_ms > 0 {
        (previous.productive_ms as f64 / previous.total_ms as f64 * 100.0).round() as i64
    } else {
        0
    };
    println!("{}", "─".repeat(60).dimmed());
    println!(
        "{:24} {:>11}% {:>11}% {:>10}",
        "Productivity Ratio".bold(),
        cur_ratio,
        prev_ratio,
        delta_str(cur_ratio, prev_ratio)
    );
    let cur_daily = current.productive_ms / current.days.max(1) as i64;
    let prev_daily = previous.productive_ms / previous.days.max(1) as i64;
    println!(
        "{:24} {:>12} {:>12} {:>10}",
        "Daily Avg (productive)",
        fmt_duration(cur_daily),
        fmt_duration(prev_daily),
        delta_str(cur_daily, prev_daily)
    );
    println!();
    Ok(())
}
