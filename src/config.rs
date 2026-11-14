use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::ArgGroup;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;

#[derive(Parser)]
#[command(version, about, disable_help_subcommand = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Accept public traffic and an authenticated origin.
    Edge(Edge),
    /// Connect to the edge and forward traffic to fixed targets.
    Origin(Origin),
}

#[derive(Args)]
#[command(group(
    ArgGroup::new("listeners")
        .args(["listen_tcp", "listen_udp"])
        .required(true)
        .multiple(true)
))]
pub struct Edge {
    /// Public TCP listener.
    #[arg(long)]
    pub listen_tcp: Option<SocketAddr>,
    /// Public UDP listener, separate from --bind.
    #[arg(long)]
    pub listen_udp: Option<SocketAddr>,
    /// QUIC backhaul listener.
    #[arg(long, default_value = "127.0.0.1:9443")]
    pub bind: SocketAddr,
    #[command(flatten)]
    pub transport: Transport,
}

#[derive(Args, Clone)]
#[command(group(
    ArgGroup::new("targets")
        .args(["target_tcp", "target_udp"])
        .required(true)
        .multiple(true)
))]
pub struct Origin {
    /// Edge QUIC backhaul address.
    #[arg(long)]
    pub edge: SocketAddr,
    /// Fixed TCP target.
    #[arg(long)]
    pub target_tcp: Option<SocketAddr>,
    /// Fixed UDP target; receives opaque datagrams by default.
    #[arg(long)]
    pub target_udp: Option<SocketAddr>,
    /// Prepend PROXY v2 before TCP application bytes (target must support it).
    #[arg(long, requires = "target_tcp")]
    pub proxy_tcp: bool,
    /// Prepend PROXY v2 to every UDP request (target must support it).
    #[arg(long, requires = "target_udp")]
    pub proxy_udp: bool,
    /// Name to verify against the edge's TLS certificate.
    #[arg(long)]
    pub server_name: String,
    #[command(flatten)]
    pub transport: Transport,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Congestion {
    Cubic,
    Bbr,
}

#[derive(Args, Clone)]
pub struct Transport {
    /// PEM CA bundle for peer verification.
    #[arg(long)]
    pub ca: PathBuf,
    /// Local PEM certificate chain, leaf first.
    #[arg(long)]
    pub cert: PathBuf,
    /// Local PEM private key.
    #[arg(long)]
    pub key: PathBuf,
    /// Expected peer's PEM certificate (exact leaf pin, in addition to CA checks).
    #[arg(long)]
    pub peer_cert: PathBuf,
    /// QUIC congestion controller.
    #[arg(long, value_enum, default_value = "bbr")]
    pub congestion: Congestion,
    /// Maximum concurrent TCP connections and, separately, UDP associations.
    #[arg(long, default_value_t = 128, value_parser = clap::value_parser!(u32).range(1..=4096))]
    pub max_connections: u32,
    /// TCP/UDP flow and QUIC connection idle timeout, in seconds.
    #[arg(long, default_value = "30", value_parser = seconds)]
    pub idle_timeout: Duration,
    /// Handshake, stream-open, target-connect and header timeout, in seconds.
    #[arg(long, default_value = "3", value_parser = seconds)]
    pub connect_timeout: Duration,
}

fn seconds(value: &str) -> Result<Duration, String> {
    match value.parse::<u64>() {
        Ok(n @ 1..=86400) => Ok(Duration::from_secs(n)),
        _ => Err("expected seconds in 1..=86400".into()),
    }
}

#[cfg(test)]
mod tests {
    use std::iter;

    use super::*;

