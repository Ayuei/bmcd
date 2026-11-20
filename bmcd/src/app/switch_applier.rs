// Copyright 2026 Oleg Tsarev
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Putting a document on the switch, and reading one back off it.
//!
//! The kernel's bridge is the interface to the switch: the DSA driver turns
//! `bridge vlan add` into register writes on the RTL8370MB-CG. So this module
//! builds `bridge` and `ip` command lines, and nothing here knows anything
//! about the hardware beyond the port names.
//!
//! ## Why the commands are built first and run second
//!
//! [`plan`] turns a document into the exact list of commands, and is a pure
//! function with tests. [`Applier::apply`] runs that list. Keeping them apart
//! means the thing most likely to be wrong -- the order and the flags -- can
//! be checked without a switch, and the thing that cannot be tested without
//! hardware is reduced to "run these, stop on the first failure".
//!
//! ## Order matters, and the order is not obvious
//!
//! Filtering goes on BEFORE the VLANs are written and off AFTER they are
//! cleared, because a bridge with filtering on and no VLANs configured
//! forwards nothing at all. Doing it the other way round takes the board off
//! the network in the middle of its own reconfiguration -- which the confirm
//! window would recover, but only after making an operator watch it happen.

use crate::app::switch_document::{
    infer_second_uplink, PortConfig, PortId, SecondUplink, SwitchDocument,
};
use std::collections::{BTreeMap, BTreeSet};

/// The kernel's name for each front-panel / node port, as the device tree
/// spells it.
///
/// The BMC's own port has no netdev, so it is absent here. Its VLAN membership
/// is programmed on the bridge with the `self` flag — see [`vlan_target`].
pub fn interface_name(port: PortId) -> Option<&'static str> {
    match port {
        PortId::Node1 => Some("node1"),
        PortId::Node2 => Some("node2"),
        PortId::Node3 => Some("node3"),
        PortId::Node4 => Some("node4"),
        PortId::Ge0 => Some("ge0"),
        PortId::Ge1 => Some("ge1"),
        PortId::Bond0 => Some("bond0"),
        PortId::Bmc => None,
    }
}

/// The ports that have a netdev, in the order their VLANs are written: every
/// physical port, plus `bond0` when either document has it in its table.
fn front_ports(running: &SwitchDocument, wanted: &SwitchDocument) -> Vec<PortId> {
    let mut ports: Vec<PortId> = PortId::ALL
        .into_iter()
        .filter(|p| interface_name(*p).is_some())
        .collect();
    if running.ports.contains_key(&PortId::Bond0) || wanted.ports.contains_key(&PortId::Bond0) {
        ports.push(PortId::Bond0);
    }
    ports
}

pub const BOND: &str = "bond0";
pub const BOND_MODE: &str = "802.3ad";
pub const BOND_MIIMON: &str = "100";
/// The RTL8370 trunk copies this at `port_lag_join`: `layer2+3` adds SIP/DIP
/// to the ASIC hash, so bridged node flows spread across ge0/ge1. The kernel
/// default `layer2` leaves the IPs out.
pub const BOND_XMIT_HASH_POLICY: &str = "layer2+3";
pub const BOND_LACP_RATE: &str = "fast";

/// The bridge every switch port is enslaved to.
pub const BRIDGE: &str = "br0";

/// Where a port's VLAN membership is written: netdev name, and whether the
/// command needs the bridge `self` flag.
///
/// Front-panel ports are their own devices. The BMC is the CPU port: there is
/// no `bmc` netdev, so membership is `bridge vlan … dev br0 … self`.
fn vlan_target(port: PortId) -> (&'static str, bool) {
    match interface_name(port) {
        Some(name) => (name, false),
        None => (BRIDGE, true),
    }
}

/// One command, as argv. A string would have to be parsed by a shell, and a
/// shell is not something this needs between it and the kernel.
pub type Command = Vec<String>;

fn cmd(parts: &[&str]) -> Command {
    parts.iter().map(|part| part.to_string()).collect()
}

fn vlan_del(dev: &str, vid: u16, bridge_self: bool) -> Command {
    // `self` last: that is how `bridge` marks the bridge's own VLAN entry.
    let mut out = vec![
        "bridge".into(),
        "vlan".into(),
        "del".into(),
        "dev".into(),
        dev.into(),
        "vid".into(),
        vid.to_string(),
    ];
    if bridge_self {
        out.push("self".into());
    }
    out
}

fn vlan_add(dev: &str, vid: u16, pvid_untagged: bool, bridge_self: bool) -> Command {
    let mut out = vec![
        "bridge".into(),
        "vlan".into(),
        "add".into(),
        "dev".into(),
        dev.into(),
        "vid".into(),
        vid.to_string(),
    ];
    if pvid_untagged {
        out.push("pvid".into());
        out.push("untagged".into());
    }
    if bridge_self {
        out.push("self".into());
    }
    out
}

/// The `bridge vlan del` lines that take `port` out of the VLANs it is leaving.
fn removals(port: PortId, running: &SwitchDocument, wanted: &SwitchDocument) -> Vec<Command> {
    let (dev, bridge_self) = vlan_target(port);
    let was = running.ports.get(&port).cloned().unwrap_or_default();
    let now = wanted.ports.get(&port).cloned().unwrap_or_default();
    was.vlans()
        .difference(&now.vlans())
        .map(|vid| vlan_del(dev, *vid, bridge_self))
        .collect()
}

