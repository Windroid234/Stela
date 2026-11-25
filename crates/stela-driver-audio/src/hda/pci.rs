// SPDX-License-Identifier: GPL-2.0-only
//! Bounded conventional HDA PCI attachment. Quirks and exact controller IDs adapt
//! Linux sound/hda/controllers/intel.c, include/linux/pci_ids.h, capability
//! layouts in include/uapi/linux/pci_regs.h, and PCI D0 transition sequencing
//! from drivers/pci/pci.c at
//! a804c5742bff224d4cb4d25dd383554d598be32b.
//!
//! Copyright(c) 2004 Intel Corporation
//! Copyright (c) 2004 Takashi Iwai <tiwai@suse.de>
//!                     PeiSen Hou <pshou@realtek.com.tw>
//! Copyright 1993 -- 1997 Drew Eckhardt, Frederic Potter, David Mosberger-Tang
//! Copyright 1997 -- 2000 Martin Mares <mj@ucw.cz>
//!
//! Platform ACPI power resources, firmware/AML/SMM arbitration and coherent DMA
//! mappings remain prerequisites. DSP/GPU controllers require additional owners
//! and are absent from this attachment table.
use super::{
    Config, CorbReset, MmioRegisters, PositionPolicy, ProbePolicy, Registers, ResetPolicy,
    StreamTagPolicy,
};
use crate::Timer;
use crate::bus::pci::regs;
use crate::bus::pci::{Address, Bar, ConfigSpace, Function, Layout};

mod power;
mod registers;
pub use power::PowerError;
pub use registers::{BindingFailure, OwnedBindingResult, PciRegisters};

pub const CLASS: [u8; 3] = [0x04, 0x03, 0x00];
const INTEL: u16 = 0x8086;
// Exact AZX_DRIVER_SKL | AZX_DCAPS_INTEL_SKYLAKE conventional subset in
// pinned intel.c/pci_ids.h. This is not a range or a DSP/graphics admission.
const INTEL_CLOCK_GATE_IDS: &[u16] = &[
    0xa170, 0x9d70, // SKL / SKL-LP.
    0xa171, 0x9d71, 0xa2f0, // KBL / KBL-LP / KBL-H.
    0xa348, 0x9dc8, // Coffee / Cannon.
    0x02c8, 0x06c8, 0xa3f0, 0xf0c8, 0xf1c8, // Comet LP/H/S/R, Rocket S.
    0x34c8, 0x3dc8, 0x38c8, 0x4dc8, // Ice LP/H/N, Jasper N.
    0xa0c8, 0x43c8, // Tiger LP/H.
    0x7ad0, 0x51c8, 0x51c9, 0x51cd, 0x51cc, 0x54c8, // Alder S/P/PS/PX/M/N.
    0x4b55, 0x4b58, // Elkhart.
    0x3198, // Gemini Lake; APL-only VS_EM4L latency operation does not apply.
    0x7a50, 0x51ca, 0x51cb, 0x51ce, 0x51cf, // Raptor S/P0/P1/M/PX.
    0x7e28, 0x7f50, 0x7728, // Meteor, Arrow S/H.
];
// Lewisburg selects SKYLAKE capabilities but AZX_DRIVER_PCH, so tags
// are separate while the SKL-only CGCTL/extended reset bracket must not run.
const INTEL_LBG_IDS: &[u16] = &[0xa1f0, 0xa270];
const ATI: u16 = 0x1002;
const AMD: u16 = 0x1022;
const NVIDIA: u16 = 0x10de;
const CAP_PM: u8 = 1;
const CAP_MSI: u8 = 5;
const CAP_MSIX: u8 = 0x11;
const CAP_PCIE: u8 = 0x10;
const SCH_HDA_DEVC: u16 = 0x78;
const SCH_NOSNOOP: u16 = 1 << 11;
const CGCTL: u16 = 0x48;
const CGCTL_MISCBDCGE: u32 = 1 << 6;
#[cfg(test)]
const MSI_ADDRESS_64: u16 = 1 << 7;
#[cfg(test)]
const MSI_VECTOR_MASK: u16 = 1 << 8;
const PM_STATE: u16 = 3;
const PM_PME_ENABLE: u16 = 1 << 8;
const PM_PME_STATUS: u16 = 1 << 15;
const PM_NO_SOFT_RESET: u16 = 1 << 3;
const TCSEL: u16 = 0x44;
const TCSEL_MASK: u8 = 7;
const ATI_SNOOP_CONTROL: u16 = 0x42;
const ATI_SNOOP_MASK: u8 = 7;
const ATI_SNOOP_ENABLE: u8 = 2;
/// Linux `azx_init_pci` for AZX_SNOOP_TYPE_NVIDIA: transaction coherence,
/// then input and output stream coherence.
const NVIDIA_TRANSREG: u16 = 0x4e;
const NVIDIA_ENABLE_COHBITS: u8 = 0x0f;
const NVIDIA_ISTRM_COH: u16 = 0x4d;
const NVIDIA_OSTRM_COH: u16 = 0x4c;
const NVIDIA_ENABLE_COHBIT: u8 = 0x01;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Setup {
    IntelIch,
    IntelPch,
    AtiAzalia,
    /// Zen-family AMD hosts, which take the ATI snoop byte the way Linux
    /// writes it: without reading it back.
    AmdZen,
    Nvidia,
}

