use std::time::{Duration, Instant};

use anyhow::{Result, ensure};

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Load(usize),
    Replay,
    Finish,
}

pub struct Scheduler {
    count: usize,
    position: usize,
    interval: Option<Duration>,
    repeat: bool,
    change_on_end: bool,
    paused_at: Option<Instant>,
    deadline: Option<Instant>,
    loading: bool,
    finished: bool,
}

impl Scheduler {
    pub fn new(
        count: usize,
        interval: Option<Duration>,
        repeat: bool,
        change_on_end: bool,
    ) -> Self {
        assert!(count > 0);
        Self {
            count,
            position: 0,
            interval: interval.filter(|d| !d.is_zero()),
            repeat,
            change_on_end,
            paused_at: None,
            deadline: None,
            loading: true,
            finished: false,
        }
    }

    pub fn position(&self) -> usize {
        self.position
    }
    pub fn loading(&self) -> bool {
        self.loading
    }

    pub fn file_loaded(&mut self, now: Instant) {
        if self.loading && !self.finished {
            self.loading = false;
            self.deadline = self.interval.map(|interval| now + interval);
            if self.paused_at.is_some() {
                self.paused_at = Some(now);
            }
        }
    }

    pub fn eof(&mut self, now: Instant) -> Option<Action> {
        if self.loading || self.finished {
            return None;
        }
        if !self.change_on_end {
            Some(self.tick(now).unwrap_or(Action::Replay))
        } else {
            Some(self.advance(now))
        }
    }

    pub fn set_paused(&mut self, paused: bool, now: Instant) {
        match (self.paused_at, paused) {
            (None, true) => self.paused_at = Some(now),
            (Some(start), false) => {
                if let Some(deadline) = &mut self.deadline {
                    *deadline += now.saturating_duration_since(start);
                }
                self.paused_at = None;
            }
            _ => (),
        }
    }

    pub fn advance(&mut self, now: Instant) -> Action {
        if self.finished {
            return Action::Finish;
        }
        if self.count == 1 && self.repeat {
            self.deadline = self.interval.map(|interval| now + interval);
            return Action::Replay;
        }
        if self.position + 1 < self.count {
            self.load(self.position + 1)
        } else if self.repeat {
            self.load(0)
        } else {
            self.finished = true;
            self.loading = false;
            self.deadline = None;
            Action::Finish
        }
    }

    pub fn previous(&mut self, _now: Instant) -> Action {
        self.load(if self.position > 0 {
            self.position - 1
        } else if self.repeat {
            self.count - 1
        } else {
            0
        })
    }

    pub fn select(&mut self, position: usize, _now: Instant) -> Result<Action> {
        ensure!(position < self.count, "playlist position out of range");
        ensure!(!self.finished, "playback has finished");
        Ok(self.load(position))
    }

    pub fn tick(&mut self, now: Instant) -> Option<Action> {
        if self.paused_at.is_none()
            && !self.loading
            && !self.finished
            && self.deadline.is_some_and(|deadline| now >= deadline)
        {
            Some(self.advance(now))
        } else {
            None
        }
    }

