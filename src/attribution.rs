use crate::model::FlowKey;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub name: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AttributionError {
    #[error("process attribution unavailable: {0}")]
    Unavailable(String),
}

pub trait ProcessAttributor: Send + Sync {
    fn lookup(&mut self, flow: &FlowKey) -> Result<Option<ProcessIdentity>, AttributionError>;

    fn backend_name(&self) -> &'static str;
}

#[derive(Debug, Default)]
pub struct NoopProcessAttributor;

impl ProcessAttributor for NoopProcessAttributor {
    fn lookup(&mut self, _flow: &FlowKey) -> Result<Option<ProcessIdentity>, AttributionError> {
        Ok(None)
    }

    fn backend_name(&self) -> &'static str {
        "disabled"
    }
}

pub fn create_process_attributor() -> Box<dyn ProcessAttributor> {
    Box::new(NoopProcessAttributor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    #[test]
    fn noop_lookup_returns_none() {
        let mut attributor = NoopProcessAttributor;
        let flow = FlowKey::canonical(
            crate::model::TransportProtocol::Tcp,
            crate::model::Endpoint {
                ip: IpAddr::from([10, 0, 0, 1]),
                port: 12345,
            },
            crate::model::Endpoint {
                ip: IpAddr::from([93, 184, 216, 34]),
                port: 443,
            },
        );
        assert_eq!(attributor.lookup(&flow).unwrap(), None);
    }

    #[test]
    fn noop_backend_name_is_disabled() {
        let attributor = NoopProcessAttributor;
        assert_eq!(attributor.backend_name(), "disabled");
    }

    #[test]
    fn factory_returns_disabled_backend() {
        let attributor = create_process_attributor();
        assert_eq!(attributor.backend_name(), "disabled");
    }
}
