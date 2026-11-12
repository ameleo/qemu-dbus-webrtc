// SPDX-License-Identifier: MPL-2.0

//! What the host's GPUs encode, as this program uses them: read from the VA elements
//! GStreamer registers, one per codec and GPU (`vaav1lpenc` for the first GPU,
//! `varenderD129av1lpenc` for the next), and the raw formats they take.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::{Result, bail};
use gstreamer as gst;
use gstreamer::prelude::*;

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
    /// Its kernel driver and PCI id, when the system says them: `i915, 8086:56a5`.
    pub description: String,
    pub codecs: Vec<Support>,
}

/// The name of a codec in the VA element names.
fn element_codec(codec: Codec) -> &'static str {
    match codec {
        Codec::Av1 => "av1",
        Codec::Vp9 => "vp9",
        Codec::Hevc => "h265",
    }
}

/// The raw formats a VA encoder takes, from its sink pad template.
fn formats(factory: &gst::ElementFactory) -> Vec<String> {
    let mut formats = Vec::new();
    for template in factory.static_pad_templates() {
        if template.direction() != gst::PadDirection::Sink {
            continue;
        }
        for structure in template.caps().iter() {
            if let Ok(list) = structure.get::<gst::List>("format") {
                formats.extend(list.iter().filter_map(|v| v.get::<String>().ok()));
            } else if let Ok(format) = structure.get::<String>("format") {
                formats.push(format);
            }
        }
    }
    formats
}

/// What a codec gives with an encoder that takes `formats`, as this program uses it.
fn support(codec: Codec, formats: &[String]) -> Support {
    let takes = |format: &str| formats.iter().any(|f| f == format);
    let full = codec.has_full_chroma();
    let ten = codec.has_ten_bits();
    Support {
        codec,
        reduced: Depths {
            eight: takes("NV12"),
            ten: ten && takes("P010_10LE"),
        },
        full: Depths {
            eight: full && takes("VUYA"),
            ten: full && ten && takes("Y410"),
        },
    }
}

/// The render node of a VA element's GPU: the first one for the elements without a device in
/// their name (asked to the element itself), `/dev/dri/renderD<n>` otherwise.
fn device(factory: &gst::ElementFactory, node: &str) -> String {
    if !node.is_empty() {
        return format!("/dev/dri/{node}");
    }
    factory
        .create()
        .build()
        .ok()
        .and_then(|element| element.property_value("device-path").get::<String>().ok())
        .unwrap_or_else(|| "the default GPU".into())
}

