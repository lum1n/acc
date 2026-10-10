//! A throwaway OpenSSH server on localhost for tests, shared by the acc-ssh
//! and acc-ffi test suites.
//!
//! Skips (returns `None`) when `sshd` or `ssh-keygen` is missing, unless
//! `ACC_REQUIRE_SSHD=1` (set in CI).

#![allow(dead_code, clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

pub struct Sshd {
    pub dir: PathBuf,
    pub port: u16,
    child: Child,
    /// The authorized user key, OpenSSH format.
    pub key: String,
    /// The host key's SHA-256 fingerprint.
    pub fingerprint: String,
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn find(name: &str) -> Option<PathBuf> {
    [
        "/usr/sbin",
        "/usr/bin",
        "/usr/local/sbin",
        "/usr/local/bin",
        "/opt/homebrew/sbin",
        "/sbin",
    ]
    .iter()
    .map(|d| Path::new(d).join(name))
    .find(|p| p.exists())
}

pub fn keygen(path: &Path) {
    let ok = Command::new(find("ssh-keygen").expect("ssh-keygen"))
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .status()
        .expect("ssh-keygen")
        .success();
    assert!(ok);
}

pub fn start_sshd(name: &str) -> Option<Sshd> {
    let (Some(sshd), Some(_)) = (find("sshd"), find("ssh-keygen")) else {
        assert!(
            std::env::var_os("ACC_REQUIRE_SSHD").is_none(),
            "sshd and ssh-keygen are required"
        );
        eprintln!("skipping: sshd not installed");
        return None;
    };
    let dir = std::env::temp_dir().join(format!("acc-sshd-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    keygen(&dir.join("host"));
    keygen(&dir.join("user"));
    std::fs::copy(dir.join("user.pub"), dir.join("authorized_keys")).expect("authorized_keys");
    let fp = Command::new(find("ssh-keygen").expect("ssh-keygen"))
        .arg("-lf")
        .arg(dir.join("host.pub"))
        .output()
        .expect("fingerprint");
    let fingerprint = String::from_utf8_lossy(&fp.stdout)
        .split_whitespace()
        .nth(1)
        .expect("fingerprint")
        .to_owned();
    // Pick a free port.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let config = format!(
        "Port {port}\nListenAddress 127.0.0.1\nHostKey {d}/host\nAuthorizedKeysFile {d}/authorized_keys\n\
         PasswordAuthentication no\nKbdInteractiveAuthentication no\nStrictModes no\nPidFile {d}/pid\n\
         PermitRootLogin prohibit-password\nLogLevel ERROR\n",
        d = dir.display()
    );
    std::fs::write(dir.join("sshd_config"), config).expect("config");
    let child = Command::new(sshd)
        .args(["-D", "-e", "-f"])
        .arg(dir.join("sshd_config"))
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn sshd");
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let key = std::fs::read_to_string(dir.join("user")).expect("key");
    Some(Sshd {
        dir,
        port,
        child,
        key,
        fingerprint,
    })
}

/// The user the tests log in as: the one running them.
pub fn user() -> String {
    std::env::var("USER")
        .ok()
        .or_else(|| {
            let out = Command::new("id").arg("-un").output().ok()?;
            Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        })
        .expect("user")
}
