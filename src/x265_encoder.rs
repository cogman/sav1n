use crate::encoder::{Encoder, EncoderOptions};
use tokio::process::Command;

const MAX_CRF: u32 = 51;

const PRESETS: [&str; 10] = [
    "veryslow",
    "slower",
    "slow",
    "medium",
    "fast",
    "faster",
    "veryfast",
    "superfast",
    "ultrafast",
    "ultrafast",
];

pub struct X265Encoder {}

unsafe impl Send for X265Encoder {}

unsafe impl Sync for X265Encoder {}

fn preset(cpu_used: u32) -> &'static str {
    PRESETS[cpu_used.min(PRESETS.len() as u32 - 1) as usize]
}

impl Encoder for X265Encoder {
    fn first_pass(&self, _options: EncoderOptions) -> Option<Command> {
        None
    }

    fn second_pass(&self, options: EncoderOptions) -> Command {
        let mut c = Command::new("x265");
        c.arg("--y4m")
            .arg("--crf")
            .arg(options.cq.min(MAX_CRF).to_string())
            .arg("--preset")
            .arg(preset(options.cpu_used))
            .arg("--pools")
            .arg(options.threads.to_string())
            .arg("--output-depth")
            .arg("10")
            .arg("--no-progress")
            .arg("--log-level")
            .arg("error")
            .arg("--output")
            .arg(options.output)
            .arg(options.input);
        c
    }

    fn container_format(&self) -> &'static str {
        "hevc"
    }

    fn max_cq(&self) -> u32 {
        MAX_CRF
    }
}

#[cfg(test)]
mod tests {
    use super::{preset, X265Encoder};
    use crate::encoder::{Encoder, EncoderOptions};

    #[test]
    fn builds_single_pass_10_bit_command() {
        let encoder = X265Encoder {};
        assert!(encoder.first_pass(EncoderOptions::default()).is_none());
        let command = encoder.second_pass(EncoderOptions {
            cpu_used: 3,
            threads: 2,
            cq: 60,
            input: "in.y4m",
            output: "out.hevc",
            ..Default::default()
        });
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|a| a.to_str().unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "--y4m",
                "--crf",
                "51",
                "--preset",
                "medium",
                "--pools",
                "2",
                "--output-depth",
                "10",
                "--no-progress",
                "--log-level",
                "error",
                "--output",
                "out.hevc",
                "in.y4m"
            ]
        );
    }

    #[test]
    fn maps_cpu_used_to_x265_presets() {
        assert_eq!(preset(0), "veryslow");
        assert_eq!(preset(3), "medium");
        assert_eq!(preset(6), "veryfast");
        assert_eq!(preset(9), "ultrafast");
        assert_eq!(preset(100), "ultrafast");
    }
}
