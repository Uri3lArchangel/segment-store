use segment_store::Store;
use std::{env, fs, io, path::PathBuf, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let path = PathBuf::from(args.next().ok_or("provide a new data file path")?);
    let count = args
        .next()
        .ok_or("provide a record count")?
        .to_str()
        .ok_or("record count must be UTF-8")?
        .parse::<usize>()?;
    if count == 0 || args.next().is_some() {
        return Err("provide FILE COUNT (COUNT > 0)".into());
    }
    // Never overwrite a user's existing file.
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)?;
    let (mut s, _) = Store::open(&path)?;
    let mut latencies = Vec::with_capacity(count);
    let wall = Instant::now();
    for i in 0..count {
        let start = Instant::now();
        s.put(&i.to_le_bytes(), &[0x42; 256])?;
        latencies.push(start.elapsed());
    }
    let total = wall.elapsed();
    drop(s);
    let start = Instant::now();
    let (mut s, recovery) = Store::open(&path)?;
    let reopen = start.elapsed();
    if s.len() != count
        || recovery.truncated_bytes != 0
        || s.get(&(count - 1).to_le_bytes())?.as_deref() != Some(&[0x42; 256][..])
    {
        return Err(io::Error::other("benchmark verification failed").into());
    }
    latencies.sort_unstable();
    let p50 = latencies[(count - 1) / 2];
    let p95 = latencies[(count - 1) - (count - 1) / 20];
    println!(
        "records={count} bytes={} writes/s={:.1} p50_us={} p95_us={} reopen_us={}",
        fs::metadata(path)?.len(),
        count as f64 / total.as_secs_f64(),
        p50.as_micros(),
        p95.as_micros(),
        reopen.as_micros()
    );
    Ok(())
}
