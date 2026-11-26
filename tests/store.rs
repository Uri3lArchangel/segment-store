use segment_store::Store;
use std::{
    collections::BTreeMap,
    fs, io,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        loop {
            let p = std::env::temp_dir().join(format!(
                "segment-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&p) {
                Ok(()) => return Self(p),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("{e}"),
            }
        }
    }
    fn file(&self) -> PathBuf {
        self.0.join("log.seg")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn replay_matches_deterministic_model() {
    let dir = Scratch::new();
    let file = dir.file();
    let mut reference = BTreeMap::new();
    let mut state = 0x57689b17u32;
    for _ in 0..5 {
        let (mut s, replay) = Store::open(&file).unwrap();
        assert_eq!(replay.truncated_bytes, 0);
        for _ in 0..60 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let key = [(state >> 8) as u8 % 31];
            if state % 4 == 0 {
                reference.remove(&key);
                s.delete(&key).unwrap();
            } else {
                let value = state.to_le_bytes().to_vec();
                s.put(&key, &value).unwrap();
                reference.insert(key, value);
            }
        }
        for k in 0..31 {
            assert_eq!(s.get(&[k]).unwrap().as_ref(), reference.get(&[k]));
        }
        assert_eq!(s.len(), reference.len());
    }
}
#[test]
fn every_byte_cut_of_final_frame_recovers_previous_value() {
    let dir = Scratch::new();
    let file = dir.file();
    {
        let (mut s, _) = Store::open(&file).unwrap();
        s.put(b"k", b"original").unwrap();
    }
    let prefix = fs::read(&file).unwrap();
    {
        let (mut s, _) = Store::open(&file).unwrap();
        s.put(b"k", b"overwrite").unwrap();
    }
    let complete = fs::read(&file).unwrap();
    for cut in prefix.len() + 1..complete.len() {
        fs::write(&file, &complete[..cut]).unwrap();
        let (mut s, replay) = Store::open(&file).unwrap();
        assert_eq!(replay.truncated_bytes, (cut - prefix.len()) as u64);
        assert_eq!(s.get(b"k").unwrap().unwrap(), b"original");
        assert_eq!(fs::read(&file).unwrap(), prefix);
        s.put(b"second", b"after repair").unwrap();
        assert_eq!(s.get(b"second").unwrap().unwrap(), b"after repair");
    }
    fs::write(&file, &prefix).unwrap();
    let (s, _) = Store::open(&file).unwrap();
    drop(s);
    fs::write(&file, complete).unwrap();
    let (mut s, replay) = Store::open(&file).unwrap();
    assert_eq!(replay.truncated_bytes, 0);
    assert_eq!(s.get(b"k").unwrap().unwrap(), b"overwrite");
}
#[test]
fn complete_corruption_is_rejected_without_truncation() {
    let dir = Scratch::new();
    let file = dir.file();
    {
        let (mut s, _) = Store::open(&file).unwrap();
        s.put(b"key", b"abc").unwrap();
        s.put(b"tail", b"def").unwrap();
    }
    let baseline = fs::read(&file).unwrap();
    // Flip each byte individually, including length fields and checksums.
    for i in 0..baseline.len() {
        let mut bad = baseline.clone();
        bad[i] ^= 0x01;
        fs::write(&file, &bad).unwrap();
        let Err(e) = Store::open(&file) else {
            panic!("corruption at {i} accepted");
        };
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&file).unwrap(), bad);
    }
}
#[test]
fn validates_sizes_and_distinguishes_empty_from_deleted() {
    let dir = Scratch::new();
    let (mut s, _) = Store::open(dir.file()).unwrap();
    assert!(s.put(b"", b"x").is_err());
    assert!(s.put(&[0; 1025], b"x").is_err());
    assert!(s.put(b"a", &vec![9; (1 << 20) + 1]).is_err());
    s.put(b"k", b"").unwrap();
    assert_eq!(s.get(b"k").unwrap(), Some(vec![]));
    s.delete(b"k").unwrap();
    assert_eq!(s.get(b"k").unwrap(), None);
}
#[test]
fn cli_reopens_and_excludes_second_writer() {
    let dir = Scratch::new();
    let file = dir.file();
    let cli = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_segment-store"));
        cmd.arg(&file);
        cmd
    };
    assert!(cli()
        .args(["put", "hello", "survives"])
        .output()
        .unwrap()
        .status
        .success());
    assert_eq!(
        cli().args(["get", "hello"]).output().unwrap().stdout,
        b"survives"
    );
    let (held, _) = Store::open(&file).unwrap();
    assert!(!cli()
        .args(["put", "key", "value"])
        .output()
        .unwrap()
        .status
        .success());
    drop(held);
    assert!(cli()
        .args(["delete", "hello"])
        .output()
        .unwrap()
        .status
        .success());
    assert_eq!(
        cli().args(["get", "hello"]).output().unwrap().status.code(),
        Some(2)
    );
}

#[test]
fn verify_nonexistent_segment_does_not_create_one() {
    let dir = Scratch::new();
    let file = dir.file();
    let output = Command::new(env!("CARGO_BIN_EXE_segment-store"))
        .arg(&file)
        .arg("verify")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(!file.exists());
}
