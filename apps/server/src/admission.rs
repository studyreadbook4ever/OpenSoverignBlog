use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

/// A bounded, per-identity sliding-window limiter.
///
/// Callers hash untrusted identities before using them as keys. Keeping the
/// map bounded prevents a stream of one-off identities from becoming its own
/// memory-exhaustion vector, while isolating one identity's failures from all
/// other identities.
pub(crate) struct KeyedRateLimiter {
    buckets: HashMap<[u8; 32], AttemptBucket>,
    maximum_buckets: usize,
    attempts_per_window: usize,
    window: Duration,
}

struct AttemptBucket {
    attempts: VecDeque<Instant>,
    last_seen: Instant,
}

impl KeyedRateLimiter {
    pub(crate) fn new(
        maximum_buckets: usize,
        attempts_per_window: usize,
        window: Duration,
    ) -> Self {
        assert!(maximum_buckets > 0);
        assert!(attempts_per_window > 0);
        assert!(!window.is_zero());
        Self {
            buckets: HashMap::new(),
            maximum_buckets,
            attempts_per_window,
            window,
        }
    }

    pub(crate) fn admit(&mut self, key: [u8; 32]) -> bool {
        self.admit_at(key, Instant::now())
    }

    pub(crate) fn forget(&mut self, key: &[u8; 32]) {
        self.buckets.remove(key);
    }

    fn admit_at(&mut self, key: [u8; 32], now: Instant) -> bool {
        if !self.buckets.contains_key(&key) {
            self.buckets
                .retain(|_, bucket| now.saturating_duration_since(bucket.last_seen) < self.window);
            if self.buckets.len() >= self.maximum_buckets
                && let Some(oldest) = self
                    .buckets
                    .iter()
                    .min_by_key(|(_, bucket)| bucket.last_seen)
                    .map(|(key, _)| *key)
            {
                self.buckets.remove(&oldest);
            }
        }

        let bucket = self.buckets.entry(key).or_insert_with(|| AttemptBucket {
            attempts: VecDeque::new(),
            last_seen: now,
        });
        bucket.last_seen = now;
        while bucket
            .attempts
            .front()
            .is_some_and(|attempt| now.saturating_duration_since(*attempt) >= self.window)
        {
            bucket.attempts.pop_front();
        }
        if bucket.attempts.len() >= self.attempts_per_window {
            return false;
        }
        bucket.attempts.push_back(now);
        true
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_identity_cannot_lock_out_another() {
        let mut limiter = KeyedRateLimiter::new(8, 2, Duration::from_secs(60));
        let first = [1_u8; 32];
        let second = [2_u8; 32];
        assert!(limiter.admit(first));
        assert!(limiter.admit(first));
        assert!(!limiter.admit(first));
        assert!(limiter.admit(second));
    }

    #[test]
    fn one_off_identities_cannot_grow_the_map_without_bound() {
        let mut limiter = KeyedRateLimiter::new(4, 1, Duration::from_secs(60));
        for value in 0_u8..32 {
            assert!(limiter.admit([value; 32]));
        }
        assert_eq!(limiter.len(), 4);
    }

    #[test]
    fn the_window_expires_and_success_can_forget_a_bucket() {
        let mut limiter = KeyedRateLimiter::new(4, 1, Duration::from_secs(10));
        let key = [7_u8; 32];
        let start = Instant::now();
        assert!(limiter.admit_at(key, start));
        assert!(!limiter.admit_at(key, start + Duration::from_secs(9)));
        assert!(limiter.admit_at(key, start + Duration::from_secs(10)));
        limiter.forget(&key);
        assert!(limiter.admit_at(key, start + Duration::from_secs(10)));
    }
}
