use segment_store::Store;
use std::{
    env,
    io::{self, Write},
    process::ExitCode,
};

fn run() -> io::Result<bool> {
    let a: Vec<_> = env::args_os().skip(1).collect();
    let (Some(path), Some(command)) = (a.first(), a.get(1).and_then(|v| v.to_str())) else {
        return Err(usage());
    };
    match (command, a.len()) {
        ("put", 4) | ("get", 3) | ("delete", 3) | ("verify", 2) => (),
        _ => return Err(usage()),
    }
    let (mut store, recovery) = if command == "put" {
        Store::open(path)?
    } else {
        Store::open_existing(path)?
    };
    if recovery.truncated_bytes > 0 {
        eprintln!(
            "recovered: discarded {} bytes from an incomplete final frame",
            recovery.truncated_bytes
        );
    }
    match command {
        "put" => store.put(a[2].as_encoded_bytes(), a[3].as_encoded_bytes())?,
        "get" => {
            let Some(value) = store.get(a[2].as_encoded_bytes())? else {
                return Ok(false);
            };
            io::stdout().lock().write_all(&value)?;
        }
        "delete" => store.delete(a[2].as_encoded_bytes())?,
        "verify" => println!(
            "{} live keys; {} bytes truncated",
            store.len(),
            recovery.truncated_bytes
        ),
        _ => unreachable!(),
    }
    Ok(true)
}
fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage: segment-store FILE {put KEY VALUE|get KEY|delete KEY|verify}",
    )
}
fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(err) => {
            eprintln!("segment-store: {err}");
            ExitCode::FAILURE
        }
    }
}
