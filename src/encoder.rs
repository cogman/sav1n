use tokio::process::Command;

pub trait Encoder {
    fn first_pass(&self, options: EncoderOptions) -> Command;
    fn second_pass(&self, options: EncoderOptions) -> Command;
}

pub struct EncoderOptions<'t> {
    pub cpu_used: u32,
    pub threads: u32,
    pub cq: u32,
    pub log_file: &'t str,
    pub input: &'t str,
    pub output: &'t str,
}

impl Default for EncoderOptions<'_> {
    fn default() -> Self {
        EncoderOptions {
            cpu_used: 0,
            threads: 1,
            cq: 20,
            log_file: "",
            input: "",
            output: "",
        }
    }
}
