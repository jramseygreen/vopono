use super::netns::NetworkNamespace;
use super::util::check_process_running;
use super::vpn::{Firewall, OpenVpnProtocol};
use anyhow::{anyhow, Context};
use log::{debug, error, info};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Serialize, Deserialize)]
pub struct OpenVpn {
    pid: u32,
}

impl OpenVpn {
    #[allow(clippy::too_many_arguments)]
    pub fn run(
        netns: &NetworkNamespace,
        config_file: PathBuf,
        auth_file: Option<PathBuf>,
        dns: &[IpAddr],
        use_killswitch: bool,
        forward_ports: Option<&Vec<u16>>,
        firewall: Firewall,
        disable_ipv6: bool,
    ) -> anyhow::Result<Self> {
        // TODO: Refactor this to separate functions
        // TODO: --status flag

        if let Err(x) = which::which("openvpn") {
            error!("OpenVPN not found. Is OpenVPN installed and on PATH?");
            return Err(anyhow!(
                "OpenVPN not found. Is OpenVPN installed and on PATH?: {:?}",
                x
            ));
        }

        let handle;
        let log_file_str = format!("/etc/netns/{}/openvpn.log", &netns.name);
        {
            File::create(&log_file_str)?;
        }

        let config_file_path = config_file.canonicalize().context("Invalid path given")?;

        info!("Launching OpenVPN...");
        let mut command_vec = (&[
            "openvpn",
            "--config",
            config_file_path.to_str().unwrap(),
            "--machine-readable-output",
            "--log",
            log_file_str.as_str(),
        ])
            .to_vec();

        if auth_file.is_some() {
            command_vec.push("--auth-user-pass");
            command_vec.push(auth_file.as_ref().unwrap().as_os_str().to_str().unwrap());
        }

        let remotes = get_remotes_from_config(&config_file)?;
        debug!("Found remotes: {:?}", &remotes);
        let working_dir = PathBuf::from(config_file_path.parent().unwrap());

        handle = netns
            .exec_no_block(&command_vec, None, true, Some(working_dir))
            .context("Failed to launch OpenVPN - is openvpn installed?")?;
        let id = handle.id();
        let mut buffer = String::with_capacity(1024);

        let mut logfile = BufReader::with_capacity(64, File::open(log_file_str)?);
        let mut pos: usize = 0;

        // Tail OpenVPN log file
        loop {
            let x = logfile.read_line(&mut buffer)?;
            pos += x;

            if x > 0 {
                debug!("{:?}", buffer);
            }

            if buffer.contains("Initialization Sequence Completed")
                || buffer.contains("AUTH_FAILED")
                || buffer.contains("Options error")
            {
                break;
            }

            logfile.seek(SeekFrom::Start(pos as u64)).unwrap();
            buffer.clear();
        }

        if buffer.contains("AUTH_FAILED") {
            if auth_file.is_some() {
                error!(
                    "OpenVPN authentication failed, deleting {}",
                    auth_file.as_ref().unwrap().display()
                );
                std::fs::remove_file(auth_file.unwrap())?;
            }
            return Err(anyhow!(
                "OpenVPN authentication failed, use -v for full log output"
            ));
        }
        if buffer.contains("Options error") {
            error!("OpenVPN options error: {}", buffer);
            return Err(anyhow!("OpenVPN options error, use -v for full log output"));
        }

        // Allow input to and output from forwarded ports
        if let Some(forwards) = forward_ports {
            super::util::open_ports(&netns, forwards.as_slice(), firewall)?;
        }

        if use_killswitch {
            killswitch(netns, dns, remotes.as_slice(), firewall, disable_ipv6)?;
        }

        Ok(Self { pid: id })
    }

    pub fn check_if_running(&self) -> bool {
        check_process_running(self.pid)
    }
}

