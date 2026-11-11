#![forbid(unsafe_code)]

fn main() -> anyhow::Result<()> {
    lossless_daemon::run(
        &lossless_daemon::config_path(),
        lossless_proto::default_socket(),
    )
}
