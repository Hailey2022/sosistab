use std::time::{Duration, Instant};

/// A high-precision pacer that uses async-io's timers under the hood.
pub struct Pacer {
    next_pace_time: Instant,
    timer: smol::Timer,
    interval: Duration,
}

impl Pacer {
    /// Creates a new pacer with a new interval.
    pub fn new(interval: Duration) -> Self {
        Self {
            next_pace_time: Instant::now(),
            timer: smol::Timer::at(Instant::now()),
            interval,
        }
    }

    /// Waits until the next time.
    pub async fn wait_next(&mut self) {
        self.next_pace_time = Instant::now().max(self.next_pace_time + self.interval);
        self.timer.set_at(self.next_pace_time);
        (&mut self.timer).await;
    }

    /// Changes the interval.
    pub fn set_interval(&mut self, interval: Duration) {
        self.interval = interval
    }
}
