// Copyright 2023 Turing Machines
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
use super::{
    helpers::{bit_iterator, load_lines},
    NodeId,
};
use anyhow::Context;
use gpiod::{Chip, Lines, Options, Output};
use std::path::PathBuf;
use std::{str::FromStr, time::Duration};
use tokio::time::sleep;
use tracing::{debug, trace, warn};

const SYS_LED: &str = "/sys/class/leds/fp::power/brightness";
const SYS_LED_2_0_5: &str = "/sys/class/leds/fp:sys/brightness";
const STATUS_LED: &str = "/sys/class/leds/fp::status/brightness";
const STATUS_LED_2_0_5: &str = "/sys/class/leds/fp:reset/brightness";
const PORT1_EN: &str = "node1-en";
const PORT2_EN: &str = "node2-en";
const PORT3_EN: &str = "node3-en";
const PORT4_EN: &str = "node4-en";

/// Physical memory, for the latch shadow word below.
const DEV_MEM: &str = "/dev/mem";
/// Boot cookie that the SPL leaves in SoC SRAM ("TPi2"). It is written on every
/// boot, cold or warm, so it only says that the SPL ran and that the shadow
/// word next to it is meaningful.
const BOOT_COOKIE_ADDR: usize = 0x0709_0108;
const BOOT_COOKIE: u32 = 0x3269_5054;
/// Shadow of the nine latched lines of the Turing Pi 2 (v2.5 and later). The
/// kernel's gpio-latch driver mirrors every write into this word and reads it
/// back, and the SPL clears it on a cold boot only, so it survives a reboot of
/// the BMC. Bit 0 is the ATX PSU, bits 1..=4 the slot rails, bits 5..=8 the
/// node enable lines (node 1 is bit 8, node 4 is bit 5, active low).
const LATCH_SHADOW_ADDR: usize = 0x0709_010c;
const LATCH_ATX_BIT: u32 = 0;
/// Latch bit of the enable line of node 1; node n is `NODE1_EN_BIT - (n - 1)`.
const NODE1_EN_BIT: u32 = 8;

// This structure is a thin layer that abstracts away the interaction details
// with Linux's power subsystem.
pub struct PowerController {
    enable: [Lines<Output>; 4],
    sysfs_power: PathBuf,
    sysfs_reset: PathBuf,
    /// Whether the board has the latch and its shadow word.
    latching: bool,
}

impl PowerController {
    /// `is_legacy_dts` is true on a Turing Pi 2 v2.4, which has no latches: the
    /// node lines are on `gpiochip1` and there is no latch shadow. From v2.5
    /// the node lines are the `nodes` chip, `gpiochip2`, on top of the latch.
    pub fn new(is_legacy_dts: bool) -> anyhow::Result<Self> {
        let has_latch = !is_legacy_dts;
        let chip1 = if is_legacy_dts {
            "/dev/gpiochip1"
        } else {
            "/dev/gpiochip2"
        };

        let chip1 = Chip::new(chip1).context(chip1)?;
        let lines = load_lines(&chip1);
        let port1 = *lines
            .get(PORT1_EN)
            .ok_or(anyhow::anyhow!("cannot find PORT1_EN"))?;
        let port2 = *lines
            .get(PORT2_EN)
            .ok_or(anyhow::anyhow!("cannot find PORT2_EN"))?;
        let port3 = *lines
            .get(PORT3_EN)
            .ok_or(anyhow::anyhow!("cannot find PORT3_EN"))?;
        let port4 = *lines
            .get(PORT4_EN)
            .ok_or(anyhow::anyhow!("cannot find PORT4_EN"))?;

        // A request of an output line drives it, to the value given here. A
        // request that did not say a value drove every enable line to 0, i.e.
        // "off", and on the 6.18 kernel (whose gpio-latch has no
        // `direction_output` that spares the line) it wrote that into the
        // latch and its shadow. That held running nodes in reset for the
        // moment until the power state was applied, and it made every later
        // read of the lines say "all off". Ask for what the board is doing
        // instead, which for a cold board is 0 anyway.
        let live = if has_latch {
            read_latch_power_state()
                .map_err(|e| warn!("cannot read the latch shadow: {:#}", e))
                .ok()
        } else {
            None
        };
        let initial = live.unwrap_or(0);
        let request = |port: gpiod::LineId, idx: usize| {
            chip1
                .request_lines(Options::output([port]).values([initial & (1 << idx) != 0]))
                .with_context(|| format!("error initializing pin node{}-en", idx + 1))
        };
        let enable = [
            request(port1, 0)?,
            request(port2, 1)?,
            request(port3, 2)?,
            request(port4, 3)?,
        ];

        let sysfs_power = fallback_if_not_exist(SYS_LED, SYS_LED_2_0_5);
        let sysfs_reset = fallback_if_not_exist(STATUS_LED, STATUS_LED_2_0_5);

        Ok(PowerController {
            enable,
            sysfs_power,
            sysfs_reset,
            latching: has_latch,
        })
    }

