// SPDX-License-Identifier: GPL-2.0-only
//! Cold wake: firmware left the controller in D1 to D3hot or with memory
//! decoding off, as boot logs show for AMD 1022:1487 and 1022:15e3. Modern
//! Intel gets the gentle form: D0 and memory decoding only.
use super::*;
use crate::bus::pci::regs;

const DECODE: u16 = regs::COMMAND_MEMORY;
const MASTER: u16 = regs::COMMAND_MASTER;
const INTX: u16 = regs::COMMAND_INTX_DISABLE;

fn amd(state: u16, command: u16) -> (Function<Model>, Clock, Bar) {
    let (mut function, timer, bar) = fixture(state);
    set_identity(&mut function, AMD, 0x1487);
    function.config().set16(regs::COMMAND, command);
    (function, timer, bar)
}

fn modern(device: u16, pmcsr: u16, command: u16) -> (Function<Model>, Clock, Bar) {
    let (mut function, timer, bar) = modern_pci_model_for(device);
    function
        .config()
        .set16(0x54, pmcsr | PM_PME_ENABLE | PM_PME_STATUS);
    function.config().set16(regs::COMMAND, command);
    (function, timer, bar)
}

fn wake_model(
    function: &mut Function<Model>,
    timer: &Clock,
    bar: Bar,
) -> Result<Wake, AttachError> {
    // SAFETY: the isolated stateful model has no real hardware/firmware owner.
    unsafe { wake_cold(function, bar, timer) }
}

fn cold_model(function: &mut Function<Model>, timer: &Clock, bar: Bar) -> Result<(), AttachError> {
    // SAFETY: The PCI function and mapped byte vector are owned by this test; Rc
    // aliases are used sequentially. No firmware or background DMA can race them.
    unsafe {
        prepare_cold_with_registers(
            function,
            bar,
            timer,
            Ownership::ExclusiveLegacy,
            MappedModel::new(),
        )
    }
    .map(drop)
}

#[test]
fn cold_entry_refuses_a_sleeping_or_undecoded_controller() {
    for (state, command) in [(3, 0x0406), (3, 0), (0, 0)] {
        let (mut function, timer, bar) = amd(state, command);
        assert_eq!(
            cold_model(&mut function, &timer, bar).unwrap_err(),
            AttachError::PowerNotReady,
            "state {state} command {command:#x}"
        );
        assert!(function.config().writes.is_empty());
    }
}

#[test]
fn sleeping_or_undecoded_controller_is_woken_for_the_cold_entry() {
    for (state, command, from_state, decode_enabled) in [
        (3, 0x0406, Some(3), false),
        (3, 0, Some(3), true),
        (2, MASTER, Some(2), true),
        (0, 0, None, true),
    ] {
        let (mut function, timer, bar) = amd(state, command);
        let wake = wake_model(&mut function, &timer, bar).unwrap();
        assert_eq!(
            wake,
            Wake {
                from_state,
                decode_enabled
            },
            "state {state} command {command:#x}"
        );
        let config = function.config();
        let pmcsr = config.get16(0x54);
        assert_eq!(pmcsr & PM_STATE, 0);
        if state != 0 {
            // PME is disabled and pending PME evidence (W1C) is kept.
            assert_eq!(pmcsr & PM_PME_ENABLE, 0);
            assert_ne!(pmcsr & PM_PME_STATUS, 0);
        }
        let command = config.get16(regs::COMMAND);
        assert_eq!(command & (DECODE | MASTER), DECODE);
        assert_ne!(command & regs::COMMAND_INTX_DISABLE, 0);
        // A D3hot exit cleared every BAR; the saved ones are back.
        assert_eq!(config.words[4], 0xfebf_0000);
        assert_eq!(config.words[5], 0x1234_0000);
        match state {
            3 => assert!(timer.now_us() >= 10_000),
            2 => assert!(timer.now_us() >= 200),
            _ => assert_eq!(timer.now_us(), 0),
        }
        cold_model(&mut function, &timer, bar).unwrap();
    }
}