/// Exact identity-selected accesses. Native widths preserve adjacent PCI
/// status/vendor fields; in particular Device Status next to DEVC is W1C.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Control {
    Byte {
        offset: u16,
        mask: u8,
        value: u8,
    },
    Word {
        offset: u16,
        mask: u16,
        value: u16,
    },
    /// A byte Linux `update_pci_byte` writes and never reads back: written
    /// the same way, and not refused when it does not hold
    /// ([`unheld_control`] reports that).
    Lenient {
        offset: u16,
        mask: u8,
        value: u8,
    },
}
const PCH_SNOOP: Control = Control::Word {
    offset: SCH_HDA_DEVC,
    mask: SCH_NOSNOOP,
    value: 0,
};
impl Control {
    fn extent(self) -> core::ops::Range<usize> {
        let (offset, bytes) = match self {
            Self::Byte { offset, .. } | Self::Lenient { offset, .. } => (offset, 1),
            Self::Word { offset, .. } => (offset, 2),
        };
        usize::from(offset)..usize::from(offset + bytes)
    }
    fn apply(self, config: &mut impl ConfigSpace) -> Result<(), AttachError> {
        const STEP: &str = "vendor control";
        match self {
            Self::Byte {
                offset,
                mask,
                value,
            } => {
                let previous = config.read8(offset)?;
                if previous == u8::MAX {
                    return Err(AttachError::rejected(STEP, offset, None, 0xff));
                }
                let next = previous & !mask | value;
                config.write8(offset, next)?;
                let observed = config.read8(offset)?;
                if observed != next {
                    return Err(AttachError::rejected(
                        STEP,
                        offset,
                        Some(next.into()),
                        observed.into(),
                    ));
                }
            }
            Self::Word {
                offset,
                mask,
                value,
            } => {
                let previous = config.read16(offset)?;
                if previous == u16::MAX {
                    return Err(AttachError::rejected(STEP, offset, None, 0xffff));
                }
                let next = previous & !mask | value;
                if next == previous {
                    return Ok(());
                }
                config.write16(offset, next)?;
                let observed = config.read16(offset)?;
                if observed == u16::MAX || observed != next {
                    return Err(AttachError::rejected(
                        STEP,
                        offset,
                        Some(next.into()),
                        observed.into(),
                    ));
                }
            }
            Self::Lenient {
                offset,
                mask,
                value,
            } => {
                let previous = config.read8(offset)?;
                config.write8(offset, previous & !mask | value)?;
            }
        }
        Ok(())
    }
}
impl Setup {
    fn controls(self) -> &'static [Control] {
        match self {
            Self::IntelIch => &[Control::Byte {
                offset: TCSEL,
                mask: TCSEL_MASK,
                value: 0,
            }],
            Self::IntelPch => &[
                Control::Byte {
                    offset: TCSEL,
                    mask: TCSEL_MASK,
                    value: 0,
                },
                PCH_SNOOP,
            ],
            Self::AtiAzalia => &[Control::Byte {
                offset: ATI_SNOOP_CONTROL,
                mask: ATI_SNOOP_MASK,
                value: ATI_SNOOP_ENABLE,
            }],
            Self::AmdZen => &[Control::Lenient {
                offset: ATI_SNOOP_CONTROL,
                mask: ATI_SNOOP_MASK,
                value: ATI_SNOOP_ENABLE,
            }],
            Self::Nvidia => &[
                Control::Byte {
                    offset: NVIDIA_TRANSREG,
                    mask: NVIDIA_ENABLE_COHBITS,
                    value: NVIDIA_ENABLE_COHBITS,
                },
                Control::Byte {
                    offset: NVIDIA_ISTRM_COH,
                    mask: NVIDIA_ENABLE_COHBIT,
                    value: NVIDIA_ENABLE_COHBIT,
                },
                Control::Byte {
                    offset: NVIDIA_OSTRM_COH,
                    mask: NVIDIA_ENABLE_COHBIT,
                    value: NVIDIA_ENABLE_COHBIT,
                },
            ],
        }
    }
}

/// Identity selected by the bounded table, never by a failed handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub vendor: u16,
    pub device: u16,
    pub corb_reset: CorbReset,
    pub reset_policy: ResetPolicy,
    pub stream_tag_policy: StreamTagPolicy,
    /// Explicit platform ceiling, independent of GCAP. ATI uses a conservative
    /// 32-bit policy without companion-SMBus stepping evidence; AMD uses 40 bits.
    pub dma_address_bits: u8,
    pub position_policy: PositionPolicy,
    pub probe_policy: ProbePolicy,
    setup: Setup,
}

/// A platform decision made before touching the PCI function. An identity
/// match alone cannot establish exclusive controller ownership.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ownership {
    ExclusiveLegacy,
    Dsp,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachError {
    MinimumDelay(crate::bus::pci::power::Error),
    Config(crate::bus::pci::ConfigError),
    PciCapability(crate::bus::pci::CapabilityError),
    UnsupportedController,
    Ownership,
    IdentityChanged,
    SubsystemUnavailable,
    SubsystemExcluded,
    BoardIdentityRequired,
    BoardExcluded,
    InvalidBar,
    InvalidCapabilities(CapabilityRefusal),
    MappedPreflightRequired,
    PowerNotReady,
    ActiveDma {
        offset: usize,
    },
    MmioPreflight(crate::Error),
    UnsupportedPowerManagement,
    PowerTransition,
    /// A configuration register read as all ones or did not hold what was
    /// just written, so preparation stopped; the log names which.
    ConfigurationRejected(Rejection),
}

/// Which configuration register stopped preparation, and how, so a log
/// names the step instead of a bare refusal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rejection {
    /// What preparation was doing.
    pub step: &'static str,
    /// The register's offset in configuration space.
    pub offset: u16,
    /// What was written there, or the bits it had to have; `None` when the
    /// register only read as all ones.
    pub wanted: Option<u32>,
    /// What the register read back.
    pub read: u32,
}

impl core::fmt::Debug for Rejection {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} at {:#04x}: ", self.step, self.offset)?;
        if let Some(wanted) = self.wanted {
            write!(f, "wanted {wanted:#x}, ")?;
        }
        write!(f, "read {:#x}", self.read)
    }
}

impl AttachError {
    fn rejected(step: &'static str, offset: u16, wanted: Option<u32>, read: u32) -> Self {
        Self::ConfigurationRejected(Rejection {
            step,
            offset,
            wanted,
            read,
        })
    }
}

/// A vendor byte Linux writes without reading back that did not hold the
/// bits written. Preparation goes on, as Linux does; the log reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unheld {
    /// The byte's offset in configuration space.
    pub offset: u16,
    /// The bits written under the control's mask.
    pub wanted: u8,
    /// The byte read back.
    pub read: u8,
}

/// Which capability check refused the function, with the offsets and IDs
/// it found, so a log names the layout that a controller presents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapabilityRefusal {
    /// The Status register reads all ones: the function is gone.
    StatusUnreadable,
    /// No capability list, where an identity control needs the PCIe
    /// capability to own its register.
    NoList,
    /// MSI-X Message Control at `offset` reads all ones.
    MsixUnreadable { offset: u16 },
    /// The PCIe capability at `offset` has no sized layout.
    PcieLayout { offset: u16 },
    /// Capability `id` at `offset` overlaps the Intel clock-gating or Device
    /// Control registers, or is one whose end cannot be known below them.
    ClockGateOverlap { offset: u16, id: u8 },
    /// The identity's vendor control at `control` lies inside capability
    /// `id`'s body at `offset`.
    ControlOverlap { control: u16, offset: u16, id: u8 },
    /// Capability `id` appears again at `offset`.
    Duplicate { offset: u16, id: u8 },
    /// The PCH snoop control needs the endpoint's PCIe capability ending
    /// at Device Control; the PCIe capability found, if any.
    PcieOwnerMissing { found: Option<u16> },
    /// Power management moved to `found` since preparation.
    PowerManagementMoved { found: Option<u16> },
}

