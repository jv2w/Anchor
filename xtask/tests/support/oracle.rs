//! The gophertunnel oracle (`xtask/go/oracle`): built once per test process and run on record
//! files of `[u32 LE id][u32 LE len][bytes]`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use super::fs::{create_dir, write};
use super::process::build_go;

/// The oracle executable, built on first use.
static BINARY: OnceLock<PathBuf> = OnceLock::new();

/// One record: a packet ID or status (0 ok, 1 error) and its bytes.
pub(crate) type Record = (u32, Vec<u8>);

/// The directory for this test binary's oracle and record files. Each test binary has its own,
/// so binaries never overwrite each other's files.
fn work_dir() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(concat!("oracle-", env!("CARGO_CRATE_NAME")))
}

/// Builds the oracle on first use and returns its path; a failed build fails the test.
fn binary() -> &'static Path {
    BINARY.get_or_init(|| {
        let dir = work_dir();
        create_dir(&dir);
        build_go(&dir, "oracle").unwrap_or_else(|e| panic!("{e}"))
    })
}

/// Runs `oracle <args> [-in <name>.in] -out <name>.out` and returns the output records. `input`
/// is written to the `-in` file when present. `name` must be unique within the test binary.
pub(crate) fn run(name: &str, args: &[&str], input: Option<&[Record]>) -> Vec<Record> {
    let exe = binary();
    let dir = work_dir();
    let out_path = dir.join(format!("{name}.out"));
    let mut command = Command::new(exe);
    command.args(args);
    if let Some(records) = input {
        let in_path = dir.join(format!("{name}.in"));
        let mut bytes = Vec::new();
        for (id, data) in records {
            bytes.extend_from_slice(&id.to_le_bytes());
            let len = u32::try_from(data.len()).expect("record fits u32");
            bytes.extend_from_slice(&len.to_le_bytes());
            bytes.extend_from_slice(data);
        }
        write(&in_path, &bytes);
        command.arg("-in").arg(in_path);
    }
    let output = command
        .arg("-out")
        .arg(&out_path)
        .output()
        .expect("the oracle starts");
    assert!(
        output.status.success(),
        "oracle {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = read(&out_path);
    let mut rest = bytes.as_slice();
    let mut records = Vec::new();
    while !rest.is_empty() {
        let (header, tail) = rest.split_at_checked(8).expect("whole record header");
        let id = u32::from_le_bytes(header[..4].try_into().expect("4 bytes"));
        let len = u32::from_le_bytes(header[4..].try_into().expect("4 bytes"));
        let len = usize::try_from(len).expect("length fits usize");
        let (data, tail) = tail.split_at_checked(len).expect("whole record data");
        records.push((id, data.to_vec()));
        rest = tail;
    }
    records
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: test tooling outside the server"
)]
fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("read an oracle output file")
}
