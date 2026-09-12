use std::time::{Duration, Instant};

pub fn init_from_env() -> Option<linkscope::ReportGuard> {
    let Ok(mode) = std::env::var("NIRI_ACTIVITY_LINKSCOPE") else {
        return None;
    };
    let mode = mode.trim().to_ascii_lowercase();
    if matches!(mode.as_str(), "" | "0" | "false" | "off" | "no") {
        return None;
    }

    match mode.as_str() {
        "trace" => linkscope::trace_enable(),
        "detail" => linkscope::trace_detail_enable(),
        "stack" => linkscope::trace_stack_enable(),
        "stack-detail" | "detail-stack" => linkscope::trace_stack_detail_enable(),
        _ => linkscope::enable(),
    }
    eprintln!("linkscope enabled for niri-activity-rs ({mode})");
    Some(linkscope::ReportGuard::new())
}

pub fn report_interval() -> Option<Duration> {
    std::env::var("NIRI_ACTIVITY_LINKSCOPE_INTERVAL_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
}

pub fn report_periodically(last_report: &mut Instant, interval: Option<Duration>) {
    let Some(interval) = interval else {
        return;
    };
    if !linkscope::is_enabled() || last_report.elapsed() < interval {
        return;
    }
    linkscope::report();
    *last_report = Instant::now();
}