/// The `bridge vlan add` lines that put `port` into the VLANs it is joining.
///
/// `was` is what the kernel will hold when these run, which after a bond
/// change is not what the running document says: see [`effective_was`].
fn additions(
    port: PortId,
    running: &SwitchDocument,
    wanted: &SwitchDocument,
    was_lag: bool,
    now_lag: bool,
) -> Vec<Command> {
    let (dev, bridge_self) = vlan_target(port);
    let was = effective_was(port, running, was_lag, now_lag);
    let now = wanted.ports.get(&port).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for vid in &now.tagged {
        if !was.tagged.contains(vid) {
            out.push(vlan_add(dev, *vid, false, bridge_self));
        }
    }
    if let Some(vid) = now.untagged {
        if was.untagged != Some(vid) {
            out.push(vlan_add(dev, vid, true, bridge_self));
        }
    }
    out
}

/// True for a command whose failure does not stop the plan: deleting the
/// bridge's own (`self`) VLAN entry.
///
/// What a delete wants is "the entry is gone", and a board whose bridge never
/// had it (a hand-written `interfaces` file, say) already is. The BMC's new
/// VLAN was added before this runs, so a delete that fails for any other
/// reason leaves the BMC in one VLAN too many, which is harmless, whereas
/// stopping here would leave the front ports half-moved with no rollback.
pub fn is_best_effort(command: &Command) -> bool {
    matches!(
        command
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice(),
        ["bridge", "vlan", "del", "dev", BRIDGE, "vid", _, "self"]
    )
}

/// Every command needed to move the switch from `running` to `wanted`.
///
/// Computed as a whole rather than emitted as the document is walked, because
/// the order is the part that matters and an order is not something you can
/// get right one line at a time.
pub fn plan(running: &SwitchDocument, wanted: &SwitchDocument) -> Vec<Command> {
    let mut commands = Vec::new();

    let turning_on = wanted.vlan_filtering && !running.vlan_filtering;
    let turning_off = !wanted.vlan_filtering && running.vlan_filtering;
    let was_lag = running.second_uplink == SecondUplink::Lag;
    let now_lag = wanted.second_uplink == SecondUplink::Lag;

    // ON FIRST. A bridge with filtering on and no VLANs yet forwards nothing,
    // so the VLANs have to exist before it starts caring about them... and
    // yet the kernel will not accept a VLAN on a bridge that is not filtering.
    // The resolution is that `bridge vlan add` works either way, and the
    // window between the two is the whole point of enabling filtering last.
    if turning_off {
        commands.push(cmd(&[
            "ip",
            "link",
            "set",
            "dev",
            BRIDGE,
            "type",
            "bridge",
            "vlan_filtering",
            "0",
        ]));
    }

    // The BMC first, and its own order inside: join the new VLAN, THEN leave
    // the old one. The BMC is what the board is managed through; being in two
    // VLANs on the bridge itself is harmless for a moment, being in none is
    // not, and a plan that stops part-way is not rolled back. A failed delete
    // here leaves the BMC in one VLAN too many (see [`is_best_effort`]).
    for port in PortId::ALL
        .into_iter()
        .filter(|p| interface_name(*p).is_none())
    {
        commands.extend(additions(port, running, wanted, was_lag, now_lag));
        commands.extend(removals(port, running, wanted));
    }

    // Then the front ports: remove what is no longer wanted, before adding, so
    // a port moving from one untagged VLAN to another is never briefly in both.
    let front = front_ports(running, wanted);
    for port in &front {
        commands.extend(removals(*port, running, wanted));
    }

    // Bond create/destroy sits between the removals and the additions, so
    // ge0/ge1 are not briefly both bridged and bonded with the wrong VIDs.
    // A port that joins the bridge gets the kernel's default_pvid 1; clear
    // that before the additions when the document does not want VID 1.
    if was_lag && !now_lag {
        commands.extend(teardown_lag());
        plan_kernel_pvid_clear(&mut commands, wanted, &[PortId::Ge0, PortId::Ge1]);
    } else if !was_lag && now_lag {
        commands.extend(setup_lag());
        plan_kernel_pvid_clear(&mut commands, wanted, &[PortId::Bond0]);
    }

    for port in &front {
        commands.extend(additions(*port, running, wanted, was_lag, now_lag));
    }

    if turning_on {
        commands.push(cmd(&[
            "ip",
            "link",
            "set",
            "dev",
            BRIDGE,
            "type",
            "bridge",
            "vlan_filtering",
            "1",
        ]));
    }

    if wanted.stp != running.stp {
        commands.push(cmd(&[
            "ip",
            "link",
            "set",
            "dev",
            BRIDGE,
            "type",
            "bridge",
            "stp_state",
            if wanted.stp { "1" } else { "0" },
        ]));
    }

    commands
}

