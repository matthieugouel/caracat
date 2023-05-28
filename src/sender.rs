//! Send probes on the network.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use log::info;
use pcap::{Active, Capture, Linktype};
use pnet::util::MacAddr;

use crate::builder::{
    build_ethernet, build_icmp, build_icmpv6, build_ipv4, build_ipv6, build_loopback, build_udp,
    Packet,
};
use crate::models::{Probe, L2, L4};
use crate::neighbors::{resolve_mac_address, RoutingTable};
use crate::timestamp::{encode, tenth_ms};
use crate::utilities::{get_ipv4_address, get_ipv6_address, get_mac_address};

pub struct Sender {
    // TODO: Check that we do not allocate more than the C++ version.
    buffer: [u8; 65536],
    dry_run: bool,
    handle: Capture<Active>,
    instance_id: u16,
    l2_protocol: L2,
    src_mac: MacAddr,
    dst_mac: MacAddr,
    src_ip_v4: Ipv4Addr,
    src_ip_v6: Ipv6Addr,
}

impl Sender {
    // TODO: Parameter for gateway resolution address.
    //       Accept gateway MAC address and do resolution upstream?
    pub fn new(interface: &str, instance_id: u16, dry_run: bool) -> Result<Self> {
        let handle = pcap::Capture::from_device(interface)?
            .buffer_size(0)
            .snaplen(0)
            .open()?;

        let l2_protocol = match handle.get_datalink() {
            Linktype::NULL => L2::BSDLoopback,
            Linktype::ETHERNET => L2::Ethernet,
            Linktype(12) => L2::None,
            other => bail!(
                "Unsupported link type: {} ({})",
                other.get_name().unwrap(),
                other.0
            ),
        };

        let src_mac: MacAddr;
        let dst_mac: MacAddr;
        // TODO: dst_mac_{v4,v6}

        if l2_protocol == L2::Ethernet {
            src_mac = get_mac_address(interface).context("Ethernet device has no MAC address")?;
            let table = RoutingTable::from_native()?;
            let route = table
                .get(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 0)))
                .context("No route for 192.0.2.0")?;
            dst_mac = resolve_mac_address(interface, route.gateway)?;
        } else {
            src_mac = MacAddr::zero();
            dst_mac = MacAddr::zero();
        }

        let src_ip_v4 = get_ipv4_address(interface).unwrap_or(Ipv4Addr::UNSPECIFIED);
        let src_ip_v6 = get_ipv6_address(interface).unwrap_or(Ipv6Addr::UNSPECIFIED);

        info!(
            "src_mac={} dst_mac={}",
            src_mac.to_string(),
            dst_mac.to_string()
        );
        info!("src_ip_v4={} src_ip_v6={}", src_ip_v4, src_ip_v6);

        Ok(Sender {
            buffer: [0u8; 65536],
            dry_run,
            handle,
            instance_id,
            l2_protocol,
            src_mac,
            dst_mac,
            src_ip_v4,
            src_ip_v6,
        })
    }

    pub fn send(&mut self, probe: &Probe) -> Result<()> {
        let l3_protocol = probe.l3_protocol();
        let l4_protocol = probe.l4_protocol();

        let timestamp = tenth_ms(SystemTime::now().duration_since(UNIX_EPOCH).unwrap());
        let timestamp_enc = encode(timestamp);

        // TODO: PAYLOAD_TWEAK_BYTES constant
        // TODO: ICMP_HEADER_SIZE constant
        let payload_size = probe.ttl as usize + 2;
        let mut packet = Packet::new(
            &mut self.buffer,
            self.l2_protocol,
            l3_protocol,
            l4_protocol,
            payload_size,
        );
        packet.l2_mut().fill(0);

        match self.l2_protocol {
            L2::BSDLoopback => build_loopback(&mut packet),
            L2::Ethernet => build_ethernet(&mut packet, self.src_mac, self.dst_mac),
            L2::None => {}
        }

        match probe.dst_addr {
            IpAddr::V4(dst_addr) => build_ipv4(
                &mut packet,
                self.src_ip_v4,
                dst_addr,
                probe.ttl,
                probe.checksum(self.instance_id),
            ),
            IpAddr::V6(dst_addr) => build_ipv6(&mut packet, self.src_ip_v6, dst_addr, probe.ttl),
        }

        match l4_protocol {
            L4::ICMP => build_icmp(&mut packet, probe.src_port, timestamp_enc),
            L4::ICMPv6 => build_icmpv6(&mut packet, probe.src_port, timestamp_enc),
            L4::UDP => build_udp(&mut packet, timestamp_enc, probe.src_port, probe.dst_port),
        }

        if !self.dry_run {
            self.handle.sendpacket(packet.l2())?;
        }

        Ok(())
    }
}
