//! Playing an animation: which frame is shown when.
//!
//! A [`Playback`] follows one animation's frames as time passes. It runs only
//! while the animation is on screen: [`Playback::advance`] moves it on to the
//! frame due at the time given and says when the next one is due, and
//! [`Playback::pause`] stops the clock, so an animation the user moves away
//! from carries on where it left off when they come back.

use std::time::{Duration, Instant};

/// Where an animation is in its frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Playback {
    delays: Vec<Duration>,
    plays: Option<u32>,
    frame: usize,
    /// When the shown frame gives way to the next; `None` while paused or
    /// before it starts.
    due: Option<Instant>,
    /// How long the shown frame has left to run, kept while paused.
    left: Option<Duration>,
    /// Plays completed.
    played: u32,
    finished: bool,
}

impl Playback {
    /// An animation of frames shown for `delays`, playing through `plays`
    /// times, or forever if `None`, starting at its first frame.
    pub fn new(delays: Vec<Duration>, plays: Option<u32>) -> Self {
        Playback {
            finished: delays.len() < 2 || plays == Some(0),
            delays,
            plays,
            frame: 0,
            due: None,
            left: None,
            played: 0,
        }
    }

    /// The frame shown now.
    pub fn frame(&self) -> usize {
        self.frame
    }

    /// Whether the animation has played as often as it should, and stays on
    /// its last frame.
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Moves on to the frame due at `now`, starting or resuming the clock if
    /// it is stopped, and returns when the next frame is due, or `None` if
    /// none is.
    pub fn advance(&mut self, now: Instant) -> Option<Instant> {
        if self.finished {
            return None;
        }
        let mut due = match self.due {
            Some(due) => due,
            None => now + self.left.take().unwrap_or(self.delays[self.frame]),
        };
        // After a long stall, such as a suspended machine, carry on from the
        // shown frame rather than replaying the time missed.
        let cycle: Duration = self.delays.iter().sum();
        if now.saturating_duration_since(due) > cycle {
            due = now;
        }
        while now >= due {
            if self.frame + 1 < self.delays.len() {
                self.frame += 1;
            } else {
                self.played += 1;
                if self.plays.is_some_and(|plays| self.played >= plays) {
                    self.finished = true;
                    self.due = None;
                    return None;
                }
                self.frame = 0;
            }
            due += self.delays[self.frame];
        }
        self.due = Some(due);
        Some(due)
    }

    /// Stops the clock at `now`, keeping the time the shown frame has left.
    pub fn pause(&mut self, now: Instant) {
        if let Some(due) = self.due.take() {
            self.left = Some(due.saturating_duration_since(now));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn frames_follow_their_delays_and_loop() {
        let start = Instant::now();
        let mut playback = Playback::new(vec![ms(100), ms(200)], None);
        assert_eq!(playback.advance(start), Some(start + ms(100)));
        assert_eq!(playback.frame(), 0);
        assert_eq!(playback.advance(start + ms(99)), Some(start + ms(100)));
        assert_eq!(playback.advance(start + ms(100)), Some(start + ms(300)));
        assert_eq!(playback.frame(), 1);
        // A late wake-up skips the frames whose time has passed.
        assert_eq!(playback.advance(start + ms(450)), Some(start + ms(600)));
        assert_eq!(playback.frame(), 1);
        assert!(!playback.finished());
    }

    #[test]
    fn a_finite_animation_stops_on_its_last_frame() {
        let start = Instant::now();
        let mut playback = Playback::new(vec![ms(10), ms(10), ms(10)], Some(2));
        playback.advance(start);
        // The first play ends at 30 ms, and the second starts over.
        assert_eq!(playback.advance(start + ms(25)), Some(start + ms(30)));
        assert_eq!(playback.frame(), 2);
        assert_eq!(playback.advance(start + ms(30)), Some(start + ms(40)));
        assert_eq!(playback.frame(), 0);
        assert_eq!(playback.advance(start + ms(55)), Some(start + ms(60)));
        assert_eq!(playback.frame(), 2);
        assert_eq!(playback.advance(start + ms(60)), None);
        assert!(playback.finished());
        assert_eq!(playback.frame(), 2);
        assert_eq!(playback.advance(start + ms(1000)), None);
    }

    #[test]
    fn single_frames_and_zero_plays_never_move() {
        let now = Instant::now();
        let mut still = Playback::new(vec![ms(100)], None);
        assert!(still.finished());
        assert_eq!(still.advance(now), None);
        assert_eq!(
            Playback::new(vec![ms(1), ms(1)], Some(0)).advance(now),
            None
        );
    }

    #[test]
    fn pausing_keeps_the_time_left() {
        let start = Instant::now();
        let mut playback = Playback::new(vec![ms(100), ms(100)], None);
        playback.advance(start);
        playback.pause(start + ms(40));
        playback.pause(start + ms(50));
        // Resumed much later, the frame still has 60 ms to go.
        let later = start + ms(5000);
        assert_eq!(playback.advance(later), Some(later + ms(60)));
        assert_eq!(playback.frame(), 0);
    }

    #[test]
    fn a_long_stall_carries_on_from_the_shown_frame() {
        let start = Instant::now();
        let mut playback = Playback::new(vec![ms(100), ms(100)], None);
        playback.advance(start);
        let later = start + ms(10_000);
        assert_eq!(playback.advance(later), Some(later + ms(100)));
        assert_eq!(playback.frame(), 1);
    }
}