/// Drop the kernel `default_pvid` left on a port that just joined `br0`.
fn plan_kernel_pvid_clear(commands: &mut Vec<Command>, wanted: &SwitchDocument, ports: &[PortId]) {
    for &port in ports {
        let now = wanted.ports.get(&port).cloned().unwrap_or_default();
        if now.vlans().contains(&1) {
            continue;
        }
        let (dev, bridge_self) = vlan_target(port);
        commands.push(vlan_del(dev, 1, bridge_self));
    }
}

/// Previous VLAN membership as the kernel will have it when add commands run.
///
/// After `setup_lag` / `teardown_lag`, a newly enslaved port carries
/// `default_pvid` 1 even if the prior document listed no VLANs for it.
fn effective_was(
    port: PortId,
    running: &SwitchDocument,
    was_lag: bool,
    now_lag: bool,
) -> PortConfig {
    let raw = running.ports.get(&port).cloned().unwrap_or_default();
    let just_joined_bridge = match port {
        PortId::Bond0 => !was_lag && now_lag,
        PortId::Ge0 | PortId::Ge1 => was_lag && !now_lag,
        _ => false,
    };
    if just_joined_bridge {
        PortConfig::untagged(1)
    } else {
        raw
    }
}

pub fn setup_lag() -> Vec<Command> {
    vec![
        cmd(&[
            "ip",
            "link",
            "add",
            BOND,
            "type",
            "bond",
            "mode",
            BOND_MODE,
            "miimon",
            BOND_MIIMON,
            "xmit_hash_policy",
            BOND_XMIT_HASH_POLICY,
            "lacp_rate",
            BOND_LACP_RATE,
        ]),
        cmd(&["ip", "link", "set", "dev", "ge0", "nomaster"]),
        cmd(&["ip", "link", "set", "dev", "ge1", "nomaster"]),
        cmd(&["ip", "link", "set", "dev", "ge0", "down"]),
        cmd(&["ip", "link", "set", "dev", "ge1", "down"]),
        cmd(&["ip", "link", "set", "dev", "ge0", "master", BOND]),
        cmd(&["ip", "link", "set", "dev", "ge1", "master", BOND]),
        cmd(&["ip", "link", "set", "dev", "ge0", "up"]),
        cmd(&["ip", "link", "set", "dev", "ge1", "up"]),
        cmd(&["ip", "link", "set", "dev", BOND, "up"]),
        cmd(&["ip", "link", "set", "dev", BOND, "master", BRIDGE]),
    ]
}

pub fn teardown_lag() -> Vec<Command> {
    vec![
        cmd(&["ip", "link", "set", "dev", BOND, "nomaster"]),
        cmd(&["ip", "link", "set", "dev", "ge0", "nomaster"]),
        cmd(&["ip", "link", "set", "dev", "ge1", "nomaster"]),
        cmd(&["ip", "link", "set", "dev", BOND, "down"]),
        cmd(&["ip", "link", "delete", BOND]),
        cmd(&["ip", "link", "set", "dev", "ge0", "master", BRIDGE]),
        cmd(&["ip", "link", "set", "dev", "ge1", "master", BRIDGE]),
        cmd(&["ip", "link", "set", "dev", "ge0", "up"]),
        cmd(&["ip", "link", "set", "dev", "ge1", "up"]),
    ]
}

/// True for the command that creates `bond0`.
pub fn creates_bond(command: &Command) -> bool {
    matches!(
        command
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice(),
        ["ip", "link", "add", BOND, ..]
    )
}

/// The commands that undo a plan that stopped part-way, as far as it got.
///
/// `previous` is what the switch was running, `document` what the failed plan
/// was moving it to. `bond_created` is whether the plan got as far as creating
/// `bond0` itself.
///
/// Going back is usually `plan(document, previous)`. The exception is a move
/// *into* LACP that failed before it created the bond: the inverse plan would
/// then run [`teardown_lag`], which deletes `bond0` and puts ge0/ge1 straight
/// on the bridge. A `bond0` this plan did not create is somebody else's (a
/// hand-written file, an earlier run), and that is a bond it must not take
/// down. So in that case only what the plan can actually have done is undone:
/// the VLANs it removed, written back from `previous`, and filtering and
/// spanning tree returned to what they were.
pub fn undo_plan(
    previous: &SwitchDocument,
    document: &SwitchDocument,
    bond_created: bool,
) -> Vec<Command> {
    let into_lag =
        document.second_uplink == SecondUplink::Lag && previous.second_uplink != SecondUplink::Lag;
    if !into_lag || bond_created {
        return plan(document, previous);
    }
    // What the kernel can hold at that point: no bond, no VLANs written yet
    // that `previous` did not already have. Planning from an empty table to
    // `previous` writes every membership back, which `bridge vlan add` takes
    // as a no-op where it is already right.
    let partial = SwitchDocument {
        vlan_filtering: document.vlan_filtering,
        stp: document.stp,
        second_uplink: previous.second_uplink,
        ports: BTreeMap::new(),
        names: BTreeMap::new(),
    };
    plan(&partial, previous)
}

/// `bridge -j vlan show`: every VLAN on every bridge port, as JSON.
pub const SHOW_VLANS: [&str; 4] = ["bridge", "-j", "vlan", "show"];

/// One VLAN on one port, as the kernel holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KernelVlan {
    pub pvid: bool,
    pub untagged: bool,
}

