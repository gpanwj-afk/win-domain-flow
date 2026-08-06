use crate::model::{Counters, DomainDelta, FlushBatch, UNKNOWN_DOMAIN};
use std::collections::HashMap;
use std::mem;

#[derive(Debug, Default)]
pub struct DomainAccumulator {
    buckets: HashMap<(i64, String), Counters>,
}

impl DomainAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, delta: DomainDelta) {
        let domain = if delta.domain.is_empty() {
            UNKNOWN_DOMAIN.to_string()
        } else {
            delta.domain
        };

        let key = (delta.day_start_utc, domain);
        let entry = self.buckets.entry(key).or_default();
        entry.add_saturating(delta.counters.bytes, delta.counters.packets);
    }

    pub fn add_all<I>(&mut self, deltas: I)
    where
        I: IntoIterator<Item = DomainDelta>,
    {
        for delta in deltas {
            self.add(delta);
        }
    }

    pub fn drain(&mut self) -> FlushBatch {
        let map = mem::take(&mut self.buckets);
        let mut rows: Vec<DomainDelta> = map
            .into_iter()
            .map(|((day, domain), counters)| DomainDelta {
                day_start_utc: day,
                domain,
                counters,
            })
            .collect();

        rows.sort_by(|a, b| {
            a.day_start_utc
                .cmp(&b.day_start_utc)
                .then(a.domain.cmp(&b.domain))
        });

        FlushBatch { rows }
    }

    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_same_key_with_saturation() {
        let mut acc = DomainAccumulator::new();

        acc.add(DomainDelta {
            day_start_utc: 86400,
            domain: "example.com".to_string(),
            counters: Counters {
                bytes: 100,
                packets: 1,
            },
        });

        acc.add(DomainDelta {
            day_start_utc: 86400,
            domain: "example.com".to_string(),
            counters: Counters {
                bytes: 50,
                packets: 2,
            },
        });

        assert_eq!(acc.len(), 1);
        let batch = acc.drain();
        assert_eq!(batch.rows.len(), 1);
        assert_eq!(batch.rows[0].counters.bytes, 150);
        assert_eq!(batch.rows[0].counters.packets, 3);
    }

    #[test]
    fn empty_domain_becomes_unknown() {
        let mut acc = DomainAccumulator::new();

        acc.add(DomainDelta {
            day_start_utc: 86400,
            domain: "".to_string(),
            counters: Counters {
                bytes: 100,
                packets: 1,
            },
        });

        assert_eq!(acc.len(), 1);
        let batch = acc.drain();
        assert_eq!(batch.rows.len(), 1);
        assert_eq!(batch.rows[0].domain, UNKNOWN_DOMAIN);
    }

    #[test]
    fn drain_sorts_and_clears() {
        let mut acc = DomainAccumulator::new();

        acc.add(DomainDelta {
            day_start_utc: 172800,
            domain: "beta.com".to_string(),
            counters: Counters {
                bytes: 50,
                packets: 1,
            },
        });

        acc.add(DomainDelta {
            day_start_utc: 86400,
            domain: "alpha.com".to_string(),
            counters: Counters {
                bytes: 100,
                packets: 2,
            },
        });

        acc.add(DomainDelta {
            day_start_utc: 86400,
            domain: "gamma.com".to_string(),
            counters: Counters {
                bytes: 75,
                packets: 1,
            },
        });

        assert_eq!(acc.len(), 3);
        let batch = acc.drain();
        assert!(acc.is_empty());

        assert_eq!(batch.rows.len(), 3);
        assert_eq!(batch.rows[0].domain, "alpha.com");
        assert_eq!(batch.rows[0].day_start_utc, 86400);
        assert_eq!(batch.rows[1].domain, "gamma.com");
        assert_eq!(batch.rows[1].day_start_utc, 86400);
        assert_eq!(batch.rows[2].domain, "beta.com");
        assert_eq!(batch.rows[2].day_start_utc, 172800);
    }

    #[test]
    fn different_days_remain_separate() {
        let mut acc = DomainAccumulator::new();

        acc.add(DomainDelta {
            day_start_utc: 86400,
            domain: "example.com".to_string(),
            counters: Counters {
                bytes: 100,
                packets: 1,
            },
        });

        acc.add(DomainDelta {
            day_start_utc: 172800,
            domain: "example.com".to_string(),
            counters: Counters {
                bytes: 200,
                packets: 2,
            },
        });

        assert_eq!(acc.len(), 2);
        let batch = acc.drain();
        assert_eq!(batch.rows.len(), 2);
        assert_eq!(batch.rows[0].day_start_utc, 86400);
        assert_eq!(batch.rows[0].counters.bytes, 100);
        assert_eq!(batch.rows[1].day_start_utc, 172800);
        assert_eq!(batch.rows[1].counters.bytes, 200);
    }
}