/// Match the enumerated identity only; `prepare` rechecks config space.
/// This is a candidate match; subsystem/DMI exclusions are checked by
/// prepare before any write. Intel DSP and GPU HDA functions remain separate.
pub fn identify<C: ConfigSpace>(function: &Function<C>) -> Result<Identity, AttachError> {
    identify_identity(&function.identity())
}

/// A copied candidate identity does not check BARs, clocks, DMA or resets.
pub fn identify_identity(function: &crate::bus::pci::Identity) -> Result<Identity, AttachError> {
    identity(function.vendor, function.device, function.class)
}

fn identity(vendor: u16, device: u16, class: [u8; 3]) -> Result<Identity, AttachError> {
    if class != CLASS {
        return Err(AttachError::UnsupportedController);
    }
    let (setup, dma_address_bits) = match (vendor, device) {
        (INTEL, 0x2668 | 0x269a | 0x27d8 | 0x284b | 0x293e | 0x293f | 0x3a3e | 0x3a6e) => {
            (Setup::IntelIch, 64)
        }
        // Haswell/Broadwell analog PCH, without I915_COMPONENT. GPU HDMI
        // identities require the separate graphics component and stay refused.
        (
            INTEL,
            0x8c20 | 0x8ca0 | 0x8d20 | 0x8d21 | 0x9c20 | 0x9c21 | 0x9ca0 | 0x1c20 | 0x1d20 | 0x1e20
            | 0x3b56 | 0x3b57 | 0x811b | 0x080a | 0x0f04 | 0x2284,
        ) => (Setup::IntelPch, 64),
        // Mapped ownership, native PCI clock reset hooks and bounded
        // conventional extended-link setup are mandatory for these IDs.
        (INTEL, id) if INTEL_CLOCK_GATE_IDS.contains(&id) || INTEL_LBG_IDS.contains(&id) => {
            (Setup::IntelPch, 64)
        }
        // Linux restricts ATI to 40 bits, and SB600 companion-SMBus revisions
        // before 0x30 additionally require 32 bits. Without companion evidence,
        // keep the supported conservative 32-bit mode on every ATI stepping.
        (ATI, 0x437b | 0x4383) => (Setup::AtiAzalia, 32),
        // Exact AMD hosts use ATI snoop and 40-bit DMA. The later family
        // selects FIFO correction and mandatory bounded discovery below.
        (AMD, 0x780d | 0x157a) => (Setup::AtiAzalia, 40),
        // That later (Zen) family: Linux AZX_DCAPS_PRESET_AMD_SB writes the
        // same snoop byte through update_pci_byte and never reads it back.
        (AMD, 0x1457 | 0x1487 | 0x15e3) => (Setup::AmdZen, 40),
        // Linux binds every NVIDIA HD Audio function with
        // AZX_DCAPS_PRESET_NVIDIA and 40-bit DMA. GF108 0bea's DMI product
        // version denylist has no board field here, so it stays refused.
        (NVIDIA, id) if id != 0x0bea => (Setup::Nvidia, 40),
        _ => return Err(AttachError::UnsupportedController),
    };
    let (position_policy, probe_policy) =
        if vendor == AMD && matches!(device, 0x1457 | 0x1487 | 0x15e3) {
            (
                PositionPolicy::AmdFifo32,
                ProbePolicy::Retry {
                    attempts: 60,
                    delay_us: 1_000_000,
                },
            )
        } else {
            (PositionPolicy::Link, ProbePolicy::Once)
        };
    let modern = vendor == INTEL && INTEL_CLOCK_GATE_IDS.contains(&device);
    Ok(Identity {
        vendor,
        device,
        // AZX_DCAPS_CORBRP_SELF_CLEAR is part of the NVIDIA preset.
        corb_reset: if setup == Setup::Nvidia {
            CorbReset::SelfClearing
        } else {
            CorbReset::Handshake
        },
        reset_policy: if modern {
            ResetPolicy::IntelClockGate
        } else {
            ResetPolicy::Generic
        },
        stream_tag_policy: if modern || (vendor == INTEL && INTEL_LBG_IDS.contains(&device)) {
            StreamTagPolicy::PerDirection
        } else {
            StreamTagPolicy::Global
        },
        dma_address_bits,
        position_policy,
        probe_policy,
        setup,
    })
}

/// Target-machine SMBIOS board strings, when available. Missing data
/// stays Unknown; a retail product name or development-host identity is no proof.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BoardIdentity<'a> {
    #[default]
    Unknown,
    Known {
        vendor: &'a str,
        name: &'a str,
    },
}

fn check_subsystem<C: ConfigSpace>(
    config: &C,
    identity: Identity,
    board: BoardIdentity<'_>,
) -> Result<(), AttachError> {
    if identity.vendor != AMD || !matches!(identity.device, 0x1457 | 0x1487 | 0x15e3) {
        return Ok(());
    }
    let subsystem = config.read32(regs::SUBSYSTEM_VENDOR_ID)?;
    if subsystem == u32::MAX {
        return Err(AttachError::SubsystemUnavailable);
    }
    let pair = (subsystem as u16, (subsystem >> 16) as u16);
    // Exact unconditional PCI denylist from the pinned driver's driver_denylist.
    if identity.device == 0x1487 && matches!(pair, (0x1043, 0x874f) | (0x1462, 0xcb59 | 0xcb60)) {
        return Err(AttachError::SubsystemExcluded);
    }
    // The donor conditions this subsystem on two DMI strings:
    // some laptops share the PCI IDs. Keep that condition, including unknowns.
    if identity.device == 0x15e3 && pair == (0x1462, 0xee59) {
        let BoardIdentity::Known { vendor, name } = board else {
            return Err(AttachError::BoardIdentityRequired);
        };
        if vendor.trim().is_empty() || name.trim().is_empty() {
            return Err(AttachError::BoardIdentityRequired);
        }
        // Linux DMI_MATCH uses case-sensitive substring matching.
        if vendor.contains("Micro-Star International Co., Ltd.")
            && name.contains("MAG X870E TOMAHAWK WIFI (MS-7E59)")
        {
            return Err(AttachError::BoardExcluded);
        }
    }
    Ok(())
}

/// Successful PCI preparation. Keep the function and mapping exclusively owned
/// throughout controller lifetime; no automatic detach/restore runs on drop.
#[derive(Debug)]
pub struct Prepared {
    identity: Identity,
    address: Address,
    base: u64,
    bytes: u64,
}