#[test]
fn awake_controller_gets_no_write() {
    let (mut function, timer, bar) = amd(0, 0x0406);
    assert_eq!(
        wake_model(&mut function, &timer, bar).unwrap(),
        Wake::default()
    );
    assert!(function.config().writes.is_empty());
    assert_eq!(timer.now_us(), 0);
}

#[test]
fn modern_intel_awake_or_bus_mastering_gets_no_write() {
    for (pmcsr, command) in [
        (0, DECODE),
        (0, DECODE | MASTER | INTX),
        (3, DECODE | MASTER | INTX),
        (3, MASTER),
        (0, MASTER),
    ] {
        let (mut function, timer, bar) = modern(0x7ad0, pmcsr, command);
        assert_eq!(
            wake_model(&mut function, &timer, bar).unwrap(),
            Wake::default(),
            "pmcsr {pmcsr} command {command:#x}"
        );
        assert!(function.config().writes.is_empty());
        assert_eq!(timer.now_us(), 0);
        if pmcsr != 0 || command & DECODE == 0 {
            // Bus mastering while asleep or undecoded: the cold entry still
            // refuses it before any MMIO read.
            let registers = MappedModel::new();
            // SAFETY: The PCI function and mapped byte vector are owned by this test; Rc
            // aliases are used sequentially. No firmware or background DMA can race them.
            let result = unsafe {
                prepare_cold_with_registers(
                    &mut function,
                    bar,
                    &timer,
                    Ownership::ExclusiveLegacy,
                    registers.clone(),
                )
            };
            assert_eq!(result.err(), Some(AttachError::PowerNotReady));
            assert_eq!(registers.reads.get(), 0);
        }
    }
}

#[test]
fn sleeping_or_undecoded_modern_intel_gets_a_gentle_wake() {
    for &device in INTEL_CLOCK_GATE_IDS {
        for (pmcsr, command, from_state, decode_enabled) in [
            (3, 0, Some(3), true),
            (3, DECODE | INTX, Some(3), false),
            (3 | PM_NO_SOFT_RESET, INTX, Some(3), true),
            (2, INTX, Some(2), true),
            (0, 0, None, true),
            (0, INTX, None, true),
        ] {
            let case = (device, pmcsr, command);
            let (mut function, timer, bar) = modern(device, pmcsr, command);
            assert_eq!(
                wake_model(&mut function, &timer, bar).unwrap(),
                Wake {
                    from_state,
                    decode_enabled
                },
                "{case:x?}"
            );
            let config = function.config();
            let observed = config.get16(0x54);
            assert_eq!(observed & PM_STATE, 0, "{case:x?}");
            if from_state.is_some() {
                // PME is disabled and pending PME evidence (W1C) is kept.
                assert_eq!(observed & PM_PME_ENABLE, 0);
                assert_ne!(observed & PM_PME_STATUS, 0);
            }
            // Decoding on; bus mastering still off and INTx as found.
            assert_eq!(config.get16(regs::COMMAND), command | DECODE, "{case:x?}");
            // Every BAR is in place, restored after a D3hot exit that reset them.
            assert_eq!(config.words[4], 0xfebf_0000);
            assert_eq!(config.words[5], 0x1234_0000);
            // Only COMMAND, PMCSR and saved-BAR writes: no snoop, TCSEL,
            // vendor or clock-gate write precedes the read-only preflight.
            assert!(config.writes.iter().all(|(offset, _, _, _)| {
                *offset == regs::COMMAND
                    || *offset == 0x54
                    || (regs::BASE_ADDRESS_0..regs::BASE_ADDRESS_0 + 24).contains(offset)
            }));
            let restored = config
                .writes
                .iter()
                .any(|(offset, _, _, _)| *offset == regs::BASE_ADDRESS_0);
            assert_eq!(restored, pmcsr == 3, "{case:x?}");
            match pmcsr & PM_STATE {
                3 => assert!(timer.now_us() >= 10_000),
                2 => assert!(timer.now_us() >= 200),
                _ => assert_eq!(timer.now_us(), 0),
            }
            // The read-only mapped preflight now runs and passes.
            cold_model(&mut function, &timer, bar).unwrap();
        }
    }
}