    pub fn wait(&self, now: Instant) -> Option<Duration> {
        if self.paused_at.is_some() {
            return None;
        }
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    fn load(&mut self, position: usize) -> Action {
        self.position = position;
        self.loading = true;
        self.deadline = None;
        Action::Load(position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eof_loops_do_not_extend_interval_and_duplicate_load_events_are_ignored() {
        let t = Instant::now();
        let mut s = Scheduler::new(2, Some(Duration::from_secs(10)), true, false);
        s.file_loaded(t);
        assert_eq!(s.eof(t + Duration::from_secs(3)), Some(Action::Replay));
        s.file_loaded(t + Duration::from_secs(3));
        assert_eq!(
            s.wait(t + Duration::from_secs(4)),
            Some(Duration::from_secs(6))
        );
        assert_eq!(s.eof(t + Duration::from_secs(10)), Some(Action::Load(1)));
        assert_eq!(s.tick(t + Duration::from_secs(10)), None);
        assert_eq!(s.eof(t + Duration::from_secs(10)), None);
    }

    #[test]
    fn natural_end_obeys_wrap_and_final_stop_including_single_entry_lists() {
        let t = Instant::now();
        let mut s = Scheduler::new(2, None, false, true);
        s.file_loaded(t);
        assert_eq!(s.eof(t), Some(Action::Load(1)));
        s.file_loaded(t);
        assert_eq!(s.eof(t), Some(Action::Finish));
        assert_eq!(s.eof(t), None);
        let mut repeating = Scheduler::new(1, None, true, true);
        repeating.file_loaded(t);
        assert_eq!(repeating.eof(t), Some(Action::Replay));
        assert!(!repeating.loading());
        assert_eq!(
            repeating.eof(t + Duration::from_secs(1)),
            Some(Action::Replay)
        );
    }

    #[test]
    fn manual_navigation_resets_deadline_and_handles_first_entry_boundaries() {
        let t = Instant::now();
        let mut s = Scheduler::new(3, Some(Duration::from_secs(5)), false, false);
        s.file_loaded(t);
        assert_eq!(s.advance(t + Duration::from_secs(2)), Action::Load(1));
        assert!(s.loading());
        assert_eq!(s.wait(t), None);
        s.file_loaded(t + Duration::from_secs(3));
        assert_eq!(
            s.wait(t + Duration::from_secs(4)),
            Some(Duration::from_secs(4))
        );
        assert_eq!(s.select(0, t).unwrap(), Action::Load(0));
        s.file_loaded(t);
        assert_eq!(s.previous(t), Action::Load(0));
        let mut r = Scheduler::new(3, None, true, true);
        r.file_loaded(t);
        assert_eq!(r.previous(t), Action::Load(2));
    }

    #[test]
    fn timers_use_wall_clock_and_finish_last_entry_only_at_deadline() {
        let t = Instant::now();
        let mut s = Scheduler::new(1, Some(Duration::from_secs(5)), false, false);
        assert_eq!(s.tick(t + Duration::from_secs(20)), None);
        s.file_loaded(t);
        assert_eq!(s.eof(t + Duration::from_secs(1)), Some(Action::Replay));
        assert_eq!(s.tick(t + Duration::from_secs(20)), Some(Action::Finish));
        assert_eq!(s.wait(t), None);
    }

    #[test]
    fn invalid_selection_does_not_change_position_or_deadline() {
        let t = Instant::now();
        let mut s = Scheduler::new(2, Some(Duration::from_secs(5)), true, false);
        s.file_loaded(t);
        assert!(s.select(2, t).is_err());
        assert_eq!(s.position(), 0);
        assert_eq!(
            s.wait(t + Duration::from_secs(1)),
            Some(Duration::from_secs(4))
        );
    }

    #[test]
    fn pause_freezes_remaining_interval_and_manual_selection_restarts_it() {
        let t = Instant::now();
        let mut s = Scheduler::new(2, Some(Duration::from_secs(10)), true, false);
        s.file_loaded(t);
        s.set_paused(true, t + Duration::from_secs(3));
        assert_eq!(s.tick(t + Duration::from_secs(30)), None);
        assert_eq!(s.wait(t + Duration::from_secs(30)), None);
        s.set_paused(false, t + Duration::from_secs(30));
        assert_eq!(
            s.wait(t + Duration::from_secs(30)),
            Some(Duration::from_secs(7))
        );
        assert_eq!(s.tick(t + Duration::from_secs(37)), Some(Action::Load(1)));
        s.set_paused(true, t + Duration::from_secs(37));
        s.file_loaded(t + Duration::from_secs(38));
        s.set_paused(false, t + Duration::from_secs(50));
        assert_eq!(
            s.wait(t + Duration::from_secs(50)),
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn video_end_override_switches_before_timer_and_disabled_end_repeats_without_timer() {
        let t = Instant::now();
        let mut s = Scheduler::new(2, Some(Duration::from_secs(30)), true, true);
        s.file_loaded(t);
        assert_eq!(s.eof(t + Duration::from_secs(1)), Some(Action::Load(1)));
        let mut looping = Scheduler::new(2, None, true, false);
        looping.file_loaded(t);
        assert_eq!(looping.eof(t), Some(Action::Replay));
        assert_eq!(looping.position(), 0);
    }
}
