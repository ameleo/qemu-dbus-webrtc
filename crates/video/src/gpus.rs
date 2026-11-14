// SPDX-License-Identifier: MPL-2.0

//! What the host's GPUs encode, as this program uses them: asked to VA-API, GPU by GPU, codec
//! by codec, in each chroma format and depth. A stream goes to the first GPU that encodes it;
//! without one, VP9 is encoded on the CPU.

use std::fmt;

use anyhow::{Result, bail};
use qdw_hw_encoder as hw;

use crate::encode::{Codec, Settings};

/// The codecs this program encodes, in the order it lists them.
const CODECS: [Codec; 3] = [Codec::Av1, Codec::Vp9, Codec::Hevc];

/// The depths a codec has in one chroma format.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Depths {
    pub eight: bool,
    pub ten: bool,
}

impl Depths {
    fn any(self) -> bool {
        self.eight || self.ten
    }
}

impl fmt::Display for Depths {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.eight, self.ten) {
            (true, true) => f.write_str("8, 10 bits"),
            (true, false) => f.write_str("8 bits"),
            (false, true) => f.write_str("10 bits"),
            (false, false) => f.write_str("-"),
        }
    }
}

/// What a GPU encodes in one codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Support {
    pub codec: Codec,
    /// 4:2:0.
    pub reduced: Depths,
    /// 4:4:4.
    pub full: Depths,
}

/// A GPU, and what it encodes.
#[derive(Debug, Clone)]
pub struct Gpu {
    /// Its render node: `/dev/dri/renderD128`.
    pub device: String,
    /// Its driver, its kernel driver and PCI id, when the system says them.
    pub description: String,
    pub codecs: Vec<Support>,
}

