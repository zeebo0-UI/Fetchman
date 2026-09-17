use std::{collections::VecDeque, time::Duration};

#[derive(Debug)]
pub struct Adaptive {
    pub target: usize,
    ceiling: usize,
    enabled: bool,
    samples: VecDeque<f64>,
    probe: Option<(usize, f64)>,
    stable_rate: Option<f64>,
    bad_windows: u8,
    settle_until: Duration,
    cooldown_until: Duration,
}

impl Adaptive {
    pub fn new(ceiling: usize, enabled: bool) -> Self {
        Self {
            target: if enabled { ceiling.min(2) } else { ceiling },
            ceiling,
            enabled,
            samples: VecDeque::new(),
            probe: None,
            stable_rate: None,
            bad_windows: 0,
            settle_until: Duration::ZERO,
            cooldown_until: Duration::ZERO,
        }
    }
    pub fn overload(&mut self, now: Duration) {
        if self.enabled {
            self.target = (self.target / 2).max(1);
        }
        self.samples.clear();
        self.probe = None;
        self.stable_rate = None;
        self.cooldown_until = now + Duration::from_secs(30);
    }
    pub fn sample(&mut self, now: Duration, rate: f64, remaining: usize, eligible: bool) -> usize {
        if !self.enabled {
            return self.target;
        }
        if !eligible || now < self.settle_until {
            self.samples.clear();
            return self.target;
        }
        self.samples.push_back(rate);
        if self.samples.len() < 6 {
            return self.target;
        }
        let mut sorted: Vec<f64> = self.samples.drain(..).collect();
        sorted.sort_by(f64::total_cmp);
        let median = (sorted[2] + sorted[3]) / 2.0;
        if let Some((previous, baseline)) = self.probe.take() {
            let useful = if self.target > previous {
                median >= baseline * 1.10 && median > 0.0
            } else {
                median >= baseline * 0.95
            };
            if !useful {
                self.target = previous;
                self.cooldown_until = now + Duration::from_secs(30);
            }
            self.stable_rate = Some(if useful { median } else { baseline });
            self.settle_until = now + Duration::from_secs(2);
            return self.target;
        }
        if self.stable_rate.is_some_and(|r| median < r * 0.8) {
            self.bad_windows += 1;
        } else {
            self.bad_windows = 0;
        }
        if self.bad_windows >= 2 && self.target > 1 {
            self.bad_windows = 0;
            self.probe = Some((self.target, median));
            self.target -= 1;
            self.settle_until = now + Duration::from_secs(2);
        } else if now >= self.cooldown_until
            && self.target < self.ceiling
            && remaining >= 2 * (self.target + 1)
            && median > 0.0
        {
            self.probe = Some((self.target, median));
            self.target += 1;
            self.settle_until = now + Duration::from_secs(2);
        } else if self.bad_windows == 0 {
            self.stable_rate = Some(median);
        }
        self.target
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grows_only_for_real_improvement() {
        let mut a = Adaptive::new(8, true);
        for second in 1..=6 {
            a.sample(Duration::from_secs(second), 100.0, 100, true);
        }
        assert_eq!(a.target, 3);
        for second in 7..=13 {
            a.sample(Duration::from_secs(second), 101.0, 100, true);
        }
        assert_eq!(a.target, 2);
        for second in 14..=30 {
            a.sample(Duration::from_secs(second), 100.0, 100, true);
        }
        assert_eq!(a.target, 2);
    }
    #[test]
    fn fixed_and_backpressure_do_not_tune() {
        let mut a = Adaptive::new(4, false);
        let mut b = Adaptive::new(8, true);
        for second in 1..100 {
            a.sample(Duration::from_secs(second), 10.0, 100, true);
            b.sample(Duration::from_secs(second), 10.0, 100, false);
        }
        assert_eq!(a.target, 4);
        assert_eq!(b.target, 2);
    }

    #[test]
    fn retains_faster_probe_and_honors_ceiling() {
        let mut controller = Adaptive::new(3, true);
        for second in 1..=6 {
            controller.sample(Duration::from_secs(second), 100.0, 100, true);
        }
        for second in 7..=30 {
            controller.sample(Duration::from_secs(second), 130.0, 100, true);
        }
        assert_eq!(controller.target, 3);
        controller.overload(Duration::from_secs(31));
        assert_eq!(controller.target, 1);
        for second in 32..=50 {
            controller.sample(Duration::from_secs(second), 100.0, 100, true);
        }
        assert_eq!(controller.target, 1);
    }
}
