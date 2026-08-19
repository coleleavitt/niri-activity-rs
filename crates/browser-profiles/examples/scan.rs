fn main() -> Result<(), browser_profiles::Error> {
    let profiles = browser_profiles::discover()?;
    println!("discovered {} profiles\n", profiles.len());

    for p in &profiles {
        let hist = browser_profiles::read_history(p)
            .map(|v| v.len())
            .unwrap_or(0);
        let visits = browser_profiles::read_visits(p)
            .map(|v| v.len())
            .unwrap_or(0);
        let eng = browser_profiles::read_engagement(p)
            .map(|v| v.len())
            .unwrap_or(0);
        let bm = browser_profiles::read_bookmarks(p)
            .map(|v| v.len())
            .unwrap_or(0);
        let dl = browser_profiles::read_downloads(p)
            .map(|v| v.len())
            .unwrap_or(0);
        let st = browser_profiles::read_search_terms(p)
            .map(|v| v.len())
            .unwrap_or(0);
        println!(
            "{:<12} {:<32} urls={:<6} visits={:<7} engage={:<6} bm={:<4} dl={:<3} search={}",
            p.browser.to_string(),
            p.name,
            hist,
            visits,
            eng,
            bm,
            dl,
            st
        );
    }

    println!("\n--- top domains by browser-measured view time ---");
    let mut eng: Vec<_> = browser_profiles::engagement_by_domain()?
        .into_iter()
        .collect();
    eng.sort_by_key(|(_, (d, _))| std::cmp::Reverse(*d));
    for (domain, (view, keys)) in eng.iter().take(10) {
        println!(
            "{:>8.1}h  {:>8} keys  {}",
            view.as_secs_f64() / 3600.0,
            keys,
            domain
        );
    }

    println!("\n--- top cross-domain referrer edges ---");
    let mut edges: Vec<_> = browser_profiles::referrer_edges()?
        .into_iter()
        .filter(|((f, t), _)| f != t)
        .collect();
    edges.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for ((from, to), n) in edges.iter().take(10) {
        println!("{:>6} hops  {} -> {}", n, from, to);
    }
    Ok(())
}