/// What the kernel says each port is in, by interface name.
pub type KernelVlans = BTreeMap<String, BTreeMap<u16, KernelVlan>>;

/// Read the output of [`SHOW_VLANS`]. `None` when it is not what `bridge -j`
/// prints, which callers treat as "unknown", never as "empty".
pub fn parse_bridge_vlans(json: &str) -> Option<KernelVlans> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let mut out = KernelVlans::new();
    for entry in value.as_array()? {
        let name = entry.get("ifname")?.as_str()?;
        let port = out.entry(name.to_string()).or_default();
        let vlans = entry
            .get("vlans")
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or_default();
        for vlan in vlans {
            let first = u16::try_from(vlan.get("vlan")?.as_u64()?).ok()?;
            let last = match vlan.get("vlanEnd").and_then(|v| v.as_u64()) {
                Some(end) => u16::try_from(end).ok()?,
                None => first,
            };
            let flags: Vec<&str> = vlan
                .get("flags")
                .and_then(|f| f.as_array())
                .map(|f| f.iter().filter_map(|flag| flag.as_str()).collect())
                .unwrap_or_default();
            let info = KernelVlan {
                pvid: flags.contains(&"PVID"),
                untagged: flags.iter().any(|flag| flag.contains("Untagged")),
            };
            for vid in first..=last {
                port.insert(vid, info);
            }
        }
    }
    Some(out)
}

/// The port a kernel interface name stands for.
fn port_for_interface(name: &str) -> Option<PortId> {
    if name == BRIDGE {
        return Some(PortId::Bmc);
    }
    PortId::ALL
        .into_iter()
        .chain([PortId::Bond0])
        .find(|port| interface_name(*port) == Some(name))
}

/// The ports with a netdev whose VLANs `document` expects the kernel to hold
/// right now, and which VLANs. Empty when filtering is off, because the table
/// is inert then. The BMC is left out: `bridge vlan show` does not reliably
/// list the bridge's own entries, and the front ports are programmed by the
/// same commands, so they say whether those ran.
pub fn expected_vlans(document: &SwitchDocument) -> BTreeMap<&'static str, BTreeSet<u16>> {
    let mut out = BTreeMap::new();
    if !document.vlan_filtering {
        return out;
    }
    for (port, config) in &document.ports {
        if let Some(name) = interface_name(*port) {
            // A slave of the bond is not a bridge port, so it holds nothing.
            let bonded = document.second_uplink == SecondUplink::Lag
                && matches!(port, PortId::Ge0 | PortId::Ge1);
            if !bonded && !config.vlans().is_empty() {
                out.insert(name, config.vlans());
            }
        }
    }
    out
}

/// What `bridge -j vlan show` prints for `document` once everything has been
/// run: the front ports' VLANs, with the flags the kernel gives them. For
/// tests, which have no kernel to ask.
#[cfg(test)]
pub(crate) fn show_vlans_json(document: &SwitchDocument) -> String {
    let lag = document.second_uplink == SecondUplink::Lag;
    let entries: Vec<serde_json::Value> = document
        .ports
        .iter()
        .filter(|(port, _)| !(lag && matches!(port, PortId::Ge0 | PortId::Ge1)))
        .filter_map(|(port, config)| {
            let name = interface_name(*port).unwrap_or(BRIDGE);
            let vlans: Vec<serde_json::Value> = config
                .vlans()
                .into_iter()
                .map(|vid| {
                    if config.untagged == Some(vid) {
                        serde_json::json!({"vlan": vid, "flags": ["PVID", "Egress Untagged"]})
                    } else {
                        serde_json::json!({"vlan": vid})
                    }
                })
                .collect();
            (!vlans.is_empty()).then(|| serde_json::json!({"ifname": name, "vlans": vlans}))
        })
        .collect();
    serde_json::Value::Array(entries).to_string()
}

