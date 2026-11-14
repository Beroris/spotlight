//! Which output devices Spotlight offers on Linux.
//!
//! ALSA lists every way into every sound card, some twenty names on a laptop, and most of
//! them can't be opened while a sound server holds the card. So with PipeWire or PulseAudio
//! running, Spotlight plays through the system default and the desktop's sound settings say
//! where that goes, which they can change while the music plays. Without a sound server,
//! each card is offered once.
//!
//! The names are read from ALSA's list. No device is opened to make it.

use crate::cpal_stream::Device;

/// One of ALSA's names for a way to play or record.
pub struct Pcm {
    pub name: String,
    pub description: String,
    pub plays: bool,
}

/// Everything ALSA lists.
fn pcms() -> Vec<Pcm> {
    let Ok(hints) = alsa::device_name::HintIter::new(None, c"pcm") else {
        return Vec::new();
    };
    hints
        .filter_map(|hint| {
            Some(Pcm {
                name: hint.name?,
                description: hint.desc.unwrap_or_default(),
                plays: !matches!(hint.direction, Some(alsa::Direction::Capture)),
            })
        })
        .collect()
}

/// The devices to offer besides the system default.
pub fn devices() -> Vec<Device> {
    offered(&pcms())
}

fn offered(pcms: &[Pcm]) -> Vec<Device> {
    if pcms.iter().any(|pcm| matches!(pcm.name.as_str(), "pipewire" | "pulse")) {
        return Vec::new();
    }
    let mut devices: Vec<Device> = Vec::new();
    for pcm in pcms.iter().filter(|pcm| pcm.plays) {
        // A card's own default, and each of its HDMI outputs. The rest are the same outputs
        // again with different channel layouts or mixing.
        if !pcm.name.starts_with("sysdefault:") && !pcm.name.starts_with("hdmi:") {
            continue;
        }
        let described = pcm.description.lines().next().unwrap_or_default().trim();
        let mut name = if described.is_empty() { pcm.name.clone() } else { described.to_string() };
        if devices.iter().any(|device| device.name == name) {
            name = format!("{name} ({})", pcm.name);
        }
        devices.push(Device { id: pcm.name.clone(), name });
    }
    devices
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(name: &str, description: &str) -> Pcm {
        Pcm { name: name.to_string(), description: description.to_string(), plays: true }
    }

    fn laptop() -> Vec<Pcm> {
        vec![
            pcm("null", "Discard all samples (playback) or generate zero samples (capture)"),
            pcm("default", "Default Audio Device"),
            pcm("hw:CARD=Generic,DEV=3", "HD-Audio Generic, HDMI 0\nDirect hardware device without any conversions"),
            pcm("hdmi:CARD=Generic,DEV=0", "HD-Audio Generic, HDMI 0\nHDMI Audio Output"),
            pcm("hdmi:CARD=Generic,DEV=1", "HD-Audio Generic, HDMI 0\nHDMI Audio Output"),
            pcm("sysdefault:CARD=Generic_1", "HD-Audio Generic, ALC245 Analog\nDefault Audio Device"),
            pcm("front:CARD=Generic_1,DEV=0", "HD-Audio Generic, ALC245 Analog\nFront output / input"),
            pcm("surround51:CARD=Generic_1,DEV=0", "HD-Audio Generic, ALC245 Analog\n5.1 Surround output"),
            pcm("dmix:CARD=Generic_1,DEV=0", "HD-Audio Generic, ALC245 Analog\nDirect sample mixing device"),
        ]
    }

    #[test]
    fn a_sound_server_leaves_only_the_system_default() {
        let mut pcms = laptop();
        pcms.push(pcm("pipewire", "PipeWire Sound Server"));
        assert!(offered(&pcms).is_empty());
    }

    #[test]
    fn without_a_sound_server_each_output_is_offered_once() {
        let devices = offered(&laptop());
        let shown: Vec<(&str, &str)> = devices.iter().map(|device| (device.id.as_str(), device.name.as_str())).collect();
        assert_eq!(
            shown,
            [
                ("hdmi:CARD=Generic,DEV=0", "HD-Audio Generic, HDMI 0"),
                ("hdmi:CARD=Generic,DEV=1", "HD-Audio Generic, HDMI 0 (hdmi:CARD=Generic,DEV=1)"),
                ("sysdefault:CARD=Generic_1", "HD-Audio Generic, ALC245 Analog"),
            ]
        );
    }

    #[test]
    fn a_microphone_is_not_an_output() {
        let mut microphone = pcm("sysdefault:CARD=Mic", "USB Microphone");
        microphone.plays = false;
        assert!(offered(&[microphone]).is_empty());
    }
}
