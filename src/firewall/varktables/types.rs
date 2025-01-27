use crate::error::{NetavarkError, NetavarkResult};
use crate::firewall::varktables::helpers::{
    add_chain_unique, append_unique, remove_if_rule_exists,
};
use crate::firewall::varktables::types::TeardownPolicy::{Never, OnComplete};
use crate::network::internal_types::PortForwardConfig;
use crate::network::types::Subnet;
use ipnet::IpNet;
use iptables::IPTables;
use std::net::IpAddr;

//  Chain names
const NAT: &str = "nat";
const FILTER: &str = "filter";
const POSTROUTING: &str = "POSTROUTING";
const PREROUTING: &str = "PREROUTING";
const NETAVARK_FORWARD: &str = "NETAVARK_FORWARD";
const OUTPUT: &str = "OUTPUT";
const FORWARD: &str = "FORWARD";
const ACCEPT: &str = "ACCEPT";
const NETAVARK_HOSTPORT_DNAT: &str = "NETAVARK-HOSTPORT-DNAT";
const NETAVARK_HOSTPORT_SETMARK: &str = "NETAVARK-HOSTPORT-SETMARK";
const NETAVARK_HOSTPORT_MASK: &str = "NETAVARK-HOSTPORT-MASQ";
const MASQUERADE: &str = "MASQUERADE";
const MARK: &str = "MARK";
const DNAT: &str = "DNAT";

const CONTAINER_DN_CHAIN: &str = "NETAVARK-DN-";

const HEXMARK: &str = "0x2000";

const MULTICAST_NET_V4: &str = "224.0.0.0/4";
const MULTICAST_NET_V6: &str = "ff00::/8";

#[derive(PartialEq, Debug, Clone)]
pub enum TeardownPolicy {
    OnComplete,
    Never,
}

#[derive(Clone, Debug)]
pub struct VarkRule {
    // Formatted string of the rule itself
    pub rule: String,
    pub td_policy: Option<TeardownPolicy>,
    pub position: Option<i32>,
}

impl VarkRule {
    fn new(rule: String, policy: Option<TeardownPolicy>) -> VarkRule {
        VarkRule {
            rule,
            td_policy: policy,
            position: None,
        }
    }
    fn to_str(&self) -> &str {
        &self.rule
    }
}
// Varkchain is an iptable chain with extra info
pub struct VarkChain<'a> {
    // name of chain
    pub chain_name: String,
    // should the chain be created by us
    pub create: bool,
    // the connection to iptables, v4 or v6
    pub driver: &'a IPTables,
    // an array of iptables rules to be added to the chain
    pub rules: Vec<VarkRule>,
    // name of table
    pub table: String,
    // if the chain should be removed
    pub td_policy: Option<TeardownPolicy>,
}

impl<'a> VarkChain<'a> {
    fn new(
        driver: &IPTables,
        table: String,
        chain_name: String,
        td_policy: Option<TeardownPolicy>,
    ) -> VarkChain {
        VarkChain {
            driver,
            chain_name,
            table,
            rules: vec![],
            create: false,
            td_policy,
        }
    }

    //  create a queue of rules in a vector
    fn build_rule(&mut self, rule: VarkRule) {
        self.rules.push(rule)
    }

    // actually add the rules to iptables
    pub fn add_rules(&self) -> NetavarkResult<()> {
        // If the chain needs to be created, we make it
        if self.create {
            add_chain_unique(self.driver, &self.table, &self.chain_name)?;
        }
        for rule in &self.rules {
            // If the rule comes with an optional position, then instead of append
            // we should use insert if it does not already exist
            match rule.position {
                None => {
                    append_unique(self.driver, &self.table, &self.chain_name, rule.to_str())?;
                }
                Some(pos) => {
                    let exists = match self
                        .driver
                        .exists(&self.table, &self.chain_name, &rule.rule)
                    {
                        Ok(b) => b,
                        Err(e) => return Err(NetavarkError::Message(e.to_string())),
                    };
                    if !exists {
                        match self
                            .driver
                            .insert(&self.table, &self.chain_name, &rule.rule, pos)
                        {
                            Ok(_) => {}
                            Err(e) => return Err(NetavarkError::Message(e.to_string())),
                        };
                    }
                }
            }
        }
        Ok(())
    }