    #[test]
    fn cli_contract() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        assert_eq!(Cli::command().get_subcommands().count(), 2);
        let parse = |args: &[&str]| {
            Cli::try_parse_from(iter::once("zerotun").chain(args.iter().copied()).chain([
                "--ca",
                "ca.pem",
                "--cert",
                "cert.pem",
                "--key",
                "key.pem",
                "--peer-cert",
                "peer.pem",
            ]))
        };
        let Command::Edge(defaults) = parse(&["edge", "--listen-udp", "127.0.0.1:8443"])
            .expect("valid UDP-only edge")
            .command
        else {
            panic!("expected edge command");
        };
        assert!(defaults.listen_tcp.is_none(), "TCP ingress is opt-in");
        assert_eq!(
            defaults.bind,
            "127.0.0.1:9443"
                .parse::<SocketAddr>()
                .expect("fixture address")
        );
        let Command::Edge(tcp_only) = parse(&["edge", "--listen-tcp", "127.0.0.1:8443"])
            .expect("valid TCP-only edge")
            .command
        else {
            panic!("expected edge command");
        };
        assert!(tcp_only.listen_udp.is_none(), "UDP ingress is opt-in");
        let congestion = |args: &[&str]| match parse(args).expect("valid relay arguments").command {
            Command::Edge(edge) => edge.transport.congestion,
            Command::Origin(origin) => origin.transport.congestion,
        };
        for mut args in [
            vec!["edge", "--listen-tcp", "127.0.0.1:8443"],
            vec![
                "origin",
                "--edge",
                "[::1]:9443",
                "--target-tcp",
                "127.0.0.1:80",
                "--server-name",
                "edge.test",
            ],
            vec![
                "origin",
                "--edge",
                "[::1]:9443",
                "--target-udp",
                "[::1]:443",
                "--server-name",
                "edge.test",
            ],
        ] {
            if let Command::Origin(origin) = parse(&args).expect("valid origin arguments").command {
                assert!(!origin.proxy_tcp, "TCP PROXY is opt-in");
                assert!(!origin.proxy_udp, "UDP PROXY is opt-in");
                assert_eq!(origin.target_tcp.is_some(), args.contains(&"--target-tcp"));
                assert_eq!(origin.target_udp.is_some(), args.contains(&"--target-udp"));
            }
            assert!(
                matches!(congestion(&args), Congestion::Bbr),
                "BBR is the default for both roles"
            );
            args.extend(["--congestion", "cubic"]);
            assert!(
                matches!(congestion(&args), Congestion::Cubic),
                "explicit CUBIC remains supported"
            );
        }
        let Command::Edge(edge) = parse(&[
            "edge",
            "--listen-tcp",
            "[::1]:8443",
            "--listen-udp",
            "127.0.0.1:8443",
        ])
        .expect("valid TCP/UDP listeners")
        .command
        else {
            panic!("expected edge command");
        };
        assert_eq!(
            edge.listen_tcp,
            Some("[::1]:8443".parse::<SocketAddr>().expect("fixture address"))
        );
        assert_eq!(
            edge.listen_udp,
            Some("127.0.0.1:8443".parse().expect("fixture address"))
        );
        let Command::Origin(origin) = parse(&[
            "origin",
            "--edge",
            "127.0.0.1:9443",
            "--server-name",
            "edge.test",
            "--target-tcp",
            "127.0.0.1:9000",
            "--target-udp",
            "[::1]:9001",
            "--proxy-tcp",
            "--proxy-udp",
        ])
        .expect("valid TCP/UDP targets and PROXY flags")
        .command
        else {
            panic!("expected origin command");
        };
        assert_eq!(
            origin.target_tcp,
            Some(
                "127.0.0.1:9000"
                    .parse::<SocketAddr>()
                    .expect("fixture address")
            )
        );
        assert_eq!(
            origin.target_udp,
            Some("[::1]:9001".parse().expect("fixture address"))
        );
        assert!(origin.proxy_tcp, "TCP PROXY was requested");
        assert!(origin.proxy_udp, "UDP PROXY was requested");
        for invalid in [
            ["--target-tcp", "127.0.0.1:80"],
            ["--max-connections", "0"],
            ["--max-connections", "4097"],
            ["--idle-timeout", "0"],
            ["--connect-timeout", "86401"],
            ["--congestion", "baseline"],
        ] {
            let args: Vec<_> = ["edge", "--listen-tcp", "127.0.0.1:8443"]
                .into_iter()
                .chain(invalid)
                .collect();
            assert!(parse(&args).is_err(), "accepted {args:?}");
        }
        for args in [
            vec!["edge"],
            vec!["edge", "--bind", "127.0.0.1:9443"],
            vec!["origin"],
            vec![
                "origin",
                "--edge",
                "127.0.0.1:9443",
                "--server-name",
                "edge.test",
            ],
            vec![
                "origin",
                "--edge",
                "127.0.0.1:9443",
                "--target-tcp",
                "127.0.0.1:80",
                "--server-name",
                "edge.test",
                "--proxy-udp",
            ],
            vec![
                "origin",
                "--edge",
                "127.0.0.1:9443",
                "--target-udp",
                "127.0.0.1:443",
                "--server-name",
                "edge.test",
                "--proxy-tcp",
            ],
        ] {
            assert!(parse(&args).is_err(), "accepted {args:?}");
        }
        assert!(Cli::try_parse_from(["zerotun", "edge"]).is_err());
    }
}
