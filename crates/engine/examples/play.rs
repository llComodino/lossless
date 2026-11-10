use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use lossless_engine::{Output, Sink, Source};

/// Play the first `seconds` of each file through one Output.
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: play <device> <seconds per file> <file>...";
    let device = args.next().context(usage)?;
    let secs: u64 = args.next().context(usage)?.parse()?;
    let mut out = Output::new(device);
    for path in args.map(PathBuf::from) {
        let mut src = Source::open(&path)?;
        let fmt = out.ensure(src.format)?;
        println!("{}: source {:?} output {fmt:?}", path.display(), src.format);
        let limit = u64::from(src.format.rate) * secs;
        let channels = u64::from(src.format.channels);
        let mut frames = 0;
        while let Some(chunk) = src.next_chunk()? {
            frames += chunk.len() as u64 / channels;
            out.write(chunk)?;
            if frames >= limit {
                break;
            }
        }
    }
    std::thread::sleep(Duration::from_millis(100));
    out.close();
    Ok(())
}