    //  remove a vector of rules
    pub fn remove_rules(&self, complete_teardown: bool) -> NetavarkResult<()> {
        for rule in &self.rules.clone() {
            // If the rule policy is Never or this is not a
            // complete teardown of the network, then we skip removal
            // of the rule
            match rule.clone().td_policy {
                None => {}
                Some(policy) => {
                    if policy == TeardownPolicy::Never || !complete_teardown {
                        continue;
                    }
                }
            }
            remove_if_rule_exists(self.driver, &self.table, &self.chain_name, rule.to_str())?;
        }
        Ok(())
    }

    // remove the chain itself.
    pub fn remove(&self) -> NetavarkResult<()> {
        // this might be a perf hit but we are going to start this
        // way and think of faster AND logical approach.
        let remaining_rules = match self.driver.list(&self.table, &self.chain_name) {
            Ok(o) => o,
            Err(e) => return Err(NetavarkError::Message(e.to_string())),
        };

        // if for some reason there is a rule left, dont remove the chain and
        // also dont make this a fatal error.  The vec returned by list always
        // reserves [0] for the chain name (-A chain_name), hence the <= 1
        if remaining_rules.len() <= 1 {
            match self.driver.delete_chain(&self.table, &self.chain_name) {
                Ok(_) => {}
                Err(e) => return Err(NetavarkError::Message(e.to_string())),
            };
        }
        Result::Ok(())
    }
}

pub fn get_network_chains(
    conn: &'_ IPTables,
    network: IpNet,
    network_hash_name: String,
    is_ipv6: bool,
) -> Vec<VarkChain<'_>> {
    let mut chains = Vec::new();
    let prefixed_network_hash_name = format!("{}-{}", "NETAVARK", network_hash_name);

    // NETAVARK-HASH
    let mut hashed_network_chain = VarkChain::new(
        conn,
        NAT.to_string(),
        prefixed_network_hash_name.clone(),
        Some(OnComplete),
    );
    hashed_network_chain.create = true;

    hashed_network_chain.build_rule(VarkRule::new(
        format!("-d {} -j {}", network, ACCEPT),
        Some(TeardownPolicy::OnComplete),
    ));

    let mut multicast_dest = MULTICAST_NET_V4;
    if is_ipv6 {
        multicast_dest = MULTICAST_NET_V6;
    }
    hashed_network_chain.build_rule(VarkRule::new(
        format!("! -d {} -j {}", multicast_dest, MASQUERADE),
        Some(TeardownPolicy::OnComplete),
    ));
    chains.push(hashed_network_chain);

    // POSTROUTING
    let mut postrouting_chain =
        VarkChain::new(conn, NAT.to_string(), POSTROUTING.to_string(), None);
    postrouting_chain.build_rule(VarkRule::new(
        format!("-s {} -j {}", network, prefixed_network_hash_name),
        Some(TeardownPolicy::OnComplete),
    ));
    chains.push(postrouting_chain);
    if !is_ipv6 {
        // NETAVARK_FORWARD
        let mut netavark_forward_chain =
            VarkChain::new(conn, FILTER.to_string(), NETAVARK_FORWARD.to_string(), None);
        netavark_forward_chain.create = true;

        // Create incoming traffic rule
        // CNI did this by IP address, this is implemented per subnet
        netavark_forward_chain.build_rule(VarkRule::new(
            format!(
                "-d {} -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT",
                network
            ),
            Some(TeardownPolicy::OnComplete),
        ));

        // Create outgoing traffic rule
        // CNI did this by IP address, this is implemented per subnet
        netavark_forward_chain.build_rule(VarkRule::new(
            format!("-s {} -j ACCEPT", network),
            Some(TeardownPolicy::OnComplete),
        ));
        chains.push(netavark_forward_chain);

        // FORWARD chain
        // Insert the rule into the first position
        let mut forward_chain = VarkChain::new(conn, FILTER.to_string(), FORWARD.to_string(), None);
        forward_chain.build_rule(VarkRule {
            rule: format!(
                "-m comment --comment 'netavark firewall plugin rules' -j {}",
                NETAVARK_FORWARD
            ),
            position: Some(1),
            td_policy: Some(TeardownPolicy::Never),
        });
        chains.push(forward_chain);
    }

    chains
}