#[test]
fn dsp_class_intel_is_never_woken() {
    for class in [[4, 1, 0], [4, 3, 0x80]] {
        for stale_enumeration in [true, false] {
            let (mut function, timer, bar) = modern(0x7ad0, 3, 0);
            if !stale_enumeration {
                function.class = class;
            }
            function.config().words[2] = (u32::from(class[0]) << 24)
                | (u32::from(class[1]) << 16)
                | (u32::from(class[2]) << 8);
            assert_eq!(
                wake_model(&mut function, &timer, bar).unwrap_err(),
                AttachError::UnsupportedController
            );
            assert!(function.config().writes.is_empty());
        }
    }
}

#[test]
fn failed_modern_intel_wake_leaves_decoding_off() {
    let (mut function, timer, bar) = modern(0x7ad0, 3, INTX);
    function.config().reject_power = true;
    assert_eq!(
        wake_model(&mut function, &timer, bar).unwrap_err(),
        AttachError::PowerTransition
    );
    assert_eq!(function.config().get16(regs::COMMAND), INTX);

    // Decoding that will not turn off stops before any power change.
    let (mut function, timer, bar) = modern(0x7ad0, 3, DECODE);
    function.config().reject_command = true;
    assert_eq!(
        wake_model(&mut function, &timer, bar).unwrap_err(),
        AttachError::rejected(
            "wake: decoding and bus mastering off",
            regs::COMMAND,
            Some(0),
            DECODE.into()
        )
    );
    assert!(
        !function
            .config()
            .writes
            .iter()
            .any(|(offset, _, _, _)| *offset == 0x54)
    );
}

#[test]
fn denylisted_board_is_not_woken() {
    let (mut function, timer, bar) = amd(3, 0);
    // ASUS 1043:874f, on the pinned driver's unconditional denylist.
    function.config().words[11] = 0x874f_1043;
    assert_eq!(
        wake_model(&mut function, &timer, bar).unwrap_err(),
        AttachError::SubsystemExcluded
    );
    assert!(function.config().writes.is_empty());
}

#[test]
fn failed_wake_leaves_decoding_off() {
    let (mut function, timer, bar) = amd(3, 0x0406);
    function.config().reject_power = true;
    assert_eq!(
        wake_model(&mut function, &timer, bar).unwrap_err(),
        AttachError::PowerTransition
    );
    assert_eq!(
        function.config().get16(regs::COMMAND) & (DECODE | MASTER),
        0
    );

    // A command register that ignores writes stops before any power change.
    let (mut function, timer, bar) = amd(3, 0x0406);
    function.config().reject_command = true;
    assert_eq!(
        wake_model(&mut function, &timer, bar).unwrap_err(),
        AttachError::rejected(
            "wake: decoding and bus mastering off",
            regs::COMMAND,
            Some(0x400),
            0x406
        )
    );
    assert!(
        !function
            .config()
            .writes
            .iter()
            .any(|(offset, _, _, _)| *offset == 0x54)
    );
}

#[test]
fn wake_says_what_it_did() {
    for (from_state, decode_enabled, text) in [
        (
            Some(3),
            true,
            "woke it from D3hot to D0 and turned on its memory decoding",
        ),
        (Some(2), false, "woke it from D2 to D0"),
        (Some(1), false, "woke it from D1 to D0"),
        (None, true, "turned on its memory decoding"),
        (None, false, "changed nothing"),
    ] {
        let wake = Wake {
            from_state,
            decode_enabled,
        };
        assert_eq!(alloc::format!("{wake}"), text);
    }
}

#[test]
fn a_rejection_names_its_step_register_and_values() {
    assert_eq!(
        alloc::format!(
            "{:?}",
            AttachError::rejected("wake: decoding on", regs::COMMAND, Some(0x406), 0x404)
        ),
        "ConfigurationRejected(wake: decoding on at 0x04: wanted 0x406, read 0x404)"
    );
    assert_eq!(
        alloc::format!("{:?}", AttachError::rejected("MSI off", 0x62, None, 0xffff)),
        "ConfigurationRejected(MSI off at 0x62: read 0xffff)"
    );
}