impl Drop for OpenVpn {
    fn drop(&mut self) {
        match nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(self.pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        ) {
            Ok(_) => debug!("Killed OpenVPN (pid: {})", self.pid),
            Err(e) => error!("Failed to kill OpenVPN (pid: {}): {:?}", self.pid, e),
        }
    }
}

pub fn killswitch(
    netns: &NetworkNamespace,
    dns: &[IpAddr],
    remotes: &[Remote],
    firewall: Firewall,
    disable_ipv6: bool,
) -> anyhow::Result<()> {
    debug!("Setting OpenVPN killswitch....");

    match firewall {
        Firewall::IpTables => {
            let ipcmds = if disable_ipv6 {
                netns.exec(&["ip6tables", "-P", "INPUT", "DROP"])?;
                netns.exec(&["ip6tables", "-P", "FORWARD", "DROP"])?;
                netns.exec(&["ip6tables", "-P", "OUTPUT", "DROP"])?;
                vec!["iptables"]
            } else {
                vec!["iptables", "ip6tables"]
            };

            for ipcmd in ipcmds {
                netns.exec(&[ipcmd, "-P", "INPUT", "DROP"])?;
                netns.exec(&[ipcmd, "-P", "FORWARD", "DROP"])?;
                netns.exec(&[ipcmd, "-P", "OUTPUT", "DROP"])?;
                netns.exec(&[
                    ipcmd,
                    "-A",
                    "INPUT",
                    "-m",
                    "conntrack",
                    "--ctstate",
                    "RELATED,ESTABLISHED",
                    "-j",
                    "ACCEPT",
                ])?;
                netns.exec(&[ipcmd, "-A", "INPUT", "-i", "lo", "-j", "ACCEPT"])?;
                netns.exec(&[ipcmd, "-A", "INPUT", "-i", "tun+", "-j", "ACCEPT"])?;
                netns.exec(&[ipcmd, "-A", "OUTPUT", "-o", "lo", "-j", "ACCEPT"])?;
                for dnsa in dns.iter() {
                    match dnsa {
                        // TODO: Tidy this up
                        IpAddr::V4(addr) => {
                            if ipcmd == "iptables" {
                                netns.exec(&[
                                    ipcmd,
                                    "-A",
                                    "OUTPUT",
                                    "-d",
                                    &addr.to_string(),
                                    "-j",
                                    "ACCEPT",
                                ])?;
                            }
                        }
                        IpAddr::V6(addr) => {
                            if ipcmd == "ip6tables" && !disable_ipv6 {
                                netns.exec(&[
                                    ipcmd,
                                    "-A",
                                    "OUTPUT",
                                    "-d",
                                    &addr.to_string(),
                                    "-j",
                                    "ACCEPT",
                                ])?;
                            }
                        }
                    }
                }

                // TODO: Tidy this up - remote can be IPv4 or IPv6 address or hostname
                for remote in remotes {
                    let port_str = format!("{}", remote.port);
                    match &remote.host {
                        // TODO: Fix this to specify destination address - but need hostname
                        // resolution working
                        Host::IPv4(ip) => {
                            if ipcmd == "iptables" {
                                netns.exec(&[
                                    ipcmd,
                                    "-A",
                                    "OUTPUT",
                                    "-p",
                                    &remote.protocol.to_string(),
                                    "-m",
                                    &remote.protocol.to_string(),
                                    // "-d",
                                    // &ip.to_string(),
                                    "--dport",
                                    port_str.as_str(),
                                    "-j",
                                    "ACCEPT",
                                ])?;
                            }
                        }
                        Host::IPv6(ip) => {
                            if ipcmd == "ip6tables" {
                                netns.exec(&[
                                    ipcmd,
                                    "-A",
                                    "OUTPUT",
                                    "-p",
                                    &remote.protocol.to_string(),
                                    "-m",
                                    &remote.protocol.to_string(),
                                    // "-d",
                                    // &ip.to_string(),
                                    "--dport",
                                    port_str.as_str(),
                                    "-j",
                                    "ACCEPT",
                                ])?;
                            }
                        }
                        Host::Hostname(name) => {
                            netns.exec(&[
                                ipcmd,
                                "-A",
                                "OUTPUT",
                                "-p",
                                &remote.protocol.to_string(),
                                // "-d",
                                // &name.to_string(),
                                "-m",
                                &remote.protocol.to_string(),
                                "--dport",
                                port_str.as_str(),
                                "-j",
                                "ACCEPT",
                            ])?;
                        }
                    }
                }

                netns.exec(&[ipcmd, "-A", "OUTPUT", "-o", "tun+", "-j", "ACCEPT"])?;
                netns.exec(&[
                    ipcmd,
                    "-A",
                    "OUTPUT",
                    "-j",
                    "REJECT",
                    "--reject-with",
                    "icmp-net-unreachable",
                ])?;
            }
        }
        Firewall::NfTables => {
            if disable_ipv6 {
                netns.exec(&["nft", "add", "table", "ip6", &netns.name])?;
                netns.exec(&[
                    "nft",
                    "add",
                    "chain",
                    "ip6",
                    &netns.name,
                    "drop_ipv6_input",
                    "{ type filter hook input priority -1 ; policy drop; }",
                ])?;
                netns.exec(&[
                    "nft",
                    "add",
                    "chain",
                    "ip6",
                    &netns.name,
                    "drop_ipv6_output",
                    "{ type filter hook output priority -1 ; policy drop; }",
                ])?;
                netns.exec(&[
                    "nft",
                    "add",
                    "chain",
                    "ip6",
                    &netns.name,
                    "drop_ipv6_forward",
                    "{ type filter hook forward priority -1 ; policy drop; }",
                ])?;
            }
            // TODO:
            netns.exec(&["nft", "add", "table", "inet", &netns.name])?;
            netns.exec(&[
                "nft",
                "add",
                "chain",
                "inet",
                &netns.name,
                "input",
                "{ type filter hook input priority 100 ; policy drop; }",
            ])?;
            netns.exec(&[
                "nft",
                "add",
                "chain",
                "inet",
                &netns.name,
                "forward",
                "{ type filter hook forward priority 100 ; policy drop; }",
            ])?;
            netns.exec(&[
                "nft",
                "add",
                "chain",
                "inet",
                &netns.name,
                "output",
                "{ type filter hook output priority 100 ; policy drop; }",
            ])?;
            netns.exec(&[
                "nft",
                "add",
                "rule",
                "inet",
                &netns.name,
                "input",
                "ct",
                "state",
                "related,established",
                "counter",
                "accept",
            ])?;
            netns.exec(&[
                "nft",
                "add",
                "rule",
                "inet",
                &netns.name,
                "input",
                "iifname",
                "\"lo\"",
                "counter",
                "accept",
            ])?;
            netns.exec(&[
                "nft",
                "add",
                "rule",
                "inet",
                &netns.name,
                "input",
                "iifname",
                "\"tun*\"",
                "counter",
                "accept",
            ])?;
            netns.exec(&[
                "nft",
                "add",
                "rule",
                "inet",
                &netns.name,
                "output",
                "oifname",
                "\"lo\"",
                "counter",
                "accept",
            ])?;
            for dnsa in dns.iter() {
                match dnsa {
                    IpAddr::V4(addr) => {
                        netns.exec(&[
                            "nft",
                            "add",
                            "rule",
                            "inet",
                            &netns.name,
                            "output",
                            "ip",
                            "daddr",
                            &addr.to_string(),
                            "counter",
                            "accept",
                        ])?;
                    }
                    IpAddr::V6(addr) => {
                        netns.exec(&[
                            "nft",
                            "add",
                            "rule",
                            "inet",
                            &netns.name,
                            "output",
                            "ip6",
                            "daddr",
                            &addr.to_string(),
                            "counter",
                            "accept",
                        ])?;
                    }
                };
            }

            for remote in remotes {
                let port_str = format!("{}", remote.port);
                match &remote.host {
                    // TODO: Fix this to specify destination address - but need hostname
                    // resolution working
                    Host::IPv4(ip) => {
                        netns.exec(&[
                            "nft",
                            "add",
                            "rule",
                            "inet",
                            &netns.name,
                            "output",
                            // "ip",
                            // "daddr",
                            // &ip.to_string(),
                            &remote.protocol.to_string(),
                            "dport",
                            port_str.as_str(),
                            "counter",
                            "accept",
                        ])?;
                    }
                    Host::IPv6(ip) => {
                        netns.exec(&[
                            "nft",
                            "add",
                            "rule",
                            "inet",
                            &netns.name,
                            "output",
                            // "ip6",
                            // "daddr",
                            // &ip.to_string(),
                            &remote.protocol.to_string(),
                            "dport",
                            port_str.as_str(),
                            "counter",
                            "accept",
                        ])?;
                    }
                    Host::Hostname(name) => {
                        // TODO: Does this work with nftables?
                        netns.exec(&[
                            "nft",
                            "add",
                            "rule",
                            "inet",
                            &netns.name,
                            "output",
                            // "ip",
                            // "daddr",
                            // &name.to_string(),
                            &remote.protocol.to_string(),
                            "dport",
                            port_str.as_str(),
                            "counter",
                            "accept",
                        ])?;
                    }
                }
            }

            netns.exec(&[
                "nft",
                "add",
                "rule",
                "inet",
                &netns.name,
                "output",
                "oifname",
                "\"tun*\"",
                "counter",
                "accept",
            ])?;

            netns.exec(&[
                "nft",
                "add",
                "rule",
                "inet",
                &netns.name,
                "output",
                "counter",
                "reject",
                "with",
                "icmp",
                "type",
                "net-unreachable",
            ])?;
        }
    }
    Ok(())
}