impl Identity {
    fn requires_mapped_preflight(self) -> bool {
        self.reset_policy == ResetPolicy::IntelClockGate
            || self.stream_tag_policy == StreamTagPolicy::PerDirection
    }

    fn config(self) -> Config {
        Config {
            corb_reset: self.corb_reset,
            reset_policy: self.reset_policy,
            stream_tag_policy: self.stream_tag_policy,
            dma_address_bits: self.dma_address_bits,
            position_policy: self.position_policy,
            probe_policy: self.probe_policy,
            ..Config::default()
        }
    }
}

impl Prepared {
    pub fn identity(&self) -> Identity {
        self.identity
    }

    pub fn config(&self) -> Config {
        self.identity.config()
    }

    pub fn bar_range(&self) -> core::ops::Range<u64> {
        self.base..self.base + self.bytes
    }

    /// # Safety
    /// The prepared BAR range must be identity mapped uncached, exclusively
    /// owned and remain mapped until controller shutdown succeeds. Keep PCI
    /// ownership, power and decode established by `prepare` for that lifetime.
    pub unsafe fn claim_mmio(self) -> Result<MmioRegisters, super::super::Error> {
        if self.identity.requires_mapped_preflight() {
            return Err(super::super::Error::InvalidConfiguration);
        }
        // SAFETY: preparation validated this BAR extent. The caller keeps that
        // range identity-mapped and uncached, with exclusive PCI ownership and
        // stable power/decode until controller shutdown.
        unsafe { MmioRegisters::claim(self.base, self.bytes) }
    }
}

#[derive(Default)]
struct Capabilities {
    pm: Option<u16>,
    msi: Option<u16>,
    msix: Option<u16>,
    pcie: Option<u16>,
}

/// The identity's vendor controls and the function's capabilities, checked
/// against each other.
///
/// NVIDIA's coherence bytes at 0x4c..0x4e are the nForce chipset function's.
/// A GPU's audio function whose capability list covers those bytes (an RTX 50
/// has its MSI capability at 0x48) has no such registers, so it is started
/// without them and the capability is not written through.
fn controls_and_capabilities<C: ConfigSpace>(
    config: &C,
    selected: Identity,
) -> Result<(&'static [Control], Capabilities), AttachError> {
    let controls = selected.setup.controls();
    match capabilities(config, controls, selected.reset_policy) {
        Err(AttachError::InvalidCapabilities(CapabilityRefusal::ControlOverlap { .. }))
            if selected.setup == Setup::Nvidia =>
        {
            Ok((&[], capabilities(config, &[], selected.reset_policy)?))
        }
        result => Ok((controls, result?)),
    }
}

fn capabilities<C: ConfigSpace>(
    config: &C,
    controls: &[Control],
    reset_policy: ResetPolicy,
) -> Result<Capabilities, AttachError> {
    let mut found = Capabilities::default();
    // A header-only entry below the end of DEVC whose body cannot be sized.
    let mut ambiguous_below_devc = false;
    let refuse = |refusal| Err(AttachError::InvalidCapabilities(refusal));
    let status = config.read16(regs::STATUS)?;
    if status == u16::MAX {
        return refuse(CapabilityRefusal::StatusUnreadable);
    }
    if status & regs::STATUS_CAP_LIST == 0 {
        return if controls.contains(&PCH_SNOOP) {
            refuse(CapabilityRefusal::NoList)
        } else {
            Ok(found)
        };
    }
    // Capability bodies and controls are checked separately so the one owned
    // PCIe Device Control field is allowed without allowing body/body aliases.
    for entry in config.capabilities() {
        let entry = entry?;
        let (offset16, id) = (entry.offset, entry.id);
        let mut owned_pcie_device_control = false;
        let slot = match id {
            CAP_PM => Some(&mut found.pm),
            CAP_MSI => Some(&mut found.msi),
            CAP_MSIX => {
                if config.read16(offset16 + 2)? == u16::MAX {
                    return refuse(CapabilityRefusal::MsixUnreadable { offset: offset16 });
                }
                Some(&mut found.msix)
            }
            CAP_PCIE => {
                if entry.layout != Layout::Sized {
                    return refuse(CapabilityRefusal::PcieLayout { offset: offset16 });
                }
                let kind = (config.read16(offset16 + 2)? >> 4) & 15;
                owned_pcie_device_control =
                    offset16 + 8 == SCH_HDA_DEVC && matches!(kind, 0 | 1 | 9);
                Some(&mut found.pcie)
            }
            // Unknown bodies are not touched; the walk reserves their headers.
            _ => None,
        };
        let start = usize::from(offset16);
        let end = start + usize::from(entry.bytes);
        if entry.layout != Layout::Sized && start < usize::from(SCH_HDA_DEVC + 2) {
            ambiguous_below_devc = true;
        }
        if reset_policy == ResetPolicy::IntelClockGate
            && ((start < usize::from(CGCTL + 4) && usize::from(CGCTL) < end)
                // An unknown body's end cannot prove vendor-control ownership.
                || (!matches!(id, CAP_PM | CAP_MSI | CAP_MSIX | CAP_PCIE)
                    && start < usize::from(SCH_HDA_DEVC + 2)))
        {
            return refuse(CapabilityRefusal::ClockGateOverlap {
                offset: offset16,
                id,
            });
        }
        for control in controls {
            let range = control.extent();
            if range.start < end
                && start < range.end
                && !(owned_pcie_device_control && *control == PCH_SNOOP)
            {
                return refuse(CapabilityRefusal::ControlOverlap {
                    control: range.start as u16,
                    offset: offset16,
                    id,
                });
            }
        }
        if let Some(slot) = slot {
            if slot.is_some() {
                return refuse(CapabilityRefusal::Duplicate {
                    offset: offset16,
                    id,
                });
            }
            *slot = Some(offset16);
        }
    }
    // With a PCIe capability, PCH78 is its Device Control, so require the
    // known endpoint owner at 0x70. Chipset-integrated PCH functions (Tiger
    // Lake 43c8 and others) list no PCIe capability at all; there 0x78 is
    // the vendor DEVC register that Linux azx_init_pci writes for every
    // AZX_SNOOP_TYPE_SCH controller. That is only accepted when no
    // header-only capability could hide a body reaching 0x78; sized bodies
    // that alias it were already refused above.
    if controls.contains(&PCH_SNOOP) {
        let owned = match found.pcie {
            Some(offset) => offset == SCH_HDA_DEVC - 8,
            None => !ambiguous_below_devc,
        };
        if !owned {
            return refuse(CapabilityRefusal::PcieOwnerMissing { found: found.pcie });
        }
    }
    Ok(found)
}

