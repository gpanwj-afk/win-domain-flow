use crate::model::{
    ApplicationCounters, ApplicationDomainDelta, ApplicationFlushBatch, Counters, DomainDelta,
    FlushBatch, HISTORICAL_APPLICATION, UNKNOWN_APPLICATION, UNKNOWN_DOMAIN,
};
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
        let domain = normalize_domain(delta.domain);
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

#[derive(Debug, Default)]
pub struct ApplicationAccumulator {
    buckets: HashMap<(i64, String, String), ApplicationCounters>,
}

impl ApplicationAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, delta: ApplicationDomainDelta) {
        let application = normalize_application(delta.application);
        let domain = normalize_domain(delta.domain);
        let key = (delta.day_start_utc, application, domain);
        let entry = self.buckets.entry(key).or_default();
        entry.add_saturating(delta.counters, delta.breakdown);
    }

    pub fn add_all<I>(&mut self, deltas: I)
    where
        I: IntoIterator<Item = ApplicationDomainDelta>,
    {
        for delta in deltas {
            self.add(delta);
        }
    }

    pub fn drain(&mut self) -> ApplicationFlushBatch {
        let map = mem::take(&mut self.buckets);
        let mut rows: Vec<ApplicationDomainDelta> = map
            .into_iter()
            .map(
                |((day_start_utc, application, domain), counters)| ApplicationDomainDelta {
                    day_start_utc,
                    application,
                    domain,
                    counters: counters.counters,
                    breakdown: counters.breakdown,
                },
            )
            .collect();

        rows.sort_by(|a, b| {
            a.day_start_utc
                .cmp(&b.day_start_utc)
                .then(a.application.cmp(&b.application))
                .then(a.domain.cmp(&b.domain))
        });

        ApplicationFlushBatch { rows }
    }

    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

fn normalize_domain(domain: String) -> String {
    if domain.trim().is_empty() {
        UNKNOWN_DOMAIN.to_string()
    } else {
        domain
    }
}

fn normalize_application(application: String) -> String {
    let trimmed = application.trim();
    if trimmed.is_empty() {
        UNKNOWN_APPLICATION.to_string()
    } else if trimmed == HISTORICAL_APPLICATION {
        HISTORICAL_APPLICATION.to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{TrafficBreakdown, TransportProtocol};

    fn domain_delta(day: i64, domain: &str, bytes: u64, packets: u64) -> DomainDelta {
        DomainDelta {
            day_start_utc: day,
            domain: domain.to_string(),
            counters: Counters { bytes, packets },
        }
    }

    fn app_delta(
        day: i64,
        application: &str,
        domain: &str,
        bytes: u64,
        packets: u64,
        upload: bool,
        protocol: TransportProtocol,
    ) -> ApplicationDomainDelta {
        let mut breakdown = TrafficBreakdown::default();
        for _ in 0..packets {
            breakdown.add_saturating(TrafficBreakdown::from_packet(
                bytes / packets.max(1),
                upload,
                protocol,
            ));
        }
        ApplicationDomainDelta {
            day_start_utc: day,
            application: application.to_string(),
            domain: domain.to_string(),
            counters: Counters { bytes, packets },
            breakdown,
        }
    }

    #[test]
    fn adds_same_domain_key_with_saturation() {
        let mut acc = DomainAccumulator::new();
        acc.add(domain_delta(86_400, "example.com", 100, 1));
        acc.add(domain_delta(86_400, "example.com", 50, 2));

        assert_eq!(acc.len(), 1);
        let batch = acc.drain();
        assert_eq!(batch.rows[0].counters.bytes, 150);
        assert_eq!(batch.rows[0].counters.packets, 3);
    }

    #[test]
    fn application_rows_are_kept_separate_and_details_are_added() {
        let mut acc = ApplicationAccumulator::new();
        acc.add(app_delta(
            86_400,
            "chrome.exe",
            "example.com",
            100,
            1,
            true,
            TransportProtocol::Tcp,
        ));
        acc.add(app_delta(
            86_400,
            "msedge.exe",
            "example.com",
            200,
            2,
            false,
            TransportProtocol::Udp,
        ));
        acc.add(app_delta(
            86_400,
            "chrome.exe",
            "example.com",
            50,
            1,
            false,
            TransportProtocol::Tcp,
        ));

        let batch = acc.drain();
        assert_eq!(batch.rows.len(), 2);
        assert_eq!(batch.rows[0].application, "chrome.exe");
        assert_eq!(batch.rows[0].counters.bytes, 150);
        assert_eq!(batch.rows[0].breakdown.upload_bytes, 100);
        assert_eq!(batch.rows[0].breakdown.download_bytes, 50);
        assert_eq!(batch.rows[1].application, "msedge.exe");
        assert_eq!(batch.rows[1].breakdown.udp_bytes, 200);
    }

    #[test]
    fn empty_names_are_normalized() {
        let mut acc = ApplicationAccumulator::new();
        acc.add(app_delta(
            86_400,
            " ",
            "",
            10,
            1,
            true,
            TransportProtocol::Tcp,
        ));
        let batch = acc.drain();
        assert_eq!(batch.rows[0].application, UNKNOWN_APPLICATION);
        assert_eq!(batch.rows[0].domain, UNKNOWN_DOMAIN);
    }

    #[test]
    fn drain_sorts_and_clears() {
        let mut acc = DomainAccumulator::new();
        acc.add(domain_delta(172_800, "beta.com", 50, 1));
        acc.add(domain_delta(86_400, "alpha.com", 100, 2));
        acc.add(domain_delta(86_400, "gamma.com", 75, 1));

        let batch = acc.drain();
        assert!(acc.is_empty());
        assert_eq!(batch.rows[0].domain, "alpha.com");
        assert_eq!(batch.rows[1].domain, "gamma.com");
        assert_eq!(batch.rows[2].domain, "beta.com");
    }
}