    /// Function to power on/off given nodes. Powering of the nodes is controlled by
    /// the Linux subsystem.
    ///
    /// # Arguments
    ///
    /// * `node_states`     bit-field representing the nodes on the turing-pi board,
    ///   where bit 1 is on and 0 equals off.
    /// * `node_mask`       bit-field to describe which nodes to control.
    ///
    /// # Returns
    ///
    /// * `Ok(())` when routine was executed successfully.
    /// * `Err(io error)` in the case there was a failure to write to the Linux
    ///   subsystem that handles the node powering.
    pub async fn set_power_node(&self, node_states: u8, node_mask: u8) -> anyhow::Result<()> {
        let updates = bit_iterator(node_states, node_mask);

        for (idx, state) in updates {
            trace!("setting power of node {}. state:{}", idx + 1, state);
            set_mode(idx + 1, state).await?;
            sleep(Duration::from_millis(100)).await;
            self.enable[idx].set_values(state)?;
        }

        Ok(())
    }

    /// Read back which nodes are powered.
    ///
    /// # Returns
    ///
    /// * `Ok(bit-field)` where bit(n) = 1 means node n is powered. On a
    ///   latching board this is read from the latch shadow in SoC SRAM, which
    ///   outlives a reboot of the BMC and is cleared by a cold boot, so it
    ///   reflects the rails that are actually on, not what this daemon last
    ///   requested. Where the shadow cannot be read, and on a board without
    ///   latches, it is the state of the node enable lines.
    /// * `Err(gpio error)` when a line could not be read.
    ///
    /// The enable lines alone cannot tell a running board from a cold one: they
    /// are active low, and the SPL clears every latch on a cold boot, which
    /// reads as "enabled".
    pub fn get_power_node(&self) -> anyhow::Result<u8> {
        if self.latching {
            match read_latch_power_state() {
                Ok(state) => return Ok(state),
                Err(e) => warn!("cannot read the latch shadow: {:#}", e),
            }
        }

        let mut node_states = 0u8;
        for (idx, line) in self.enable.iter().enumerate() {
            let state = line
                .get_values(0u8)
                .with_context(|| format!("error reading enable line of node {}", idx + 1))?;
            node_states |= (state & 1) << idx;
        }

        Ok(node_states)
    }

    /// Reset a given node by setting the reset pin logically high for 1 second
    pub async fn reset_node(&self, node: NodeId) -> anyhow::Result<()> {
        debug!("reset node {:?}", node);
        let bits = node.to_bitfield();

        self.set_power_node(0u8, bits).await?;
        sleep(Duration::from_secs(1)).await;
        self.set_power_node(bits, bits).await?;
        Ok(())
    }

    pub async fn power_led(&self, on: bool) -> anyhow::Result<()> {
        tokio::fs::write(&self.sysfs_power, if on { "1" } else { "0" })
            .await
            .context(SYS_LED)
    }

    pub async fn status_led(&self, on: bool) -> anyhow::Result<()> {
        tokio::fs::write(&self.sysfs_reset, if on { "1" } else { "0" })
            .await
            .context(STATUS_LED)
    }
}

async fn set_mode(node_id: usize, node_state: u8) -> std::io::Result<()> {
    let node_value = if node_state > 0 {
        "enabled"
    } else {
        "disabled"
    };

    let sys_path = format!("/sys/bus/platform/devices/node{}-power/state", node_id);
    tokio::fs::write(sys_path, node_value).await
}

fn fallback_if_not_exist(sysfs: &str, fallback: &str) -> PathBuf {
    let mut sysfs = PathBuf::from_str(sysfs).expect("valid utf8 path");
    if !sysfs.exists() {
        sysfs = PathBuf::from_str(fallback).expect("valid utf8 path");
        tracing::info!("power led: falling back to {}", fallback);
    }
    sysfs
}

/// Which nodes are powered, from the boot cookie and the latch shadow word.
///
/// A node is powered when the ATX PSU latch is set and its enable latch is
/// asserted (the enable lines are active low, so the bit is clear). After a cold
/// boot the SPL has cleared every latch: the ATX bit is clear, so no node counts
/// as powered even though every enable bit reads as asserted. Without the SPL's
/// cookie the shadow word is not meaningful and nothing is powered.
///
/// # Returns
///
/// A bit-field where bit(n) = 1 means node n is powered.
fn nodes_powered_by_latch(cookie: u32, shadow: u32) -> u8 {
    if cookie != BOOT_COOKIE || shadow & (1 << LATCH_ATX_BIT) == 0 {
        return 0;
    }

    (0..4u32)
        .filter(|node| shadow & (1 << (NODE1_EN_BIT - node)) == 0)
        .fold(0u8, |acc, node| acc | (1 << node))
}