fn delay(timer: &impl Timer, us: u64) -> Result<(), AttachError> {
    timer.check().map_err(AttachError::MinimumDelay)?;
    timer.wait_us(us).map_err(AttachError::MinimumDelay)
}

fn bar_matches<C: ConfigSpace>(
    config: &C,
    base: u64,
    wide: bool,
) -> Result<bool, crate::bus::pci::ConfigError> {
    let low = config.read32(regs::BASE_ADDRESS_0)?;
    let expected_type = if wide { 4 } else { 0 };
    if low & 15 != expected_type {
        return Ok(false);
    }
    let actual = u64::from(low & !15)
        | if wide {
            u64::from(config.read32(regs::BASE_ADDRESS_0 + 4)?) << 32
        } else {
            0
        };
    Ok(actual == base)
}

fn validated_bar<C: ConfigSpace>(config: &C, bar0: Bar) -> Result<(u64, u64, bool), AttachError> {
    let (base, bytes, wide) = match bar0 {
        Bar::Memory {
            address,
            size,
            prefetchable: false,
            wide,
        } if address != 0
            && address & 0x7f == 0
            && size >= 0x440
            && size.is_power_of_two()
            && address & (size - 1) == 0
            && address.checked_add(size).is_some() =>
        {
            (address, size, wide)
        }
        _ => return Err(AttachError::InvalidBar),
    };
    if !bar_matches(config, base, wide)? {
        return Err(AttachError::InvalidBar);
    }
    Ok((base, bytes, wide))
}

/// Prepare an already mapped, D0, decoded and quiescent conventional function.
/// Modern Intel identities require this path: read-only PCI/MMIO ownership
/// preflight precedes every PCI mutation. No temporary power/decode operation
/// is used to inspect an unknown DSP owner. Legacy `prepare` refuses these IDs.
/// For the cold entry, [`wake_cold`] may first bring such a function to D0
/// with memory decoding on, as Linux does for the legacy class, and changes
/// nothing else.
/// The returned adapter keeps the exact PCI function and mandatory reset
/// policy; use its `config()` when constructing the controller.
///
/// # Safety
/// All `prepare` platform ownership, coherent DMA and mapping prerequisites
/// apply. Additionally, the function must already be in D0 with memory
/// decode enabled, and `registers` must map exactly its sized BAR0 uncached with
/// native widths. Keep that mapping alive on failure as well as success. The
/// platform must have quiesced all prior DMA and excluded firmware/AML/SMM/DSP
/// owners before this read-only inspection; observation cannot grant ownership.
pub unsafe fn prepare_with_registers<'a, C: ConfigSpace, R: Registers>(
    function: &'a mut Function<C>,
    bar0: Bar,
    timer: &impl Timer,
    ownership: Ownership,
    registers: R,
) -> Result<PciRegisters<R, &'a mut Function<C>>, AttachError> {
    // SAFETY: forwarded complete original platform and stopped-DMA contract.
    unsafe { prepare_mapped(function, bar0, timer, ownership, registers, true) }
}

/// Prepare conventional PCI power/decode/snoop without enabling bus mastering.
/// This deliberately permits inherited conventional stream RUN state so the
/// owning driver can stop it under its original root before new DMA binding.
///
/// # Safety
/// ExclusiveLegacy ownership, real original config/BAR/root/backing, D0/decode
/// and firmware/DSP/AML/SMM exclusion from prepare_with_registers apply. Prior
/// DMA backing remains pinned; no root/private-domain migration is yet allowed.
/// An owning lifecycle must retain both arguments on every error or unwind.
pub unsafe fn prepare_cold_with_registers<'a, C: ConfigSpace, R: Registers>(
    function: &'a mut Function<C>,
    bar0: Bar,
    timer: &impl Timer,
    ownership: Ownership,
    registers: R,
) -> Result<PciRegisters<R, &'a mut Function<C>>, AttachError> {
    // SAFETY: forwarded original-owner cold containment prerequisites.
    unsafe { prepare_mapped(function, bar0, timer, ownership, registers, false) }
}

/// What [`wake_cold`] changed, for the boot log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Wake {
    /// The PCI power state the function was found in (1, 2 or 3 for D1, D2
    /// or D3hot) when it had to enter D0; `None` when it was already in D0.
    pub from_state: Option<u8>,
    /// Memory decoding was off and has been turned on.
    pub decode_enabled: bool,
}

/// What the wake did, in words, for a log line about a later failure.
impl core::fmt::Display for Wake {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if let Some(state) = self.from_state {
            let state = match state {
                3 => "D3hot",
                2 => "D2",
                _ => "D1",
            };
            write!(f, "woke it from {state} to D0")?;
            if self.decode_enabled {
                f.write_str(" and turned on its memory decoding")?;
            }
            Ok(())
        } else if self.decode_enabled {
            f.write_str("turned on its memory decoding")
        } else {
            f.write_str("changed nothing")
        }
    }
}

