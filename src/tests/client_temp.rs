use super::*;
use crate::testsupport::TempDir;
use std::os::unix::fs::symlink;
use std::process::{Child, Stdio};

const MINUTE: u64 = 60_000;
const NOW: u64 = 1_800_000_000_000;

/// A client folder named as made `ago` ms before `NOW`, holding a file.
fn client(root: &Path, label: &str, ago: u64) -> PathBuf {
    let dir = root.join(format!("{label}-{}-0", NOW - ago));
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("left.txt"), "x").unwrap();
    dir
}

/// A process that uses a folder until the test ends.
struct User(Child);
impl Drop for User {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn user(configure: impl FnOnce(&mut Command)) -> User {
    let mut command = Command::new("sleep");
    command.arg("600").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    configure(&mut command);
    User(command.spawn().unwrap())
}

// The sweep deletes on the owner's machines: a folder a running client still
// uses, or anything that is not an ended client's own folder, must survive it.
#[test]
fn a_sweep_removes_only_ended_clients_folders() {
    let temp = TempDir::new("client-temp-sweep");
    let root = temp.path().join("clients");
    private_root(&root, true).unwrap();
    let ended = client(&root, "s1", 3 * MINUTE);
    // A tool left a read-only folder inside, which must not keep it.
    fs::create_dir(ended.join("locked")).unwrap();
    fs::write(ended.join("locked/file"), "x").unwrap();
    fs::set_permissions(ended.join("locked"), fs::Permissions::from_mode(0o500)).unwrap();
    let starting = client(&root, "s2", MINUTE / 2);
    let by_environment = client(&root, "job", 3 * MINUTE);
    let by_working_folder = client(&root, "title", 3 * MINUTE);
    let by_open_file = client(&root, "quota", 3 * MINUTE);
    let unknown = root.join("notes");
    fs::create_dir(&unknown).unwrap();
    let elsewhere = temp.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::write(elsewhere.join("keep.txt"), "x").unwrap();
    let link = root.join(format!("s3-{}-0", NOW - 3 * MINUTE));
    symlink(&elsewhere, &link).unwrap();

    // Only its Java options name this one, as a JVM daemon's would.
    let java = vars_for(&by_environment, None).into_iter().find(|(name, _)| name == "JAVA_TOOL_OPTIONS").unwrap().1;
    let _environment = user(|command| {
        command.env("JAVA_TOOL_OPTIONS", java);
    });
    let _working = user(|command| {
        command.current_dir(&by_working_folder);
    });
    let _open = fs::File::open(by_open_file.join("left.txt")).unwrap();

    let swept = sweep(&root, NOW).unwrap();
    assert_eq!(swept.removed, vec![ended.clone()]);
    assert!(swept.failed.is_empty(), "{:?}", swept.failed);
    assert!(!ended.exists());
    for kept in [&starting, &by_environment, &by_working_folder, &by_open_file, &unknown] {
        assert!(kept.is_dir(), "{} was removed", kept.display());
    }
    assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert!(elsewhere.join("keep.txt").exists());
}

// In a shared /var/tmp another user could make the root first, or a link to
// their own folder, and read or replace what clients write.
#[test]
fn a_root_another_user_could_reach_is_refused() {
    let temp = TempDir::new("client-temp-root");
    let open = temp.path().join("open");
    fs::create_dir(&open).unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(create(&open, "s1", NOW).is_err());
    assert!(sweep(&open, NOW).is_err());
    let target = temp.path().join("target");
    DirBuilder::new().mode(0o700).create(&target).unwrap();
    let link = temp.path().join("link");
    symlink(&target, &link).unwrap();
    assert!(create(&link, "s1", NOW).is_err());
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
}
