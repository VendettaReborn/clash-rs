use ipnetwork::Ipv4Network;
use nix::sched::{CloneFlags, setns};
use std::{error::Error, fs::File, path::Path, process::Command};

#[derive(Debug)]
pub struct NetnsBuilder {
    name: String,
    /// name for outer veth pair
    veth_outer: Option<String>,
    /// name for inner veth pair
    veth_inner: Option<String>,
    // ipv4 addr for veth_outside
    ipv4: core::net::Ipv4Addr,
    ipv4_prefix: u8,
    cleanup: bool,
}

impl Default for NetnsBuilder {
    fn default() -> Self {
        Self {
            name: "tmpns".into(),
            veth_outer: Some("veth-o".into()),
            veth_inner: Some("veth-i".into()),
            ipv4: core::net::Ipv4Addr::new(19, 89, 6, 4),
            ipv4_prefix: 24,
            cleanup: true,
        }
    }
}

#[allow(unused)]
impl NetnsBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn name(mut self, name: &str) -> Self {
        self.name = name.to_string();
        self
    }

    pub fn veth_outside(mut self, veth_outside: &str) -> Self {
        self.veth_outer = Some(veth_outside.to_string());
        self
    }

    pub fn veth_inside(mut self, veth_inside: &str) -> Self {
        self.veth_inner = Some(veth_inside.to_string());
        self
    }

    pub fn ipv4(mut self, ipv4: core::net::Ipv4Addr) -> Self {
        self.ipv4 = ipv4;
        self
    }

    pub fn ipv4_mask(mut self, ipv4_mask: u8) -> Self {
        self.ipv4_prefix = ipv4_mask;
        self
    }

    pub fn cleanup(mut self, cleanup: bool) -> Self {
        self.cleanup = cleanup;
        self
    }

    pub fn build(self) -> NetnsGuard {
        NetnsGuard::new(
            &self.name,
            (self.veth_outer.unwrap(), self.veth_inner.unwrap()),
            (self.ipv4, self.ipv4_prefix),
            self.cleanup,
        )
        .unwrap()
    }
}

pub struct NetnsGuard {
    name: String,
    #[allow(unused)]
    veth_outer: String,
    ipv4_network: Ipv4Network,
    cleanup: bool,
}

impl NetnsGuard {
    fn new(
        name: &str,
        veth: (String, String),
        ipv4: (core::net::Ipv4Addr, u8),
        cleanup: bool,
    ) -> Result<Self, Box<dyn Error>> {
        let (veth_outer, veth_inner) = veth;
        let ipv4_network = Ipv4Network::new(ipv4.0, ipv4.1)?;
        let prefix = ipv4_network.prefix();

        let mut iter = ipv4_network.iter();
        let veth_outer_addr = iter.next().unwrap();
        let veth_inner_addr = iter.next().unwrap();

        Command::new("ip").args(["netns", "add", name]).output()?;
        Command::new("ip")
            .args([
                "link",
                "add",
                &veth_inner,
                "type",
                "veth",
                "peer",
                "name",
                &veth_outer,
            ])
            .output()?;
        Command::new("ip")
            .args(["link", "set", &veth_inner, "netns", name])
            .output()?;
        Command::new("ip")
            .args([
                "addr",
                "add",
                &format!("{:?}/{}", veth_outer_addr, prefix),
                "dev",
                &veth_outer,
            ])
            .output()?;
        Command::new("ip")
            .args(["link", "set", &veth_outer, "up"])
            .output()?;
        Command::new("ip")
            .args([
                "netns",
                "exec",
                name,
                "ip",
                "addr",
                "add",
                &format!("{:?}/{}", veth_inner_addr, prefix),
                "dev",
                &veth_inner,
            ])
            .output()?;
        Command::new("ip")
            .args([
                "netns",
                "exec",
                name,
                "ip",
                "link",
                "set",
                &veth_inner,
                "up",
            ])
            .output()?;
        Command::new("ip")
            .args(["netns", "exec", name, "ip", "link", "set", "lo", "up"])
            .output()?;
        Command::new("ip")
            .args([
                "netns",
                "exec",
                name,
                "ip",
                "route",
                "add",
                "default",
                "via",
                &veth_outer_addr.to_string(),
            ])
            .output()?;
        Command::new("sysctl")
            .args(["-w", "net.ipv4.ip_forward=1"])
            .output()?;
        Command::new("iptables")
            .args([
                "-t",
                "nat",
                "-A",
                "POSTROUTING",
                "-s",
                &format!("{}/{}", ipv4_network.network(), ipv4_network.prefix()),
                "-j",
                "MASQUERADE",
            ])
            .output()?;

        // Set network namespace
        let path = Path::new("/var/run/netns").join(name);
        let netns = File::open(path)?;
        setns(netns, CloneFlags::CLONE_NEWNET)?;

        Ok(Self {
            name: name.to_string(),
            veth_outer,
            ipv4_network,
            cleanup,
        })
    }
}

impl Drop for NetnsGuard {
    fn drop(&mut self) {
        if !self.cleanup {
            return;
        }
        // will automatically delete the veth pair
        let _ = Command::new("ip")
            .args(["netns", "delete", &self.name])
            .output();
        let _ = Command::new("iptables")
            .args([
                "-t",
                "nat",
                "-D",
                "POSTROUTING",
                "-s",
                &format!("{}", self.ipv4_network),
                "-j",
                "MASQUERADE",
            ])
            .output();
    }
}