/// Bring a conventional function that firmware left in D1, D2 or D3hot, or
/// with memory decoding off, to D0 with memory decoding on, so that
/// `prepare_cold_with_registers` can inspect BAR0. Firmware that never used
/// the controller may leave it either way. Linux `pci_enable_device` does the
/// same before `azx_probe` reads the controller: `pci_set_power_state` enters
/// D0, then `pci_enable_resources` turns memory decoding on.
///
/// The sequence is legacy `prepare`'s: identity, subsystem, BAR and
/// capability checks before any write; decoding and mastering off; the BARs
/// saved; PMCSR D0 with PME disabled and pending PME kept; the PCI PM settle
/// time (10 ms from D3hot, 200 us from D2); a D0 readback; the saved BARs
/// written back when a D3hot exit without No_Soft_Reset cleared them; then
/// memory decoding on, with bus mastering left off and INTx disabled for the
/// cold entry.
///
/// Identities that require mapped preflight (modern Intel) get a gentler
/// form: bus mastering and INTx are left as found, and a function found with
/// bus mastering on gets no write, so the cold entry still refuses it when it
/// is not ready. Stela admits these only with the legacy class 0x040300,
/// which Linux `snd_intel_dsp_driver_probe` sends to the legacy HDA driver
/// (0x040100 and 0x040380 mean a DSP, for SOF or SST), and `azx_create` then
/// calls `pcim_enable_device` before it maps BAR0 or reads GCAP. The
/// read-only mapped preflight that follows still refuses a controller that a
/// processing owner holds. A function already in D0 with memory decoding on
/// gets no write either.
///
/// # Safety
/// The platform ownership prerequisites of `prepare_cold_with_registers`
/// apply, except D0 and memory decoding, which this establishes. Nothing may
/// access the function's BAR0 mapping before this returns `Ok`.
pub unsafe fn wake_cold<C: ConfigSpace>(
    function: &mut Function<C>,
    bar0: Bar,
    timer: &impl Timer,
) -> Result<Wake, AttachError> {
    timer.check().map_err(AttachError::MinimumDelay)?;
    let selected = identify(function)?;
    // Modern Intel: no bus mastering or INTx change (see above).
    let gentle = selected.requires_mapped_preflight();
    let config = function.config();
    let id = config.read32(regs::VENDOR_ID)?;
    let class = config.read32(regs::CLASS_REVISION)?;
    let actual = identity(
        id as u16,
        (id >> 16) as u16,
        [(class >> 24) as u8, (class >> 16) as u8, (class >> 8) as u8],
    )?;
    if actual != selected || config.read8(regs::HEADER_TYPE)? & 0x7f != 0 {
        return Err(AttachError::IdentityChanged);
    }
    check_subsystem(config, selected, BoardIdentity::Unknown)?;
    let (base, _, wide) = validated_bar(config, bar0)?;
    let (_, caps) = controls_and_capabilities(config, selected)?;
    // All ones: the function does not answer (D3cold), as the cold entry says.
    let command = config.read16(regs::COMMAND)?;
    if command == u16::MAX {
        return Err(AttachError::PowerNotReady);
    }
    let mut sleeping = None;
    if let Some(pm) = caps.pm {
        let version = config.read16(pm + 2)? & 7;
        let value = config.read16(pm + 4)?;
        if value == u16::MAX {
            return Err(AttachError::PowerNotReady);
        }
        if !(1..=3).contains(&version) {
            return Err(AttachError::UnsupportedPowerManagement);
        }
        if value & PM_STATE != 0 {
            sleeping = Some((pm + 4, value));
        }
    }
    let decode_off = command & regs::COMMAND_MEMORY == 0;
    if sleeping.is_none() && !decode_off {
        return Ok(Wake::default());
    }
    // Something may own a modern Intel function that is bus mastering, so
    // it is left to the cold entry's refusal rather than turned off.
    if gentle && command & regs::COMMAND_MASTER != 0 {
        return Ok(Wake::default());
    }
    // Save the BARs before a D3hot exit can reset them. No BAR or PMCSR
    // write happens with a live decoder or bus master.
    let mut bars = [0; 6];
    for (index, bar) in bars.iter_mut().enumerate() {
        *bar = config.read32(regs::BASE_ADDRESS_0 + index as u16 * 4)?;
    }
    all_ones_bar("wake: BAR save", &bars)?;
    let quiet = if gentle {
        command & !regs::COMMAND_MEMORY
    } else {
        command & !(regs::COMMAND_MEMORY | regs::COMMAND_MASTER) | regs::COMMAND_INTX_DISABLE
    };
    if quiet != command {
        config.write16(regs::COMMAND, quiet)?;
    }
    let observed = config.read16(regs::COMMAND)?;
    if observed == u16::MAX || observed & (regs::COMMAND_MEMORY | regs::COMMAND_MASTER) != 0 {
        return Err(AttachError::rejected(
            "wake: decoding and bus mastering off",
            regs::COMMAND,
            Some(quiet.into()),
            observed.into(),
        ));
    }
    let mut wake = Wake::default();
    if let Some((offset, value)) = sleeping {
        let state = value & PM_STATE;
        // PME_Status is W1C: write zero to preserve pending wake evidence.
        config.write16(offset, value & !(PM_STATE | PM_PME_ENABLE | PM_PME_STATUS))?;
        delay(
            timer,
            match state {
                3 => 10_000,
                2 => 200,
                _ => 0,
            },
        )?;
        let observed = config.read16(offset)?;
        if observed == u16::MAX || observed & PM_STATE != 0 {
            return Err(AttachError::PowerTransition);
        }
        if state == 3 && value & PM_NO_SOFT_RESET == 0 {
            for (index, value) in bars.iter().enumerate() {
                config.write32(regs::BASE_ADDRESS_0 + index as u16 * 4, *value)?;
            }
        }
        wake.from_state = Some(state as u8);
    }
    if !bar_matches(config, base, wide)? {
        return Err(AttachError::InvalidBar);
    }
    let enabled = quiet | regs::COMMAND_MEMORY;
    config.write16(regs::COMMAND, enabled)?;
    let observed = config.read16(regs::COMMAND)?;
    if observed == u16::MAX
        || observed & (regs::COMMAND_MEMORY | regs::COMMAND_MASTER) != regs::COMMAND_MEMORY
    {
        return Err(AttachError::rejected(
            "wake: decoding on",
            regs::COMMAND,
            Some(enabled.into()),
            observed.into(),
        ));
    }
    wake.decode_enabled = decode_off;
    Ok(wake)
}

/// The first saved BAR that reads all ones stops preparation: the function
/// may not answer, and restoring that value after a D3hot exit would be wrong.
fn all_ones_bar(step: &'static str, bars: &[u32; 6]) -> Result<(), AttachError> {
    match bars.iter().position(|bar| *bar == u32::MAX) {
        Some(index) => Err(AttachError::rejected(
            step,
            regs::BASE_ADDRESS_0 + index as u16 * 4,
            None,
            u32::MAX,
        )),
        None => Ok(()),
    }
}

/// For an identity whose vendor bytes Linux writes without reading them back
/// (Zen-family AMD), the first such byte that does not hold the bits
/// preparation wrote. Read-only, for the boot log once preparation succeeded.
pub fn unheld_control<C: ConfigSpace>(
    function: &mut Function<C>,
) -> Result<Option<Unheld>, AttachError> {
    let selected = identify(function)?;
    let config = function.config();
    for control in selected.setup.controls() {
        if let Control::Lenient {
            offset,
            mask,
            value,
        } = *control
        {
            let read = config.read8(offset)?;
            if read & mask != value {
                return Ok(Some(Unheld {
                    offset,
                    wanted: value,
                    read,
                }));
            }
        }
    }
    Ok(None)
}

