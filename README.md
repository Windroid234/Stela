<h1 align="center">
  <img src="assets/stela-icon.png" alt="" width="96"><br>
  Stela
</h1>

A translation-based Apple Silicon layer for x86-64 PCs. It is a UEFI boot
program with an arm64-to-x86-64 translator, a Metal driver for NVIDIA GPUs and
a model of Apple silicon hardware. Stela currently presents itself as an M2 Mac mini.

> [!NOTE]
> **About this fork.** This is a fork of Stela by Tyler ([tylertms/Stela](https://github.com/tylertms/Stela), now archived) with mods and fixes on top. On 22 November 2026 Tyler removed the original repository, whose README now says Tyler does not endorse, and is not affiliated with, any forks or continuations. That includes this one, so please don't contact Tyler about it: report problems here instead.
>
> | Mod | What it does | Status |
> |---|---|---|
> | 🔊 **Audio bridge** | Plays macOS's sound through the PC's own HD Audio: line-out, speakers or headphones | 🧪 In [test build 3](https://github.com/reece-builds/Stela-Mods/releases/tag/v0.5.102). Nobody has heard sound from it yet |
> | ⏰ **Sound chip wake-up** | Starts an onboard sound chip that the PC's firmware left asleep, the way Linux does. It fixes `HD Audio native: Pci(PowerNotReady)` | 🧪 In [test build 3](https://github.com/reece-builds/Stela-Mods/releases/tag/v0.5.102) |
> | 🔵 **Raptor Lake sound fix** | Lets Intel 600 and 700-series sound chips start. Made by [justpainful](https://github.com/justpainful) | ✅ In `main`, coming in the next test build |
> | 🌙 **Newer Intel wake-up** | The same wake-up for newer Intel sound chips (6th-gen Core and up), done gently: it only powers the chip on and turns on its memory access, like Linux, and leaves the chip alone if something is using it | ✅ In `main`, coming in the next test build |
> | 🛑 **GPU check** | Install macOS stops before erasing anything if the PC's GPU can't run macOS, instead of finishing a long install that can never start | ✅ In `main`, coming in the next test build |
> | 🧰 **AMD sound fix** | Newer AMD sound chips (Ryzen) get their memory "snoop" setting the way Linux sets it, instead of refusing to start when it doesn't stick. When a sound chip still won't start, the boot log now says which step failed and what the chip answered | ✅ In `main`, coming in the next test build |
>
> - 🧪 **Help test it:** if you have a PC with a GTX 16 or RTX GPU, see [Testers wanted](https://github.com/reece-builds/Stela-Mods/issues/3).
> - ❌ **GTX 10 (Pascal) GPUs are not supported.** An earlier version of this fork said they were, which was wrong: Pascal needs NVIDIA firmware that is signature-locked, as Tyler explained before the original repository was removed. The earlier GTX 10 downloads were broken and have been removed.

> [!TIP]
> **🏆 Testers hall of fame**
> - [**FASTDROPX**](https://github.com/FASTDROPX): the first real-hardware test, on test build 1. Their boot log found the bug that stopped Stela's sound chip from starting.
> - [**NikNikovsky**](https://github.com/NikNikovsky): tested on an RTX 3070 with an Intel 600-series board. Their test build 3 log showed the Intel sound chip check that justpainful's fix covers.
> - [**FlookyPupper**](https://github.com/FlookyPupper): tested test build 3 on a Ryzen 5 5600GT with an RTX 3060. Their log led to the AMD sound fix and the clearer sound errors.
>
> Your name could be next: see [Testers wanted](https://github.com/reece-builds/Stela-Mods/issues/3) 🙌

> [!WARNING]
Stela is still in development. It is slow and still may not boot on supported hardware. Fixes are on the way, please submit a GitHub Issue with your boot logs if you encounter an error!

## Translator

An ahead-of-time and just-in-time compiler translates arm64e code to x86-64.
It covers more than 3,000 AArch64 instruction encodings, maps guest registers
to host registers and caches translated code.

## NVIDIA driver

The Metal driver compiles AIR, Metal's shader IR, directly to NIR, Mesa's IR,
and supports the Metal3 and Apple7 GPU families.

## Hardware requirements

| Part | Requirement |
|---|---|
| CPU | Intel or AMD x86-64-v2 (Intel Nehalem, AMD Bulldozer or newer). AVX2 and AVX-512 are used when present. |
| Memory | 8 GB or more. |
| Firmware | 64-bit UEFI with Secure Boot off. |
| GPU | NVIDIA GeForce GTX 16 or RTX 20, 30, 40 or 50 series. GTX 10 series (Pascal) is not supported: it needs signature-enforced NVIDIA firmware. |
| Disk | NVMe, or SATA in AHCI mode. 60 GB or more for macOS: a GPT partition or a whole disk to erase. |
| USB stick | 8 GB for the Online installer, 32 GB for the Offline installer. |
| Ethernet | Intel I225, I226, I210, I211, I350, 82574L, 82575, 82576, 82579, 82580, I217, I218, I219.<br>Realtek RTL8111, RTL8168, RTL8169, RTL8411, RTL8125, RTL8126, Killer E2600, E3000.<br>Broadcom BCM5720, BCM5761, BCM5762.<br>Aquantia AQC100, AQC107, AQC108, AQC109, AQC111, AQC112, AQC113, AQC114, AQC115C, AQC116C.<br>Qualcomm Atheros AR8131, AR8132, AR8151, AR8152, AR8161, AR8162, AR8171, AR8172, Killer E2200, E2400, E2500.<br>USB: Realtek RTL8152, RTL8153, RTL8156, ASIX AX88179, AX88178A, AX88772, CDC ECM and NCM. |
| Wi-Fi | None currently supported. |
| Bluetooth | None currently supported. |
| Audio | Experimental: the guest's USB speaker plays through the PC's HD Audio analog output (line-out, speakers or headphones). Not yet confirmed working on real hardware. |
| USB | xHCI (USB 3) and EHCI controllers, hubs, USB disks. |
| Input | USB keyboards and mice. PS/2 keyboards, mice and Synaptics or Elantech touchpads. |

## Install

1. Download `stela` from this fork's
   [releases page](https://github.com/reece-builds/Stela-Mods/releases):
   take the newest test build.
2. Run `sudo ./stela` (Linux, macOS) or double-click `stela.exe`
   (Windows).
3. Choose the USB stick and the installer, then confirm. This erases the USB
   stick.
4. Boot the PC from the USB stick.

## Update

Download the new `stela`, plug the USB stick in and run:

```sh
sudo ./stela update
```

On Windows, open Terminal with Run as administrator and run:

```
.\stela.exe update
```

## Logs

If Stela does not work, plug the USB stick into any computer with `stela` and
save the boot log. Include `log.txt` in your GitHub Issue. Save it before you
start Stela again many times: the log keeps the start of each of the last
eight boots, but only the latest part of a long run.

```sh
sudo ./stela log --output log.txt
```

On Windows, open Terminal with Run as administrator and run:

```
.\stela.exe log --output log.txt
```

## Build

You need [Rust](https://rustup.rs), git, Python 3.10 or newer, and LLVM 21,
22 or 23 with Clang, LLD and libclang. On macOS, Xcode's Apple clang 21 or
newer also works.

```sh
git clone https://github.com/reece-builds/Stela-Mods
cd Stela-Mods
cargo xtask dist
```

This writes `target/release/stela`. The first build fetches and compiles
Linux, Mesa and LLVM sources and takes a long time.

## Legal

Stela contains no Apple software. You are responsible for following Apple's
software license agreement and terms of use.

Stela's code is GPL-2.0-or-later. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

## Community

Questions, test results and bug reports for this fork go in its
[GitHub Issues](https://github.com/reece-builds/Stela-Mods/issues).