pub fn get_remotes_from_config(path: &PathBuf) -> anyhow::Result<Vec<Remote>> {
    let file_string = std::fs::read_to_string(path)?;
    let mut output_vec = Vec::new();
    // Regex extract
    let re = Regex::new(r"remote ([^\s]+) ([0-9]+)\s?(tcp|udp|tcp-client)?")?;
    let caps = re.captures_iter(&file_string);

    let re2 = Regex::new(r"proto ([a-z\-]+)")?;
    let mut caps2 = re2.captures_iter(&file_string);
    let default_proto = caps2.next().map(|x| x.get(1)).flatten();

    for cap in caps {
        let proto = match (cap.get(3), default_proto) {
            (None, None) => {
                return Err(anyhow!(
                    "No protocol given in OpenVPN config: {}",
                    path.display()
                ));
            }
            (Some(x), _) => OpenVpnProtocol::from_str(x.as_str()),
            (None, Some(x)) => OpenVpnProtocol::from_str(x.as_str()),
        }?;

        output_vec.push(Remote {
            host: Host::from_str(cap.get(1).unwrap().as_str()).expect("Could not convert hostname"),
            port: cap.get(2).unwrap().as_str().parse::<u16>()?,
            protocol: proto,
        });
    }

    if output_vec.is_empty() {
        return Err(anyhow!(
            "Failed to extract remotes from config file: {}",
            &path.display()
        ));
    }
    Ok(output_vec)
}

#[derive(Debug)]
pub struct Remote {
    host: Host,
    pub port: u16,
    protocol: OpenVpnProtocol,
}

#[derive(Debug)]
pub enum Host {
    IPv4(std::net::Ipv4Addr),
    IPv6(std::net::Ipv6Addr),
    Hostname(String),
}

impl FromStr for Host {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(x) = s.parse() {
            Ok(Host::IPv4(x))
        } else if let Ok(x) = s.parse() {
            Ok(Host::IPv6(x))
        } else {
            Ok(Host::Hostname(s.to_string()))
        }
    }
}