unsafe fn prepare_mapped<'a, C: ConfigSpace, R: Registers>(
    function: &'a mut Function<C>,
    bar0: Bar,
    timer: &impl Timer,
    ownership: Ownership,
    mut registers: R,
    stopped: bool,
) -> Result<PciRegisters<R, &'a mut Function<C>>, AttachError> {
    use super::registers::{
        COMMAND_RUN, CORBCTL, GCAP, RIRBCTL, SD_CTL, SD_RUN, VMAJ, VMIN, stream_base,
    };
    use crate::Error;
    timer.check().map_err(AttachError::MinimumDelay)?;
    if ownership != Ownership::ExclusiveLegacy {
        return Err(AttachError::Ownership);
    }
    let selected = identify(function)?;
    let config = function.config();
    let id = config.read32(regs::VENDOR_ID)?;
    let class = config.read32(regs::CLASS_REVISION)?;
    let actual = identity(
        id as u16,
        (id >> 16) as u16,
        [(class >> 24) as u8, (class >> 16) as u8, (class >> 8) as u8],
    )?;
    if actual != selected || config.read8(regs::HEADER_TYPE)? & 0x7f != 0 {
        return Err(AttachError::IdentityChanged);
    }
    check_subsystem(config, selected, BoardIdentity::Unknown)?;
    let (_, bytes, _) = validated_bar(config, bar0)?;
    if registers.region_bytes() < 0x440 || registers.region_bytes() as u64 > bytes {
        return Err(AttachError::InvalidBar);
    }
    let (_, caps) = controls_and_capabilities(config, selected)?;
    let command = config.read16(regs::COMMAND)?;
    if command == u16::MAX || command & regs::COMMAND_MEMORY == 0 {
        return Err(AttachError::PowerNotReady);
    }
    if let Some(pm) = caps.pm {
        let version = config.read16(pm + 2)? & 7;
        let state = config.read16(pm + 4)?;
        if !(1..=3).contains(&version) || state == u16::MAX || state & PM_STATE != 0 {
            return Err(AttachError::PowerNotReady);
        }
    }
    if selected.reset_policy == ResetPolicy::IntelClockGate && config.read32(CGCTL)? == u32::MAX {
        return Err(AttachError::rejected(
            "clock gate read",
            CGCTL,
            None,
            u32::MAX,
        ));
    }
    let (major, minor) = (registers.read8(VMAJ), registers.read8(VMIN));
    if major != 1 || minor != 0 {
        return Err(AttachError::MmioPreflight(Error::UnsupportedVersion {
            major,
            minor,
        }));
    }
    let gcap = registers.read16(GCAP);
    let total = ((gcap >> 8) & 15) + ((gcap >> 12) & 15) + ((gcap >> 3) & 31);
    if total == 0 || total > 30 {
        return Err(AttachError::MmioPreflight(Error::UnsupportedTopology));
    }
    for offset in [CORBCTL, RIRBCTL]
        .into_iter()
        .chain((0..total as u8).map(|index| stream_base(index) + SD_CTL))
    {
        let value = registers.read8(offset);
        if value == u8::MAX {
            return Err(AttachError::MmioPreflight(Error::RegisterUnavailable {
                offset,
            }));
        }
        let run = if matches!(offset, CORBCTL | RIRBCTL) {
            COMMAND_RUN
        } else {
            SD_RUN
        };
        if stopped && value & run != 0 {
            return Err(AttachError::ActiveDma { offset });
        }
    }
    if selected.reset_policy == ResetPolicy::IntelClockGate {
        let extended = super::extended::inspect(&mut registers, total as u8)
            .map_err(AttachError::MmioPreflight)?;
        extended
            .preflight_clock(&mut registers)
            .map_err(AttachError::MmioPreflight)?;
    }
    // SAFETY: the caller supplies exclusive platform ownership, power and a
    // matching register mapping. Preflight checked D0 and the identity-selected
    // link controls. The legacy entry requires stopped DMA; the cold entry
    // leaves BME clear for the owning inherited-engine stop before root bind.
    let prepared = unsafe {
        prepare_inner(
            function,
            bar0,
            timer,
            ownership,
            BoardIdentity::Unknown,
            if stopped {
                Preparation::Mapped
            } else {
                Preparation::ColdMapped
            },
        )
    }?;
    // SAFETY: preparation and the register implementation refer to
    // the same BAR/function checked above. Moving registers into the adapter
    // keeps that mapping while function remains exclusively borrowed.
    unsafe { prepared.bind_registers(function, registers) }
}

/// Enter PCI D0, select the identity's conventional coherent DMA behavior,
/// enable memory/mastering, and disable all PCI interrupt delivery.
///
/// `bar0` must be the BAR0 sized by the platform while the function is
/// exclusively owned. It must be nonprefetchable memory, at least 0x440 bytes.
/// This function never sizes BARs or guesses addresses. Failure after a config
/// write leaves ownership with the caller, with decoding/mastering disabled
/// where possible; the caller must not free DMA or mapping resources blindly.
///
/// # Safety
/// The platform has made main power/clocks available (including D3cold/ACPI
/// resources), quiesced every prior DMA engine, and established exclusive
/// ownership against firmware, AML, SMM, DSP and other CPUs. `ExclusiveLegacy`
/// certifies that decision; the enum cannot discover it. Configuration space
/// must use native byte/word stores. No-PM-capability functions must already be
/// configuration-ready in D0. The supplied allocator used subsequently
/// must provide coherent, controller-reachable DMA through the IOMMU.
/// No later agent may change power, decode, snoop or controller ownership.
pub unsafe fn prepare<C: ConfigSpace>(
    function: &mut Function<C>,
    bar0: Bar,
    timer: &impl Timer,
    ownership: Ownership,
) -> Result<Prepared, AttachError> {
    // SAFETY: the caller has powered and quiesced this exclusively owned
    // function and supplies its sized BAR and coherent DMA prerequisites.
    // Unknown board evidence adds no target-specific grant.
    unsafe { prepare_with_board(function, bar0, timer, ownership, BoardIdentity::Unknown) }
}

/// Prepare with target SMBIOS board evidence for conditional exclusions.
/// `prepare` supplies Unknown and refuses ambiguous subsystem identities.
///
/// # Safety
/// All platform/power/ownership/coherent-DMA prerequisites of `prepare` apply.
/// Known board strings must come from this target's firmware SMBIOS,
/// never a guessed machine name, the development host, or a PCI-ID inference.
pub unsafe fn prepare_with_board<C: ConfigSpace>(
    function: &mut Function<C>,
    bar0: Bar,
    timer: &impl Timer,
    ownership: Ownership,
    board: BoardIdentity<'_>,
) -> Result<Prepared, AttachError> {
    // SAFETY: the caller keeps exclusive PCI/platform ownership, stable
    // power and coherent reachable DMA, and supplies firmware-derived board
    // identity. This path performs no mapped-register preflight.
    unsafe {
        prepare_inner(
            function,
            bar0,
            timer,
            ownership,
            board,
            Preparation::Unmapped,
        )
    }
}