/// Reads the boot cookie and the latch shadow word out of SoC SRAM.
///
/// Reads only: no line is requested and nothing is written.
fn read_latch_words() -> anyhow::Result<(u32, u32)> {
    use std::os::fd::AsRawFd;

    const PAGE: usize = 4096;
    let base = LATCH_SHADOW_ADDR & !(PAGE - 1);
    anyhow::ensure!(
        BOOT_COOKIE_ADDR & !(PAGE - 1) == base,
        "the boot cookie and the shadow word are not in one page"
    );

    let file = std::fs::File::open(DEV_MEM).context(DEV_MEM)?;
    // SAFETY: a read-only shared mapping of one page of SRAM at an offset that
    // is page aligned, checked for failure, read with volatile loads inside the
    // page at aligned offsets, and unmapped before returning.
    unsafe {
        let map = nix::libc::mmap(
            std::ptr::null_mut(),
            PAGE,
            nix::libc::PROT_READ,
            nix::libc::MAP_SHARED,
            file.as_raw_fd(),
            base as nix::libc::off_t,
        );
        if map == nix::libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("mmap /dev/mem");
        }

        let word =
            |addr: usize| std::ptr::read_volatile(map.cast::<u8>().add(addr - base).cast::<u32>());
        let words = (word(BOOT_COOKIE_ADDR), word(LATCH_SHADOW_ADDR));
        nix::libc::munmap(map, PAGE);
        Ok(words)
    }
}

/// The powered nodes of a latching board, read from the latch shadow.
///
/// # Returns
///
/// * `Ok(bit-field)` where bit(n) = 1 means node n is powered.
/// * `Err(e)` when the shadow could not be read.
fn read_latch_power_state() -> anyhow::Result<u8> {
    let (cookie, shadow) = read_latch_words()?;
    let state = nodes_powered_by_latch(cookie, shadow);
    debug!(
        "latch shadow {:#05x}, cookie {:#010x}: powered nodes {:#06b}",
        shadow, cookie, state
    );
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ATX on, the four slot rails on, the four enable latches asserted (low):
    /// what the board B had when it was read, `0x1f`.
    const ALL_ON: u32 = 0b0_0001_1111;

    #[test]
    fn a_running_board_reads_all_nodes_powered() {
        assert_eq!(nodes_powered_by_latch(BOOT_COOKIE, ALL_ON), 0b1111);
    }

    #[test]
    fn a_cold_board_reads_nothing_powered_although_every_enable_bit_is_clear() {
        // The SPL clears all nine latches on a cold boot. The enable bits are
        // active low, so they read as asserted; only the ATX bit gives it away.
        assert_eq!(nodes_powered_by_latch(BOOT_COOKIE, 0), 0);
    }

    #[test]
    fn a_node_whose_enable_latch_is_released_is_not_powered() {
        // Node 2 off: its enable latch is bit 7 and is high when released.
        assert_eq!(
            nodes_powered_by_latch(BOOT_COOKIE, ALL_ON | (1 << 7)),
            0b1101
        );
        // Node 1 off: bit 8. Node 4 off: bit 5.
        assert_eq!(
            nodes_powered_by_latch(BOOT_COOKIE, ALL_ON | (1 << 8)),
            0b1110
        );
        assert_eq!(
            nodes_powered_by_latch(BOOT_COOKIE, ALL_ON | (1 << 5)),
            0b0111
        );
    }

    #[test]
    fn all_nodes_off_on_a_warm_board_reads_nothing_powered() {
        // Every enable latch released, ATX released with the last slot.
        assert_eq!(nodes_powered_by_latch(BOOT_COOKIE, 0b1_1110_0000), 0);
        // And with the ATX latch still set.
        assert_eq!(nodes_powered_by_latch(BOOT_COOKIE, 0b1_1110_0001), 0);
    }

    #[test]
    fn a_shadow_without_the_spl_cookie_is_not_trusted() {
        assert_eq!(nodes_powered_by_latch(0, ALL_ON), 0);
        assert_eq!(nodes_powered_by_latch(0x5aa5_a55a, ALL_ON), 0);
    }

    #[test]
    fn bits_above_the_nine_latches_are_ignored() {
        assert_eq!(
            nodes_powered_by_latch(BOOT_COOKIE, ALL_ON | 0xffff_fe00),
            0b1111
        );
    }
}
