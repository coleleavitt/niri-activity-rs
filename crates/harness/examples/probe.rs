use std::time::{Duration, SystemTime};

fn ago(t: Option<SystemTime>) -> String {
    match t.and_then(|t| t.elapsed().ok()) {
        Some(d) if d.as_secs() < 90 => format!("{}s", d.as_secs()),
        Some(d) if d.as_secs() < 5400 => format!("{}m", d.as_secs() / 60),
        Some(d) if d.as_secs() < 172_800 => format!("{}h", d.as_secs() / 3600),
        Some(d) => format!("{}d", d.as_secs() / 86400),
        None => "never".into(),
    }
}

fn main() {
    let win = Duration::from_secs(30);
    let running = harness::running();
    println!(
        "{:<13} {:>7} {:>8} {:>7} {:>12} {:>11}",
        "Harness", "active", "running", "seen", "gen tokens/5m", "total/5m"
    );
    println!("{}", "-".repeat(64));
    for h in harness::Harness::ALL {
        let u = harness::recent_usage(*h, 300_000);
        println!(
            "{:<13} {:>7} {:>8} {:>7} {:>12} {:>11}",
            h.to_string(),
            if harness::is_active(*h, win) {
                "YES"
            } else {
                "-"
            },
            if running.contains(h) { "yes" } else { "-" },
            ago(harness::last_activity(*h)),
            u.map_or("—".into(), |u| u.generated().to_string()),
            u.map_or("—".into(), |u| u.total().to_string())
        );
    }
    let all = harness::recent_usage_all(300_000);
    println!("\nany_active(30s)     = {}", harness::any_active(win));
    println!("any_generating(30s) = {}", harness::any_generating(30_000));
    println!(
        "last 5m: generated={} in={} out={} reasoning={} cache_r={}",
        all.generated(),
        all.input,
        all.output,
        all.reasoning,
        all.cache_read
    );
}
