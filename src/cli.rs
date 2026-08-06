use crate::capture::list_devices;
use crate::model::DEFAULT_BPF_FILTER;
use crate::runtime::{run_live, RuntimeConfig};
use crate::storage::Storage;
use clap::{Parser, Subcommand};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(name = "win-domain-flow")]
#[command(version)]
#[command(about = "Lightweight Windows domain-level network byte monitor")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List pcap/Npcap capture devices.
    Devices,

    /// Capture TCP/443 and UDP/443 and persist domain byte totals.
    Capture {
        #[arg(long)]
        interface: String,

        #[arg(long, default_value = "domainflow.db")]
        db: PathBuf,

        #[arg(long, default_value_t = 1)]
        flush_seconds: u64,

        #[arg(long, default_value_t = 300)]
        idle_seconds: u64,

        #[arg(long, default_value = DEFAULT_BPF_FILTER)]
        bpf: String,
    },

    /// Query top domains from SQLite.
    Top {
        #[arg(long, default_value = "domainflow.db")]
        db: PathBuf,

        #[arg(long, default_value_t = 1)]
        days: u32,

        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
}

pub fn run(cli: Cli) -> anyhow::Result<()> {
    let mut out = std::io::stdout();
    run_with_writer(cli, &mut out)
}

pub fn run_with_writer<W: Write>(cli: Cli, out: &mut W) -> anyhow::Result<()> {
    match cli.command {
        Command::Devices => {
            writeln!(out, "name\tdescription")?;
            let devices = list_devices().map_err(|e| anyhow::anyhow!(e))?;
            for dev in &devices {
                let desc = dev.description.as_deref().unwrap_or("");
                writeln!(out, "{}\t{}", sanitize_tsv(&dev.name), sanitize_tsv(desc))?;
            }
            Ok(())
        }
        Command::Capture {
            interface,
            db,
            flush_seconds,
            idle_seconds,
            bpf,
        } => {
            if interface.trim().is_empty() {
                anyhow::bail!("--interface must not be empty");
            }
            if bpf.trim().is_empty() {
                anyhow::bail!("--bpf must not be empty");
            }
            if !(1..=60).contains(&flush_seconds) {
                anyhow::bail!("--flush-seconds must be in 1..=60");
            }
            if !(1..=86_400).contains(&idle_seconds) {
                anyhow::bail!("--idle-seconds must be in 1..=86400");
            }

            let config = RuntimeConfig {
                flush_interval: Duration::from_secs(flush_seconds),
                idle_timeout: Duration::from_secs(idle_seconds),
                bpf_filter: bpf,
            };

            let summary = run_live(&interface, &db, config).map_err(|e| anyhow::anyhow!(e))?;

            writeln!(
                out,
                "captured={}\taccepted={}\tparse_errors={}\tresolved_flows={}\tbatches={}",
                summary.captured_packets,
                summary.accepted_packets,
                summary.parse_errors,
                summary.resolved_flows,
                summary.submitted_batches,
            )?;

            Ok(())
        }
        Command::Top { db, days, limit } => {
            if !(1..=3650).contains(&days) {
                anyhow::bail!("--days must be in 1..=3650");
            }
            if !(1..=1000).contains(&limit) {
                anyhow::bail!("--limit must be in 1..=1000");
            }

            let storage = Storage::open(&db).map_err(|e| anyhow::anyhow!(e))?;
            let rows = storage
                .top_domains_recent(days, limit)
                .map_err(|e| anyhow::anyhow!(e))?;

            render_top_rows(&rows, out)?;
            Ok(())
        }
    }
}

pub fn render_top_rows<W: Write>(
    rows: &[crate::model::TopDomainRow],
    out: &mut W,
) -> std::io::Result<()> {
    writeln!(out, "domain\tbytes\tpackets")?;
    for row in rows {
        writeln!(
            out,
            "{}\t{}\t{}",
            sanitize_tsv(&row.domain),
            row.bytes,
            row.packets
        )?;
    }
    Ok(())
}

fn sanitize_tsv(s: &str) -> String {
    s.replace(['\t', '\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_capture_defaults() {
        let cli =
            Cli::try_parse_from(["win-domain-flow", "capture", "--interface", "eth0"]).unwrap();
        match cli.command {
            Command::Capture {
                interface,
                db,
                flush_seconds,
                idle_seconds,
                bpf,
            } => {
                assert_eq!(interface, "eth0");
                assert_eq!(db, PathBuf::from("domainflow.db"));
                assert_eq!(flush_seconds, 1);
                assert_eq!(idle_seconds, 300);
                assert_eq!(bpf, DEFAULT_BPF_FILTER);
            }
            _ => panic!("expected Capture command"),
        }
    }

    #[test]
    fn cli_rejects_missing_interface() {
        let result = Cli::try_parse_from(["win-domain-flow", "capture"]);
        assert!(result.is_err());
    }

    #[test]
    fn top_output_is_stable_tsv() {
        use crate::model::TopDomainRow;

        let rows = vec![
            TopDomainRow {
                domain: "example.com".to_string(),
                bytes: 12345,
                packets: 100,
            },
            TopDomainRow {
                domain: "(unknown)".to_string(),
                bytes: 6789,
                packets: 50,
            },
        ];

        let mut buf = Vec::new();
        render_top_rows(&rows, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();

        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines[0], "domain\tbytes\tpackets");
        assert_eq!(lines[1], "example.com\t12345\t100");
        assert_eq!(lines[2], "(unknown)\t6789\t50");
    }

    #[test]
    fn empty_top_still_prints_header() {
        let rows: Vec<crate::model::TopDomainRow> = vec![];

        let mut buf = Vec::new();
        render_top_rows(&rows, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();

        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "domain\tbytes\tpackets");
    }

    #[test]
    fn tsv_fields_are_sanitized() {
        let rows = vec![crate::model::TopDomainRow {
            domain: "bad\tdomain\nname\rhere".to_string(),
            bytes: 100,
            packets: 1,
        }];

        let mut buf = Vec::new();
        render_top_rows(&rows, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();

        assert!(output.contains("bad domain name here"));
        assert!(!output.contains('\t') || output.starts_with("domain\tbytes\tpackets"));
    }
}