/// The kernel driver and PCI id of a render node, from sysfs.
fn description(device: &str) -> String {
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

/// The host's GPUs and what each encodes, from the VA elements of GStreamer (initialized).
pub fn gpus() -> Vec<Gpu> {
    // By render node name ("" for the first GPU), then codec.
    let mut found: BTreeMap<String, (String, Vec<Support>)> = BTreeMap::new();
    let registry = gst::Registry::get();
    for feature in registry.features(gst::ElementFactory::static_type()) {
        let Ok(factory) = feature.downcast::<gst::ElementFactory>() else {
            continue;
        };
        let name = factory.name();
        // The low-power encoders: the ones this program uses.
        let Some(middle) = name
            .strip_prefix("va")
            .and_then(|n| n.strip_suffix("lpenc"))
        else {
            continue;
        };
        for codec in CODECS {
            let Some(node) = middle.strip_suffix(element_codec(codec)) else {
                continue;
            };
            let entry = found
                .entry(node.to_string())
                .or_insert_with(|| (device(&factory, node), Vec::new()));
            entry.1.push(support(codec, &formats(&factory)));
        }
    }
    found
        .into_values()
        .map(|(device, mut codecs)| {
            codecs.sort_by_key(|s| CODECS.iter().position(|c| *c == s.codec));
            Gpu {
                description: description(&device),
                device,
                codecs,
            }
        })
        .collect()
}

/// Whether this host can encode on the CPU: GStreamer's libvpx VP9 encoder is installed.
pub fn software() -> bool {
    gst::ElementFactory::find("vp9enc").is_some()
}

/// What `--codecs` prints: each GPU and what it encodes, with the options that pick it, and
/// the encoding on the CPU.
pub fn report(gpus: &[Gpu], software: bool) -> String {
    let mut out = String::new();
    if gpus.is_empty() {
        out += "No VA-API encoder found: is GStreamer's va plugin installed, and a GPU with its \
                VA driver? Encoding on the CPU.\n";
    }
    for (i, gpu) in gpus.iter().enumerate() {
        let used = if i == 0 { " (used)" } else { "" };
        let description = if gpu.description.is_empty() {
            String::new()
        } else {
            format!(" ({})", gpu.description)
        };
        out += &format!("{}{description}{used}\n", gpu.device);
        for s in &gpu.codecs {
            let name = match s.codec {
                Codec::Av1 => "av1",
                Codec::Vp9 => "vp9",
                Codec::Hevc => "hevc",
            };
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
    let used = if gpus.is_empty() { " (used)" } else { "" };
    if software {
        out += &format!(
            "CPU{used}\n  vp9   {:34} --software --codec vp9 [--chroma 444]\n",
            "4:2:0 8 bits, 4:4:4 8 bits"
        );
    } else {
        out += "CPU: no VP9 encoder (GStreamer's vpx plugin)\n";
    }
    if !gpus.is_empty() {
        out += "10 bits go to clients that say they decode them (Hello); the others get 8 bits.\n";
    }
    out
}

/// Whether the GPU this program uses encodes `settings`; a message that says what it does
/// encode otherwise.
pub fn check(gpus: &[Gpu], settings: &Settings) -> Result<()> {
    if settings.software {
        if settings.codec != Codec::Vp9 {
            bail!("encoding on the CPU is VP9 only: --codec vp9 (see --codecs)");
        }
        if !software() {
            bail!("no VP9 encoder for the CPU: GStreamer's vpx plugin is missing (see --codecs)");
        }
        return Ok(());
    }
    let Some(gpu) = gpus.first() else {
        bail!("no VA-API encoder found (see --codecs)");
    };
    let Some(support) = gpu.codecs.iter().find(|s| s.codec == settings.codec) else {
        bail!(
            "{} does not encode {:?} (see --codecs)",
            gpu.device,
            settings.codec
        );
    };
    let depths = if settings.full_chroma {
        support.full
    } else {
        support.reduced
    };
    if !depths.eight {
        let chroma = if settings.full_chroma {
            "4:4:4"
        } else {
            "4:2:0"
        };
        bail!(
            "{} does not encode {:?} in {chroma} (see --codecs)",
            gpu.device,
            settings.codec
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(formats: &[&str]) -> Vec<String> {
        formats.iter().map(|f| f.to_string()).collect()
    }

    #[test]
    fn what_each_codec_gives() {
        let a380_hevc = strings(&["NV12", "P010_10LE", "VUYA", "Y410"]);
        let hevc = support(Codec::Hevc, &a380_hevc);
        assert_eq!(
            hevc.reduced,
            Depths {
                eight: true,
                ten: true
            }
        );
        assert_eq!(
            hevc.full,
            Depths {
                eight: true,
                ten: true
            }
        );
        // VP9 in 10 bits is not offered, whatever the encoder takes.
        let vp9 = support(Codec::Vp9, &a380_hevc);
        assert_eq!(
            vp9.full,
            Depths {
                eight: true,
                ten: false
            }
        );
        // No AV1 in 4:4:4 here.
        let av1 = support(Codec::Av1, &strings(&["NV12", "P010_10LE"]));
        assert!(!av1.full.any());
        assert_eq!(av1.reduced.to_string(), "8, 10 bits");
    }
}