/// The kernel's own account of the switch, as a document.
///
/// Used when a plan has failed part-way: what `intended` says is running is
/// then a guess, and a guess is what the next plan would be computed from.
/// VLANs come from `bridge vlan show`, filtering and spanning tree from the
/// bridge, and the bond from whether `bond0` exists. The BMC's own entry and
/// the VLAN names come from `fallback`, because the kernel does not report
/// them reliably (the first) or at all (the second).
pub fn document_from_kernel(
    vlans: &KernelVlans,
    filtering: bool,
    stp: bool,
    bond_present: bool,
    fallback: &SwitchDocument,
) -> SwitchDocument {
    let mut ports: BTreeMap<PortId, PortConfig> = PortId::ALL
        .into_iter()
        .map(|port| (port, PortConfig::default()))
        .collect();
    if bond_present {
        ports.insert(PortId::Bond0, PortConfig::default());
    }
    for (name, held) in vlans {
        let Some(port) = port_for_interface(name) else {
            continue;
        };
        if port == PortId::Bond0 && !bond_present {
            continue;
        }
        let untagged = held
            .iter()
            .find(|(_, flags)| flags.pvid && flags.untagged)
            .map(|(vid, _)| *vid);
        let tagged = held
            .keys()
            .copied()
            .filter(|vid| Some(*vid) != untagged)
            .collect();
        ports.insert(port, PortConfig { untagged, tagged });
    }
    if let Some(bmc) = fallback.ports.get(&PortId::Bmc) {
        ports.insert(PortId::Bmc, bmc.clone());
    }
    let mut document = SwitchDocument {
        vlan_filtering: filtering,
        stp,
        second_uplink: SecondUplink::Redundant,
        ports,
        names: fallback.names.clone(),
    };
    document.second_uplink = infer_second_uplink(&document);
    document
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::switch_document::{Preset, SecondUplink};

    fn trunk() -> SwitchDocument {
        Preset::Trunk {
            management_vid: 10,
            node_vid: 20,
            second_uplink: SecondUplink::Redundant,
        }
        .expand()
    }

    fn joined(commands: &[Command]) -> Vec<String> {
        commands.iter().map(|c| c.join(" ")).collect()
    }

    #[test]
    fn applying_a_document_to_itself_does_nothing() {
        assert!(plan(&trunk(), &trunk()).is_empty());
        assert!(plan(&Preset::flat().expand(), &Preset::flat().expand()).is_empty());
    }

    /// The BMC has no `bmc` netdev, so no command can name one; its VLANs are
    /// `dev br0 ... self` lines, covered by the tests below.
    #[test]
    fn no_command_ever_names_the_bmc_port() {
        let lines = joined(&plan(&Preset::flat().expand(), &trunk()));
        assert!(!lines.iter().any(|l| l.contains("dev bmc")), "{lines:#?}");
        assert_eq!(interface_name(PortId::Bmc), None);
    }

    /// Only the `br0 ... self` lines of a plan: the BMC's own.
    fn self_lines(plan: &[Command]) -> Vec<String> {
        joined(plan)
            .into_iter()
            .filter(|l| l.contains("dev br0") && l.ends_with(" self"))
            .collect()
    }

    /// The BMC joins its new VLAN before it leaves the old one, so it is never
    /// in none: a plan that stops part-way is not rolled back.
    #[test]
    fn the_bmc_vlan_is_programmed_on_br0_self_add_first() {
        let to_trunk = plan(&Preset::flat().expand(), &trunk());
        assert_eq!(
            self_lines(&to_trunk),
            [
                "bridge vlan add dev br0 vid 10 pvid untagged self",
                "bridge vlan del dev br0 vid 1 self",
            ]
        );
        assert_eq!(
            self_lines(&plan(&Preset::flat().expand(), &Preset::Split.expand())),
            [
                "bridge vlan add dev br0 vid 4093 pvid untagged self",
                "bridge vlan del dev br0 vid 1 self",
            ]
        );
    }

    /// The path that runs when a change is not confirmed: back to Flat, the BMC
    /// on VID 1 again, added before the management VLAN is dropped.
    #[test]
    fn reverting_trunk_to_flat_puts_the_bmc_back_on_vid_1_first() {
        let back = plan(&trunk(), &Preset::flat().expand());
        assert_eq!(
            self_lines(&back),
            [
                "bridge vlan add dev br0 vid 1 pvid untagged self",
                "bridge vlan del dev br0 vid 10 self",
            ]
        );
        // Filtering still comes off before anything else.
        assert!(joined(&back)[0].contains("vlan_filtering 0"), "{back:#?}");
    }

    #[test]
    fn changing_the_management_vid_moves_the_bmc_add_first() {
        let mut ten = trunk();
        let mut twenty = trunk();
        for (doc, vid) in [(&mut ten, 10), (&mut twenty, 20)] {
            doc.ports.get_mut(&PortId::Bmc).unwrap().untagged = Some(vid);
        }
        assert_eq!(
            self_lines(&plan(&ten, &twenty)),
            [
                "bridge vlan add dev br0 vid 20 pvid untagged self",
                "bridge vlan del dev br0 vid 10 self",
            ]
        );
    }

    #[test]
    fn an_unchanged_management_vid_emits_no_self_lines() {
        // Trunk to a Trunk that differs only in the node VLAN.
        let mut other = trunk();
        other.ports.get_mut(&PortId::Node1).unwrap().untagged = Some(30);
        let lines = plan(&trunk(), &other);
        assert!(!lines.is_empty(), "the node port did change");
        assert!(self_lines(&lines).is_empty(), "{lines:#?}");
    }

    /// Whatever else is in the plan, the BMC's lines come after filtering is
    /// switched off, before any front port is touched, and before it is
    /// switched on.
    #[test]
    fn the_bmc_lines_sit_between_filtering_off_and_the_front_ports() {
        let lines = joined(&plan(&trunk(), &Preset::flat().expand()));
        let first_self = lines.iter().position(|l| l.ends_with(" self")).unwrap();
        let first_front = lines.iter().position(|l| l.contains("dev node")).unwrap();
        assert!(first_self < first_front, "{lines:#?}");

        let lines = joined(&plan(&Preset::flat().expand(), &trunk()));
        let last_self = lines.iter().rposition(|l| l.ends_with(" self")).unwrap();
        let on = lines
            .iter()
            .position(|l| l.contains("vlan_filtering 1"))
            .unwrap();
        assert!(last_self < on, "{lines:#?}");
    }

    /// Only the delete of the bridge's own entry may fail without stopping the
    /// plan; every other command still does.
    #[test]
    fn only_a_self_delete_is_best_effort() {
        let plan = plan(&Preset::flat().expand(), &trunk());
        let tolerated: Vec<_> = plan.iter().filter(|c| is_best_effort(c)).collect();
        assert_eq!(
            joined(&tolerated.iter().map(|c| (*c).clone()).collect::<Vec<_>>()),
            ["bridge vlan del dev br0 vid 1 self"]
        );
        assert!(!is_best_effort(&cmd(&[
            "bridge", "vlan", "del", "dev", "node1", "vid", "1"
        ])));
        assert!(!is_best_effort(&cmd(&[
            "bridge", "vlan", "add", "dev", "br0", "vid", "10", "pvid", "untagged", "self"
        ])));
    }

    /// A name is not a switch setting. Renaming a VLAN must not touch the
    /// hardware, or every relabelling would cost the board a reconfiguration
    /// of something that was already right.
    #[test]
    fn renaming_a_vlan_plans_nothing() {
        let running = trunk();
        let mut renamed = running.clone();
        renamed
            .names
            .insert(20, "the modules, out the back".to_string());
        assert_ne!(running, renamed, "the documents do differ");
        assert!(plan(&running, &renamed).is_empty());
    }

    /// Filtering must not be switched on before the VLANs exist, and must be
    /// switched off before they are removed. A bridge filtering with no VLANs
    /// forwards nothing.
    #[test]
    fn filtering_goes_on_last_and_comes_off_first() {
        let on = joined(&plan(&Preset::flat().expand(), &trunk()));
        let index = on
            .iter()
            .position(|l| l.contains("vlan_filtering 1"))
            .expect("filtering is turned on");
        assert_eq!(
            index,
            on.len() - 2,
            "filtering should be enabled after the VLANs: {on:#?}"
        );

        let off = joined(&plan(&trunk(), &Preset::flat().expand()));
        let index = off
            .iter()
            .position(|l| l.contains("vlan_filtering 0"))
            .expect("filtering is turned off");
        assert_eq!(
            index, 0,
            "filtering should be disabled before anything else: {off:#?}"
        );
    }

    /// A port moving between untagged VLANs must leave the old one before it
    /// joins the new one, or it is briefly in both.
    #[test]
    fn removals_come_before_additions() {
        let lines: Vec<_> = joined(&plan(&Preset::Split.expand(), &trunk()))
            .into_iter()
            .filter(|l| l.starts_with("bridge vlan") && !l.ends_with(" self"))
            .collect();
        let first_add = lines.iter().position(|l| l.contains("vlan add")).unwrap();
        let last_del = lines.iter().rposition(|l| l.contains("vlan del")).unwrap();
        assert!(last_del < first_add, "{lines:#?}");
    }

    #[test]
    fn an_untagged_vlan_is_added_as_pvid_untagged() {
        let lines = joined(&plan(&Preset::flat().expand(), &trunk()));
        assert!(
            lines
                .iter()
                .any(|l| l == "bridge vlan add dev node1 vid 20 pvid untagged"),
            "{lines:#?}"
        );
    }

    #[test]
    fn a_tagged_vlan_is_added_without_pvid() {
        let lines = joined(&plan(&Preset::flat().expand(), &trunk()));
        assert!(
            lines.iter().any(|l| l == "bridge vlan add dev ge0 vid 10"),
            "{lines:#?}"
        );
        assert!(!lines
            .iter()
            .any(|l| l == "bridge vlan add dev ge0 vid 10 pvid untagged"));
    }

    #[test]
    fn spanning_tree_is_only_touched_when_it_changes() {
        let with_stp = joined(&plan(&Preset::flat().expand(), &trunk()));
        assert!(with_stp.iter().any(|l| l.contains("stp_state 1")));

        let no_change = joined(&plan(&trunk(), &trunk()));
        assert!(!no_change.iter().any(|l| l.contains("stp_state")));
    }

    /// Taking the second uplink out of service should touch ge1 and nothing
    /// else, which is the cheapest possible proof that the plan is a
    /// difference and not a rewrite.
    #[test]
    fn a_narrow_change_produces_a_narrow_plan() {
        let redundant = trunk();
        let off = Preset::Trunk {
            management_vid: 10,
            node_vid: 20,
            second_uplink: SecondUplink::Off,
        }
        .expand();
        let lines = joined(&plan(&redundant, &off));
        assert!(
            lines
                .iter()
                .all(|l| l.contains("ge1") || l.contains("stp_state")),
            "{lines:#?}"
        );
    }

    #[test]
    fn flat_to_flat_lag_creates_bond0_and_clears_ge_vlans() {
        let flat = Preset::flat().expand();
        let lag = Preset::Flat {
            second_uplink: SecondUplink::Lag,
        }
        .expand();
        let lines = joined(&plan(&flat, &lag));
        assert!(
            lines.iter().any(|l| l
                == "ip link add bond0 type bond mode 802.3ad miimon 100 \
                    xmit_hash_policy layer2+3 lacp_rate fast"),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l == "ip link set dev bond0 master br0"),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("vlan del dev ge0") || l.contains("vlan del dev ge1")),
            "slaves leave the flat VLAN before bonding: {lines:#?}"
        );
    }

    #[test]
    fn lag_to_flat_tears_down_bond0() {
        let lag = Preset::Flat {
            second_uplink: SecondUplink::Lag,
        }
        .expand();
        let flat = Preset::flat().expand();
        let lines = joined(&plan(&lag, &flat));
        assert!(
            lines.iter().any(|l| l == "ip link delete bond0"),
            "{lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l == "ip link set dev ge0 master br0"),
            "{lines:#?}"
        );
    }

    #[test]
    fn flat_to_trunk_lag_clears_kernel_default_pvid_on_bond0() {
        let flat = Preset::flat().expand();
        let lag = Preset::Trunk {
            management_vid: 10,
            node_vid: 20,
            second_uplink: SecondUplink::Lag,
        }
        .expand();
        let lines = joined(&plan(&flat, &lag));
        assert!(
            lines.iter().any(|l| l == "bridge vlan del dev bond0 vid 1"),
            "bond0 joins br0 with default_pvid 1: {lines:#?}"
        );
    }

    #[test]
    fn lag_to_trunk_clears_kernel_default_pvid_on_ge_ports() {
        let lag = Preset::Flat {
            second_uplink: SecondUplink::Lag,
        }
        .expand();
        let trunk = trunk();
        let lines = joined(&plan(&lag, &trunk));
        assert!(
            lines.iter().any(|l| l == "bridge vlan del dev ge0 vid 1"),
            "ge0 rejoins br0 with default_pvid 1: {lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l == "bridge vlan del dev ge1 vid 1"),
            "ge1 rejoins br0 with default_pvid 1: {lines:#?}"
        );
    }

    fn lag_flat() -> SwitchDocument {
        Preset::Flat {
            second_uplink: SecondUplink::Lag,
        }
        .expand()
    }

    fn lag_trunk() -> SwitchDocument {
        Preset::Trunk {
            management_vid: 10,
            node_vid: 20,
            second_uplink: SecondUplink::Lag,
        }
        .expand()
    }

    /// Back and forth, and back again. The second trip into LACP has to look
    /// like the first, whatever the kernel did to the ports in between.
    #[test]
    fn lag_to_flat_and_back_again_plans_the_same_way_both_times() {
        let flat = Preset::flat().expand();
        let into = joined(&plan(&flat, &lag_trunk()));
        let out = joined(&plan(&lag_trunk(), &flat));
        let again = joined(&plan(&flat, &lag_trunk()));
        assert_eq!(into, again);

        // Out of LACP: the bond goes and the slaves rejoin br0, where the
        // kernel gives them VID 1: exactly what Flat wants, so no add.
        assert!(
            out.contains(&"ip link delete bond0".to_string()),
            "{out:#?}"
        );
        assert!(
            out.contains(&"ip link set dev ge0 master br0".to_string()),
            "{out:#?}"
        );
        assert!(
            !out.iter().any(|l| l.starts_with("bridge vlan add dev ge")),
            "{out:#?}"
        );
        // Into LACP: bond0 is created and carries the trunk's VLANs.
        assert!(
            into.iter()
                .any(|l| l.starts_with("ip link add bond0 type bond")),
            "{into:#?}"
        );
        for vid in [10, 20] {
            assert!(
                into.contains(&format!("bridge vlan add dev bond0 vid {vid}")),
                "{into:#?}"
            );
        }
        assert!(
            into.contains(&"bridge vlan del dev bond0 vid 1".to_string()),
            "the kernel's default VID 1 on a new bridge port goes: {into:#?}"
        );
    }

    /// The bond is made between the removals and the additions, and filtering
    /// comes on after both: the order the rest of the plan relies on.
    #[test]
    fn the_bond_is_made_between_the_removals_and_the_additions() {
        let lines = joined(&plan(&Preset::flat().expand(), &lag_trunk()));
        let add_bond = lines
            .iter()
            .position(|l| l.starts_with("ip link add bond0"))
            .unwrap();
        let last_front_del = lines
            .iter()
            .rposition(|l| l.starts_with("bridge vlan del dev ge"))
            .unwrap();
        let first_front_add = lines
            .iter()
            .position(|l| l.starts_with("bridge vlan add dev node"))
            .unwrap();
        let on = lines
            .iter()
            .position(|l| l.contains("vlan_filtering 1"))
            .unwrap();
        assert!(last_front_del < add_bond, "{lines:#?}");
        assert!(add_bond < first_front_add, "{lines:#?}");
        assert!(first_front_add < on, "{lines:#?}");
    }

    #[test]
    fn only_the_command_that_makes_the_bond_creates_it() {
        let plan = plan(&Preset::flat().expand(), &lag_flat());
        let creating: Vec<_> = plan.iter().filter(|c| creates_bond(c)).collect();
        assert_eq!(creating.len(), 1, "{plan:#?}");
        assert!(!creates_bond(&cmd(&["ip", "link", "delete", BOND])));
        assert!(!creates_bond(&cmd(&[
            "ip", "link", "set", "dev", BOND, "up"
        ])));
    }

    /// A move into LACP that failed before it made the bond must not undo
    /// itself by tearing a bond down: the one there is not its own.
    #[test]
    fn undoing_a_move_into_lag_that_never_made_the_bond_does_not_touch_it() {
        let flat = Preset::flat().expand();
        let undo = joined(&undo_plan(&flat, &lag_trunk(), false));
        assert!(!undo.iter().any(|l| l.contains("bond0")), "{undo:#?}");
        assert!(!undo.iter().any(|l| l.contains("nomaster")), "{undo:#?}");
        assert!(
            !undo.iter().any(|l| l.contains("master br0")),
            "ge0 and ge1 were not moved, so are not moved back: {undo:#?}"
        );
        // What it can have done is put back: VLANs written from the old
        // document.
        assert!(
            undo.contains(&"bridge vlan add dev ge0 vid 1 pvid untagged".to_string()),
            "{undo:#?}"
        );
    }

    /// Once the bond is made it is this plan's, and goes.
    #[test]
    fn undoing_a_move_into_lag_that_made_the_bond_removes_it() {
        let flat = Preset::flat().expand();
        let undo = joined(&undo_plan(&flat, &lag_trunk(), true));
        assert!(
            undo.contains(&"ip link delete bond0".to_string()),
            "{undo:#?}"
        );
        assert!(
            undo.contains(&"ip link set dev ge0 master br0".to_string()),
            "{undo:#?}"
        );
    }

    /// Out of LACP and back is not a move into it: the bond was there, and is
    /// put back the same way whatever got made.
    #[test]
    fn undoing_a_move_out_of_lag_is_the_plain_inverse() {
        let flat = Preset::flat().expand();
        for created in [false, true] {
            assert_eq!(
                undo_plan(&lag_trunk(), &flat, created),
                plan(&flat, &lag_trunk())
            );
        }
    }

    #[test]
    fn the_bridge_vlan_listing_is_read_with_flags_and_ranges() {
        let json = r#"[
            {"ifname":"ge0","vlans":[{"vlan":1,"flags":["PVID","Egress Untagged"]},
                                      {"vlan":10,"vlanEnd":12}]},
            {"ifname":"br0","vlans":[{"vlan":1,"flags":["PVID","Egress Untagged"]}]}
        ]"#;
        let held = parse_bridge_vlans(json).expect("parses");
        let ge0 = &held["ge0"];
        assert_eq!(ge0.len(), 4, "{ge0:?}");
        assert_eq!(
            ge0[&1],
            KernelVlan {
                pvid: true,
                untagged: true
            }
        );
        assert_eq!(ge0[&11], KernelVlan::default());
        assert!(held["br0"].contains_key(&1));
        assert_eq!(parse_bridge_vlans("not json"), None);
        assert_eq!(
            parse_bridge_vlans("{}"),
            None,
            "an object is not the listing"
        );
    }

    /// The kernel's account of a switch that is in the state a document says
    /// reads back as that document: what makes it a safe thing to plan from.
    #[test]
    fn a_switch_in_the_documents_state_reads_back_as_the_document() {
        for document in [
            Preset::flat().expand(),
            Preset::Split.expand(),
            trunk(),
            Preset::Trunk {
                management_vid: 10,
                node_vid: 20,
                second_uplink: SecondUplink::Off,
            }
            .expand(),
            lag_flat(),
            lag_trunk(),
        ] {
            let held = parse_bridge_vlans(&show_vlans_json(&document)).expect("parses");
            let back = document_from_kernel(
                &held,
                document.vlan_filtering,
                document.stp,
                document.second_uplink == SecondUplink::Lag,
                &document,
            );
            assert_eq!(back, document);
        }
    }

    /// Where the kernel disagrees with what was meant, the kernel wins.
    #[test]
    fn the_kernel_wins_over_the_document_it_is_read_against() {
        let intended = lag_trunk();
        // The bond never got made, and ge0 kept a VLAN.
        let held = parse_bridge_vlans(
            r#"[{"ifname":"ge0","vlans":[{"vlan":10},{"vlan":1,"flags":["PVID","Egress Untagged"]}]}]"#,
        )
        .unwrap();
        let read = document_from_kernel(&held, false, false, false, &intended);
        assert_eq!(read.second_uplink, SecondUplink::Redundant);
        assert!(!read.ports.contains_key(&PortId::Bond0));
        assert!(!read.vlan_filtering);
        assert_eq!(
            read.ports[&PortId::Ge0],
            PortConfig {
                untagged: Some(1),
                tagged: [10].into_iter().collect()
            }
        );
        // The BMC's own entry is the document's: the kernel does not say.
        assert_eq!(read.ports[&PortId::Bmc], intended.ports[&PortId::Bmc]);
    }

    /// A slave of the bond is not a bridge port and holds nothing, so a
    /// healthy LACP switch is not expected to show VLANs on it.
    #[test]
    fn expected_vlans_skip_bond_slaves_and_the_bmc() {
        let expected = expected_vlans(&lag_trunk());
        assert!(expected.contains_key("bond0"));
        assert!(!expected.contains_key("ge0"));
        assert!(!expected.contains_key("br0"));
        assert!(expected_vlans(&Preset::flat().expand()).is_empty());
    }
}