/// The kernel driver and PCI id of a render node, from sysfs: `i915, 8086:56a5`.
fn kernel_driver(device: &str) -> String {
    let Some(node) = device.strip_prefix("/dev/dri/") else {
        return String::new();
    };
    let sys = format!("/sys/class/drm/{node}/device");
    let driver = std::fs::read_link(format!("{sys}/driver"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    let id = |file: &str| {
        std::fs::read_to_string(format!("{sys}/{file}"))
            .ok()
            .map(|v| v.trim().trim_start_matches("0x").to_string())
    };
    let pci = id("vendor")
        .zip(id("device"))
        .map(|(v, d)| format!("{v}:{d}"));
    [driver, pci]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a GPU encodes in `codec`, in the formats this program uses.
fn support(gpu: &hw::Gpu, codec: Codec) -> Support {
    let encodes = |full_chroma: bool, ten_bits: bool| {
        (!full_chroma || codec.has_full_chroma())
            && (!ten_bits || codec.has_ten_bits())
            && gpu.encodes(codec.hw(), full_chroma, ten_bits)
    };
    Support {
        codec,
        reduced: Depths {
            eight: encodes(false, false),
            ten: encodes(false, true),
        },
        full: Depths {
            eight: encodes(true, false),
            ten: encodes(true, true),
        },
    }
}

/// The host's GPUs and what each encodes.
pub fn gpus() -> Vec<Gpu> {
    hw::Gpu::all()
        .iter()
        .map(|gpu| {
            let device = gpu.device.display().to_string();
            let kernel = kernel_driver(&device);
            let description = [gpu.driver.trim().to_string(), kernel]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("; ");
            Gpu {
                device,
                description,
                codecs: CODECS.iter().map(|&codec| support(gpu, codec)).collect(),
            }
        })
        .collect()
}

/// What `--codecs` prints: each GPU and what it encodes, with the options that pick it, and
/// the encoding on the CPU.
pub fn report(gpus: &[Gpu]) -> String {
    let mut out = String::new();
    if gpus.is_empty() {
        out += "No GPU that VA-API drives: is a GPU there, with its VA driver? Encoding on the \
                CPU.\n";
    }
    for gpu in gpus {
        let description = if gpu.description.is_empty() {
            String::new()
        } else {
            format!(" ({})", gpu.description)
        };
        out += &format!("{}{description}\n", gpu.device);
        for s in &gpu.codecs {
            let name = s.codec.name();
            let mut ways = Vec::new();
            if s.reduced.any() {
                ways.push(format!("4:2:0 {}", s.reduced));
            }
            if s.full.any() {
                ways.push(format!("4:4:4 {}", s.full));
            }
            let options = if s.full.any() {
                format!("--codec {name} [--chroma 444]")
            } else {
                format!("--codec {name}")
            };
            if ways.is_empty() {
                out += &format!("  {name:5} -\n");
            } else {
                out += &format!("  {name:5} {:34} {options}\n", ways.join(", "));
            }
        }
    }
    out += &format!(
        "CPU\n  vp9   {:34} --software --codec vp9 [--chroma 444]\n",
        "4:2:0 8 bits, 4:4:4 8 bits"
    );
    if !gpus.is_empty() {
        out += "A stream goes to the first GPU that encodes it. 10 bits go to clients that say \
                they decode them (Hello); the others get 8 bits.\n";
    }
    out
}

/// Whether a GPU of the host, or the CPU, encodes `settings`; a message that says what they
/// do encode otherwise.
pub fn check(gpus: &[Gpu], settings: &Settings) -> Result<()> {
    if settings.software {
        if settings.codec != Codec::Vp9 {
            bail!("encoding on the CPU is VP9 only: --codec vp9 (see --codecs)");
        }
        return Ok(());
    }
    if gpus.is_empty() {
        bail!("no GPU that VA-API drives (see --codecs)");
    }
    let chroma = if settings.full_chroma {
        "4:4:4"
    } else {
        "4:2:0"
    };
    let encodes = gpus.iter().any(|gpu| {
        gpu.codecs.iter().any(|s| {
            s.codec == settings.codec
                && if settings.full_chroma {
                    s.full.eight
                } else {
                    s.reduced.eight
                }
        })
    });
    if !encodes {
        bail!(
            "no GPU encodes {} in {chroma} (see --codecs)",
            settings.codec.name()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(codecs: Vec<Support>) -> Gpu {
        Gpu {
            device: "/dev/dri/renderD128".into(),
            description: String::new(),
            codecs,
        }
    }

    const EIGHT: Depths = Depths {
        eight: true,
        ten: false,
    };

    #[test]
    fn settings_are_checked_against_the_gpus() {
        let settings = |codec, full_chroma, software| Settings {
            codec,
            full_chroma,
            bitrate: 1000,
            ten_bits: false,
            software,
            max_bitrate: 1000,
        };
        let gpus = [gpu(vec![
            Support {
                codec: Codec::Av1,
                reduced: EIGHT,
                full: Depths::default(),
            },
            Support {
                codec: Codec::Vp9,
                reduced: EIGHT,
                full: EIGHT,
            },
        ])];
        assert!(check(&gpus, &settings(Codec::Av1, false, false)).is_ok());
        assert!(check(&gpus, &settings(Codec::Av1, true, false)).is_err());
        assert!(check(&gpus, &settings(Codec::Vp9, true, false)).is_ok());
        assert!(check(&gpus, &settings(Codec::Hevc, false, false)).is_err());
        // On the CPU, VP9 only, GPU or not.
        assert!(check(&[], &settings(Codec::Vp9, true, true)).is_ok());
        assert!(check(&[], &settings(Codec::Av1, false, true)).is_err());
        assert!(check(&[], &settings(Codec::Av1, false, false)).is_err());
    }

    /// The report of this host: every GPU VA-API drives, with the codecs; the CPU in any case.
    #[test]
    fn the_report_lists_the_cpu() {
        let report = report(&gpus());
        assert!(report.contains("CPU\n  vp9"));
    }
}
