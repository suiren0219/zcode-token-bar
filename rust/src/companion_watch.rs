//! 轮状态机。不接触统计数据库，只接收主程序传入的最小字段。
use super::{AutoKind, AutoSignal, CompanionSettings, TurnState};
use std::collections::VecDeque;

#[derive(Default)]
pub(super) struct TurnWatch {
    session_id: String,
    turn_id: String,
    started_at: i64,
    status: String,
    errors: i64,
    handled: bool,
    baseline_at: f64,
    observed_at: f64,
    last_now: Option<f64>,
    last_emitted: Option<f64>,
    emitted: VecDeque<f64>,
}

impl TurnWatch {
    pub fn reset(&mut self) { *self = Self::default(); }

    pub fn rebaseline(&mut self) {
        self.session_id.clear();
        self.turn_id.clear();
        self.started_at = 0;
        self.status.clear();
        self.errors = 0;
        self.handled = false;
        self.baseline_at = 0.0;
        self.observed_at = 0.0;
    }

    pub fn selection_changed(&self, state: &TurnState) -> bool {
        !self.session_id.is_empty() && (!Self::valid(state) || self.session_id != state.session_id)
    }

    fn valid(s: &TurnState) -> bool {
        s.available && !s.is_sub && !s.session_id.is_empty() && !s.turn_id.is_empty()
            && !s.session_id.starts_with("sess_subagent")
            && s.session_id.len() <= 1024 && s.turn_id.len() <= 1024
            && matches!(s.status.as_str(), "running" | "completed" | "error" | "cancelled")
            && s.started_at > 0 && s.completed_at >= 0 && s.duration_ms >= 0 && s.tool_errors >= 0
            && s.duration_ms <= 1_000_000_000_000 && s.tool_errors <= 1_000_000_000_000
            && if s.status == "running" { s.completed_at == 0 } else { s.completed_at >= s.started_at }
    }

    fn remember(&mut self, s: &TurnState, now: f64, baseline: bool, settings: &CompanionSettings) {
        self.turn_id = s.turn_id.clone();
        self.started_at = s.started_at;
        self.status = s.status.clone();
        self.errors = s.tool_errors;
        self.observed_at = now;
        self.handled = baseline && (s.status != "running" || s.tool_errors >= settings.error_threshold as i64);
    }

    pub fn observe(&mut self, s: &TurnState, now: f64, settings: &CompanionSettings) -> Option<AutoSignal> {
        if !now.is_finite() || now < 0.0 {
            self.rebaseline();
            return None;
        }
        if self.last_now.is_some_and(|last| now < last) { self.rebaseline(); }
        self.last_now = Some(now);
        if !Self::valid(s) {
            self.rebaseline();
            return None;
        }
        if self.session_id != s.session_id {
            self.rebaseline();
            self.session_id = s.session_id.clone();
            self.baseline_at = now;
            self.remember(s, now, true, settings);
            return None;
        }
        let mut previous_errors = self.errors;
        let mut previous_status = self.status.clone();
        let gap = now - self.observed_at;
        if self.turn_id != s.turn_id {
            if s.started_at <= self.started_at { return None; }
            self.remember(s, now, false, settings);
            previous_errors = 0;
            previous_status.clear();
        } else {
            if s.started_at != self.started_at || s.tool_errors < self.errors
                || (previous_status != "running" && s.status != previous_status) {
                self.handled = true;
            }
            self.errors = s.tool_errors;
            self.status = s.status.clone();
            self.observed_at = now;
        }
        if self.handled { return None; }
        if s.cancelled_by_user || s.status == "cancelled" {
            self.handled = true;
            return None;
        }
        let failed = s.tool_errors >= settings.error_threshold as i64
            && (previous_errors < settings.error_threshold as i64
                || (!settings.trigger_running_errors && s.status != "running" && previous_status == "running"));
        let finished = s.status == "completed" && previous_status != "completed";
        let kind = if failed && (s.status != "running" || settings.trigger_running_errors) {
            Some(AutoKind::ToolFailures)
        } else if finished && s.duration_ms >= settings.long_turn_seconds.saturating_mul(1000) as i64 {
            Some(AutoKind::LongTurn)
        } else if finished && settings.end_of_turn {
            Some(AutoKind::TurnFinished)
        } else { None };
        if s.status != "running" || kind.is_some() { self.handled = true; }
        let kind = kind?;
        let occurred_at = if s.status == "running" { now } else { s.completed_at as f64 / 1000.0 };
        if s.started_at as f64 / 1000.0 > now || occurred_at < self.baseline_at
            || now < occurred_at || now - occurred_at > settings.event_ttl_seconds as f64
            || (s.status == "running" && gap > settings.event_ttl_seconds as f64) {
            return None;
        }
        while self.emitted.front().is_some_and(|at| now - at >= 3600.0) { self.emitted.pop_front(); }
        if self.emitted.len() >= settings.hourly_limit as usize
            || self.last_emitted.is_some_and(|last| now - last < settings.cooldown_seconds as f64) {
            return None;
        }
        self.last_emitted = Some(now);
        self.emitted.push_back(now);
        Some(AutoSignal { kind, occurred_at, duration_ms: s.duration_ms as u64, tool_errors: s.tool_errors as u64 })
    }
}