pub fn get_port_forwarding_chains<'a>(
    conn: &'a IPTables,
    pfwd: &PortForwardConfig,
    container_ip: &IpAddr,
    network_address: &Subnet,
    is_ipv6: bool,
) -> Vec<VarkChain<'a>> {
    let mut localhost_ip = "127.0.0.1";
    if is_ipv6 {
        localhost_ip = "::1";
    }
    let mut chains = Vec::new();
    // Set up all chains
    let network_dn_chain_name = CONTAINER_DN_CHAIN.to_owned() + &pfwd.network_hash_name;

    let comment_dn_network_cid = format!(
        "-m comment --comment 'dnat name: {} id: {}'",
        pfwd.network_name, pfwd.container_id
    );

    // // NETAVARK-HASH

    // NETAVARK-DN-HASH
    let mut netavark_hashed_dn_chain = VarkChain::new(
        conn,
        NAT.to_string(),
        CONTAINER_DN_CHAIN.to_string() + &pfwd.network_hash_name,
        Some(OnComplete),
    );

    // NETAVARK_HOSTPORT_DNAT
    // Determination to create the chain is done only
    // if there are port mappings
    let mut netavark_hostport_dn_chain = VarkChain::new(
        conn,
        NAT.to_string(),
        NETAVARK_HOSTPORT_DNAT.to_string(),
        None,
    );

    // Setup one-off rules that have nothing to do with ports
    // PREROUTING
    let mut prerouting_chain = VarkChain::new(conn, NAT.to_string(), PREROUTING.to_string(), None);
    prerouting_chain.build_rule(VarkRule::new(
        format!("-j {} -m addrtype --dst-type LOCAL", NETAVARK_HOSTPORT_DNAT),
        Some(TeardownPolicy::Never),
    ));

    //  OUTPUT
    let mut output_chain = VarkChain::new(conn, NAT.to_string(), OUTPUT.to_string(), None);
    output_chain.build_rule(VarkRule::new(
        format!("-j {} -m addrtype --dst-type LOCAL", NETAVARK_HOSTPORT_DNAT),
        Some(TeardownPolicy::Never),
    ));

    // NETAVARK-HOSTPORT-SETMARK
    let mut netavark_hostport_setmark = VarkChain::new(
        conn,
        NAT.to_string(),
        NETAVARK_HOSTPORT_SETMARK.to_string(),
        None,
    );
    netavark_hostport_setmark.create = true;
    netavark_hostport_setmark.build_rule(VarkRule::new(
        format!("-j {}  --set-xmark {}/{}", MARK, HEXMARK, HEXMARK),
        Some(TeardownPolicy::Never),
    ));
    chains.push(netavark_hostport_setmark);

    //  NETAVARK-HOSTPORT-MASQ
    let mut netavark_hostport_masq_chain = VarkChain::new(
        conn,
        NAT.to_string(),
        NETAVARK_HOSTPORT_MASK.to_string(),
        None,
    );
    netavark_hostport_masq_chain.create = true;
    netavark_hostport_masq_chain.build_rule(VarkRule::new(
        format!(
            "-j {} -m comment --comment 'netavark portfw masq mark' -m mark --mark {}/{}",
            MASQUERADE, HEXMARK, HEXMARK
        ),
        Some(TeardownPolicy::Never),
    ));
    netavark_hostport_masq_chain.create = true;
    chains.push(netavark_hostport_masq_chain);

    //  POSTROUTING
    let mut postrouting = VarkChain::new(conn, NAT.to_string(), POSTROUTING.to_string(), None);
    // This rule must be in the first position
    postrouting.build_rule(VarkRule {
        rule: format!("-j {} ", NETAVARK_HOSTPORT_MASK),
        position: Some(1),
        td_policy: Some(Never),
    });

    chains.push(postrouting);

    //  Determine if we need to create chains
    if !pfwd.port_mappings.is_empty() {
        netavark_hostport_dn_chain.create = true;
        netavark_hashed_dn_chain.create = true;
    }

    for i in pfwd.port_mappings.clone() {
        if let Ok(ip) = i.host_ip.parse::<IpAddr>() {
            match ip {
                IpAddr::V4(_) => {
                    if is_ipv6 {
                        continue;
                    }
                }
                IpAddr::V6(_) => {
                    if !is_ipv6 {
                        continue;
                    }
                }
            }
        }

        // hostport dnat
        let is_range = i.range > 1;
        let mut host_port = i.host_port.to_string();
        if is_range {
            host_port = format!("{}:{}", i.host_port, (i.host_port + (i.range - 1)))
        }
        netavark_hostport_dn_chain.build_rule(VarkRule::new(
            format!(
                // I'm leaving this commented code for now in the case
                // we need to revert.
                // "-j {} -p {} -m multiport --destination-ports {} {}",
                "-j {} -p {} --dport {} {}",
                network_dn_chain_name, i.protocol, &host_port, comment_dn_network_cid
            ),
            None,
        ));

        let mut dn_setmark_rule_localhost = format!(
            "-j {} -s {} -p {} --dport {}",
            NETAVARK_HOSTPORT_SETMARK, network_address.subnet, i.protocol, &host_port
        );

        let mut dn_setmark_rule_subnet = format!(
            "-j {} -s {} -p {} --dport {}",
            NETAVARK_HOSTPORT_SETMARK, localhost_ip, i.protocol, &host_port
        );

        // if a destination ip address is provided, we need to alter
        // the rule a bit
        if !i.host_ip.is_empty() {
            dn_setmark_rule_localhost = format!("{} -d {}", dn_setmark_rule_localhost, i.host_ip);
            dn_setmark_rule_subnet = format!("{} -d {}", dn_setmark_rule_subnet, i.host_ip);
        }

        // dn container (the actual port usages)
        netavark_hashed_dn_chain.build_rule(VarkRule::new(dn_setmark_rule_localhost, None));

        netavark_hashed_dn_chain.build_rule(VarkRule::new(dn_setmark_rule_subnet, None));

        let mut container_ip_value = container_ip.to_string();
        if is_ipv6 {
            container_ip_value = format!("[{}]", container_ip_value)
        }
        let mut container_port = i.container_port.to_string();
        if is_range {
            container_port = format!(
                "{}-{}/{}",
                i.container_port,
                (i.container_port + (i.range - 1)),
                i.host_port
            );
        }
        let mut dnat_rule = format!(
            "-j {} -p {} --to-destination {}:{} --destination-port {}",
            DNAT, i.protocol, container_ip_value, container_port, &host_port
        );

        // if a destination ip address is provided, we need to alter
        // the rule a bit
        if !i.host_ip.is_empty() {
            dnat_rule = format!("{} -d {}", dnat_rule, i.host_ip)
        }
        netavark_hashed_dn_chain.build_rule(VarkRule::new(dnat_rule, None));
    }

    //  The order is important here.  Be certain before changing it
    chains.push(netavark_hashed_dn_chain);
    chains.push(netavark_hostport_dn_chain);
    chains.push(prerouting_chain);
    chains.push(output_chain);

    chains
}
