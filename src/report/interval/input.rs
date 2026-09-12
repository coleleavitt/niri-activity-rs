#[derive(Debug, Clone, Copy, Default)]
pub(in crate::report) struct GranularInput {
    pub(in crate::report) backspace_count: i64,
    pub(in crate::report) modifier_count: i64,
    pub(in crate::report) left_clicks: i64,
    pub(in crate::report) right_clicks: i64,
    pub(in crate::report) middle_clicks: i64,
    pub(in crate::report) scroll_up: i64,
    pub(in crate::report) scroll_down: i64,
    pub(in crate::report) scroll_horizontal: i64,
}

impl GranularInput {
    pub(super) fn slice(self, start: i64, end: i64, total: i64) -> Self {
        Self {
            backspace_count: proportional_between(self.backspace_count, start, end, total),
            modifier_count: proportional_between(self.modifier_count, start, end, total),
            left_clicks: proportional_between(self.left_clicks, start, end, total),
            right_clicks: proportional_between(self.right_clicks, start, end, total),
            middle_clicks: proportional_between(self.middle_clicks, start, end, total),
            scroll_up: proportional_between(self.scroll_up, start, end, total),
            scroll_down: proportional_between(self.scroll_down, start, end, total),
            scroll_horizontal: proportional_between(self.scroll_horizontal, start, end, total),
        }
    }
}

pub(super) fn proportional_between(value: i64, start: i64, end: i64, total: i64) -> i64 {
    if value <= 0 || total <= 0 || end <= start {
        return 0;
    }
    let value = i128::from(value);
    let total = i128::from(total);
    let at = |offset: i64| value * i128::from(offset.max(0)) / total;
    i64::try_from(at(end).saturating_sub(at(start))).unwrap_or(i64::MAX)
}
