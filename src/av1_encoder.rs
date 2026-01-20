use crate::encoder::{Encoder, EncoderOptions};
use tokio::process::Command;

pub struct Av1Encoder {}

unsafe impl Send for Av1Encoder {}

unsafe impl Sync for Av1Encoder {}

impl Encoder for Av1Encoder {
    fn first_pass(&self, options: EncoderOptions) -> Command {
        let mut c = Command::new("aomenc");
        c.arg("--quiet")
            .arg("--good")
            .arg("--passes=2")
            .arg("--pass=1")
            .arg("-b")
            .arg("10")
            .arg("--kf-max-dist=250")
            .arg("--lag-in-frames=48")
            .arg("--enable-fwd-kf=1")
            .arg("--aq-mode=1")
            .arg("--enable-qm=1")
            .arg("--enable-keyframe-filtering=2")
            .arg("--deltaq-mode=0")
            .arg(format!("--threads={}", options.threads))
            .arg(format!("--fpf={}", options.log_file))
            .arg("--end-usage=q")
            .arg("-o")
            .arg("/dev/null")
            .arg(options.input);
        return c;
    }

    fn second_pass(&self, options: EncoderOptions) -> Command {
        let mut c = Command::new("aomenc");
        c.arg(format!("--cq-level={}", options.cq))
            .arg(format!("--cpu-used={}", options.cpu_used))
            .arg(format!("--fpf={}", options.log_file))
            .arg("--quiet")
            .arg("--good")
            .arg("--passes=2")
            .arg("--pass=2")
            .arg("--lag-in-frames=48")
            .arg("--enable-fwd-kf=1")
            .arg("--aq-mode=1")
            .arg("--enable-qm=1")
            .arg("--enable-keyframe-filtering=2")
            .arg("--deltaq-mode=0")
            .arg("--kf-max-dist=250")
            .arg("--arnr-strength=0")
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
