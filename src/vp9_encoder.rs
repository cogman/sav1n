use tokio::process::Command;
use crate::encoder::{Encoder, EncoderOptions};

pub struct Vp9Encoder {}

unsafe impl Send for Vp9Encoder {}

unsafe impl Sync for Vp9Encoder {}

impl Encoder for Vp9Encoder {
    fn first_pass(&self, options: EncoderOptions) -> Command {
        let mut c = Command::new("vpxenc");
        c.arg("--quiet")
            .arg("--passes=2")
            .arg("--pass=1")
            .arg("-b")
            .arg("10")
            .arg("--profile=2")
            .arg(format!("--threads={}", options.threads))
            .arg(format!("--fpf={}", options.log_file))
            .arg("--end-usage=q")
            .arg("-o")
            .arg("/dev/null")
            .arg(options.input);
        return c;
    }

    fn second_pass(&self, options: EncoderOptions) -> Command {
        let mut c = Command::new("vpxenc");
        c.arg(format!("--cq-level={}", options.cq))
            .arg(format!("--cpu-used={}", options.cpu_used))
            .arg(format!("--fpf={}", options.log_file))
            .arg("--quiet")
            .arg("--passes=2")
            .arg("--pass=2")
            .arg("--profile=2")
            .arg("--good")
            .arg("--lag-in-frames=25")
            .arg("--kf-max-dist=250")
            .arg("--auto-alt-ref=1")
            .arg("--arnr-strength=1")
            .arg("--arnr-maxframes=7")
            .arg("--enable-tpl=1")
            .arg(format!("--threads={}", options.threads))
            .arg("-b")
            .arg("10")
            .arg("--end-usage=q")
            .arg("--ivf")
            .arg("-o")
            .arg(options.output)
            .arg(options.input);
        return c;
    }
}