enum Preparation {
    Unmapped,
    Mapped,
    ColdMapped,
}

unsafe fn prepare_inner<C: ConfigSpace>(
    function: &mut Function<C>,
    bar0: Bar,
    timer: &impl Timer,
    ownership: Ownership,
    board: BoardIdentity<'_>,
    preparation: Preparation,
) -> Result<Prepared, AttachError> {
    timer.check().map_err(AttachError::MinimumDelay)?;
    if ownership != Ownership::ExclusiveLegacy {
        return Err(AttachError::Ownership);
    }
    let selected = identify(function)?;
    let address = function.address;
    let config = function.config();
    let id = config.read32(regs::VENDOR_ID)?;
    let class = config.read32(regs::CLASS_REVISION)?;
    let actual = identity(
        id as u16,
        (id >> 16) as u16,
        [(class >> 24) as u8, (class >> 16) as u8, (class >> 8) as u8],
    )?;
    if actual != selected || config.read8(regs::HEADER_TYPE)? & 0x7f != 0 {
        return Err(AttachError::IdentityChanged);
    }
    if selected.requires_mapped_preflight() && matches!(preparation, Preparation::Unmapped) {
        return Err(AttachError::MappedPreflightRequired);
    }
    check_subsystem(config, actual, board)?;
    let (base, bytes, wide) = validated_bar(config, bar0)?;
    let (controls, caps) = controls_and_capabilities(config, selected)?;
    let pmcsr = if let Some(pm) = caps.pm {
        let version = config.read16(pm + 2)? & 7;
        Some((pm + 4, version, config.read16(pm + 4)?))
    } else {
        None
    };
    if let Some((_, version, value)) = pmcsr
        && (!(1..=3).contains(&version) || value == u16::MAX)
    {
        return Err(AttachError::UnsupportedPowerManagement);
    }
    // Save resources before a D3hot exit can reset them. Disable decode/mastering
    // before restoring any BAR; PCI status is never written by command updates.
    let mut bars = [0; 6];
    for (index, bar) in bars.iter_mut().enumerate() {
        *bar = config.read32(regs::BASE_ADDRESS_0 + index as u16 * 4)?;
    }
    all_ones_bar("BAR save", &bars)?;
    let command = config.read16(regs::COMMAND)?;
    if command == u16::MAX {
        return Err(AttachError::rejected(
            "command read",
            regs::COMMAND,
            None,
            0xffff,
        ));
    }
    let command = command & !(regs::COMMAND_MEMORY | regs::COMMAND_MASTER);
    config.write16(regs::COMMAND, command | regs::COMMAND_INTX_DISABLE)?;
    let result = (|| {
        let observed = config.read16(regs::COMMAND)?;
        if observed & (regs::COMMAND_MEMORY | regs::COMMAND_MASTER) != 0 {
            return Err(AttachError::rejected(
                "decoding and bus mastering off",
                regs::COMMAND,
                Some((command | regs::COMMAND_INTX_DISABLE).into()),
                observed.into(),
            ));
        }
        if let Some((offset, _, value)) = pmcsr {
            let state = value & PM_STATE;
            // PME_Status is W1C: write zero to preserve pending wake evidence.
            config.write16(offset, value & !(PM_STATE | PM_PME_ENABLE | PM_PME_STATUS))?;
            delay(
                timer,
                match state {
                    3 => 10_000,
                    2 => 200,
                    _ => 0,
                },
            )?;
            if config.read16(offset)? & PM_STATE != 0 {
                return Err(AttachError::PowerTransition);
            }
            if state == 3 && value & PM_NO_SOFT_RESET == 0 {
                config.write16(regs::COMMAND, command | regs::COMMAND_INTX_DISABLE)?;
                for (index, value) in bars.iter().enumerate() {
                    config.write32(regs::BASE_ADDRESS_0 + index as u16 * 4, *value)?;
                }
            }
        }
        if !bar_matches(config, base, wide)? {
            return Err(AttachError::InvalidBar);
        }
        // The exact identity selects ICH TCSEL, PCH TCSEL+word snoop, or
        // ATI/AMD byte snoop. These fields and access widths are not interchangeable.
        for control in controls {
            control.apply(config)?;
        }
        for (offset, enable) in [(caps.msi, 1), (caps.msix, 1 << 15)] {
            if let Some(offset) = offset {
                let previous = config.read16(offset + 2)?;
                if previous == u16::MAX {
                    return Err(AttachError::rejected("MSI off", offset + 2, None, 0xffff));
                }
                config.write16(offset + 2, previous & !enable)?;
                let observed = config.read16(offset + 2)?;
                if observed & enable != 0 {
                    return Err(AttachError::rejected(
                        "MSI off",
                        offset + 2,
                        Some((previous & !enable).into()),
                        observed.into(),
                    ));
                }
            }
        }
        let enabled = command
            | regs::COMMAND_MEMORY
            | regs::COMMAND_INTX_DISABLE
            | if matches!(preparation, Preparation::ColdMapped) {
                0
            } else {
                regs::COMMAND_MASTER
            };
        config.write16(regs::COMMAND, enabled)?;
        let readback = config.read16(regs::COMMAND)?;
        if readback == u16::MAX
            || readback & (regs::COMMAND_MEMORY | regs::COMMAND_MASTER | regs::COMMAND_INTX_DISABLE)
                != enabled
                    & (regs::COMMAND_MEMORY | regs::COMMAND_MASTER | regs::COMMAND_INTX_DISABLE)
        {
            return Err(AttachError::rejected(
                "decoding on",
                regs::COMMAND,
                Some(enabled.into()),
                readback.into(),
            ));
        }
        Ok(Prepared {
            identity: selected,
            address,
            base,
            bytes,
        })
    })();
    // Failure may follow a real config or minimum-delay effect. Preserve its
    // exact cause and original owning graph; do not issue another write as
    // though that effect had been rolled back. ColdMapped never enables BME.
    result
}

#[cfg(test)]
pub(super) mod tests;

impl From<crate::bus::pci::ConfigError> for AttachError {
    fn from(error: crate::bus::pci::ConfigError) -> Self {
        Self::Config(error)
    }
}
impl From<crate::bus::pci::CapabilityError> for AttachError {
    fn from(error: crate::bus::pci::CapabilityError) -> Self {
        Self::PciCapability(error)
    }
}
