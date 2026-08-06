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

pub trait ProcessAttributor: Send {
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

#[cfg(windows)]
mod windows_backend {
    use super::{AttributionError, ProcessAttributor, ProcessIdentity};
    use crate::model::{Endpoint, FlowKey, TransportProtocol};
    use netstat2::{
        get_sockets_info, AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo, SocketInfo,
    };
    use std::collections::{BTreeSet, HashMap};
    use std::hash::Hash;
    use std::time::{Duration, Instant};
    use sysinfo::{Pid, ProcessesToUpdate, System};

    const CACHE_TTL: Duration = Duration::from_millis(250);

    pub struct WindowsProcessAttributor {
        last_refresh: Option<Instant>,
        tcp_flows: HashMap<FlowKey, Option<u32>>,
        udp_endpoints: HashMap<Endpoint, Option<u32>>,
        udp_ports: HashMap<u16, Option<u32>>,
        identities: HashMap<u32, ProcessIdentity>,
        system: System,
    }

    impl Default for WindowsProcessAttributor {
        fn default() -> Self {
            Self {
                last_refresh: None,
                tcp_flows: HashMap::new(),
                udp_endpoints: HashMap::new(),
                udp_ports: HashMap::new(),
                identities: HashMap::new(),
                system: System::new(),
            }
        }
    }

    impl WindowsProcessAttributor {
        fn cache_is_stale(&self) -> bool {
            self.last_refresh
                .is_none_or(|last| last.elapsed() >= CACHE_TTL)
        }

        fn refresh(&mut self) -> Result<(), AttributionError> {
            let sockets = get_sockets_info(
                AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6,
                ProtocolFlags::TCP | ProtocolFlags::UDP,
            )
            .map_err(|error| AttributionError::Unavailable(error.to_string()))?;

            let pids: BTreeSet<u32> = sockets
                .iter()
                .flat_map(|socket| socket.associated_pids.iter().copied())
                .filter(|pid| *pid != 0)
                .collect();
            let sysinfo_pids: Vec<Pid> = pids.iter().copied().map(Pid::from_u32).collect();
            if !sysinfo_pids.is_empty() {
                self.system
                    .refresh_processes(ProcessesToUpdate::Some(&sysinfo_pids), true);
            }

            self.tcp_flows.clear();
            self.udp_endpoints.clear();
            self.udp_ports.clear();
            self.identities.clear();

            for pid in pids {
                self.identities
                    .insert(pid, process_identity(&self.system, pid));
            }

            for socket in sockets {
                let Some(pid) = preferred_pid(&socket) else {
                    continue;
                };

                match socket.protocol_socket_info {
                    ProtocolSocketInfo::Tcp(tcp) => {
                        let flow = FlowKey::canonical(
                            TransportProtocol::Tcp,
                            Endpoint {
                                ip: tcp.local_addr,
                                port: tcp.local_port,
                            },
                            Endpoint {
                                ip: tcp.remote_addr,
                                port: tcp.remote_port,
                            },
                        );
                        insert_unique(&mut self.tcp_flows, flow, pid);
                    }
                    ProtocolSocketInfo::Udp(udp) => {
                        let endpoint = Endpoint {
                            ip: udp.local_addr,
                            port: udp.local_port,
                        };
                        insert_unique(&mut self.udp_endpoints, endpoint, pid);
                        insert_unique(&mut self.udp_ports, udp.local_port, pid);
                    }
                }
            }

            self.last_refresh = Some(Instant::now());
            Ok(())
        }

        fn find_pid(&self, flow: &FlowKey) -> Option<u32> {
            match flow.protocol {
                TransportProtocol::Tcp => self.tcp_flows.get(flow).copied().flatten(),
                TransportProtocol::Udp => self
                    .udp_endpoints
                    .get(&flow.first)
                    .or_else(|| self.udp_endpoints.get(&flow.second))
                    .copied()
                    .flatten()
                    .or_else(|| {
                        self.udp_ports
                            .get(&flow.first.port)
                            .or_else(|| self.udp_ports.get(&flow.second.port))
                            .copied()
                            .flatten()
                    }),
            }
        }
    }

    impl ProcessAttributor for WindowsProcessAttributor {
        fn lookup(&mut self, flow: &FlowKey) -> Result<Option<ProcessIdentity>, AttributionError> {
            if self.cache_is_stale() {
                self.refresh()?;
            }

            Ok(self
                .find_pid(flow)
                .and_then(|pid| self.identities.get(&pid).cloned()))
        }

        fn backend_name(&self) -> &'static str {
            "windows-ip-helper"
        }
    }

    fn preferred_pid(socket: &SocketInfo) -> Option<u32> {
        socket
            .associated_pids
            .iter()
            .copied()
            .filter(|pid| *pid != 0)
            .min()
    }

    fn process_identity(system: &System, pid: u32) -> ProcessIdentity {
        let process = system.process(Pid::from_u32(pid));
        let name = process
            .and_then(|process| process.exe().and_then(|path| path.file_name()))
            .or_else(|| process.map(|process| process.name()))
            .map(|name| name.to_string_lossy().trim().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| format!("PID {pid}"));
        ProcessIdentity { pid, name }
    }

    fn insert_unique<K>(map: &mut HashMap<K, Option<u32>>, key: K, pid: u32)
    where
        K: Eq + Hash,
    {
        match map.entry(key) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(Some(pid));
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if entry.get().is_some_and(|existing| existing != pid) {
                    entry.insert(None);
                }
            }
        }
    }

    pub fn create() -> Box<dyn ProcessAttributor> {
        Box::new(WindowsProcessAttributor::default())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn duplicate_candidates_become_ambiguous() {
            let mut map = HashMap::new();
            insert_unique(&mut map, 443_u16, 10);
            insert_unique(&mut map, 443_u16, 20);
            assert_eq!(map.get(&443), Some(&None));
        }

        #[test]
        fn duplicate_same_pid_remains_stable() {
            let mut map = HashMap::new();
            insert_unique(&mut map, 443_u16, 10);
            insert_unique(&mut map, 443_u16, 10);
            assert_eq!(map.get(&443), Some(&Some(10)));
        }
    }
}

pub fn create_process_attributor() -> Box<dyn ProcessAttributor> {
    #[cfg(windows)]
    {
        windows_backend::create()
    }

    #[cfg(not(windows))]
    {
        Box::new(NoopProcessAttributor)
    }
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

    #[cfg(not(windows))]
    #[test]
    fn factory_returns_disabled_backend_off_windows() {
        let attributor = create_process_attributor();
        assert_eq!(attributor.backend_name(), "disabled");
    }
}
