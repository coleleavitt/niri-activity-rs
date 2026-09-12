//! Shared terminal application identity.
//!
//! These exact Wayland app IDs gate terminal-only title and shell-state
//! interpretation across live tracking, agent detection, and DB backfills.

pub(crate) const APP_IDS: &[&str] = &[
    "Alacritty",
    "kitty",
    "foot",
    "org.wezfurlong.wezterm",
    "wezterm",
    "alacritty",
    "org.codeberg.dnkl.foot",
];

pub(crate) fn is_terminal_app(app_id: &str) -> bool {
    APP_IDS.contains(&app_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_declared_ids_and_rejects_lookalikes() {
        for app_id in APP_IDS {
            assert!(
                is_terminal_app(app_id),
                "missing declared terminal {app_id}"
            );
        }
        for app_id in ["Foot", "kitty-preview", "org.mozilla.firefox", ""] {
            assert!(!is_terminal_app(app_id), "accepted lookalike {app_id}");
        }
    }
}
