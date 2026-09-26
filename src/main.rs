use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const FIREFOX: &str = "/usr/bin/firefox";
const DARKHTTPD: &str = "/usr/bin/darkhttpd";
const STARTPAGE: &str = "/usr/share/magfox/startpage";
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;
const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
    fn signal(signum: i32, handler: usize) -> usize;
    fn geteuid() -> u32;
}

extern "C" fn handle_signal(_: i32) {
    STOP_REQUESTED.store(true, Ordering::Relaxed);
}

enum InstanceLock {
    Primary(LockGuard),
    Secondary(PathBuf),
}

struct LockGuard {
    path: PathBuf,
    file: File,
}

impl LockGuard {
    fn acquire() -> io::Result<InstanceLock> {
        let runtime_dir = env::var_os("XDG_RUNTIME_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
        Self::acquire_at(runtime_dir.join("magfox.lock"))
    }

    fn acquire_at(path: PathBuf) -> io::Result<InstanceLock> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        // flock is tied to this open file description and is released by the OS
        // even if magfox is killed unexpectedly.
        let result = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
        if result == 0 {
            Ok(InstanceLock::Primary(Self { path, file }))
        } else {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                Ok(InstanceLock::Secondary(path))
            } else {
                Err(error)
            }
        }
    }

    fn set_port(&mut self, port: u16) -> io::Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        writeln!(self.file, "{port}")?;
        self.file.sync_data()
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.path) {
            if error.kind() != io::ErrorKind::NotFound {
                eprintln!("magfox: cannot remove {}: {error}", self.path.display());
            }
        }
    }
}

struct ServerGuard {
    child: Child,
}

impl ServerGuard {
    fn start(port: u16) -> io::Result<Self> {
        let startpage = startpage_path();
        let child = Command::new(DARKHTTPD)
            .arg(startpage)
            .args(["--addr", "127.0.0.1", "--port", &port.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()?;
        let mut server = Self { child };
        server.wait_until_ready(port)?;
        Ok(server)
    }

    fn wait_until_ready(&mut self, port: u16) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let address = socket_address(port);

        loop {
            if STOP_REQUESTED.load(Ordering::Relaxed) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "termination requested while starting darkhttpd",
                ));
            }
            if let Some(status) = self.child.try_wait()? {
                return Err(io::Error::other(format!(
                    "darkhttpd exited before becoming ready ({status})"
                )));
            }
            if TcpStream::connect_timeout(&address.into(), Duration::from_millis(100)).is_ok() {
                thread::sleep(Duration::from_millis(50));
                if self.child.try_wait()?.is_none() {
                    return Ok(());
                }
                return Err(io::Error::other(
                    "darkhttpd exited while opening the listening socket",
                ));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "darkhttpd did not start within 5 seconds",
                ));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn ensure_running(&mut self) -> io::Result<()> {
        if let Some(status) = self.child.try_wait()? {
            Err(io::Error::other(format!(
                "darkhttpd terminated unexpectedly ({status})"
            )))
        } else {
            Ok(())
        }
    }
}

fn socket_address(port: u16) -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)
}

fn find_free_port() -> io::Result<u16> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}

fn startpage_path() -> PathBuf {
    if let Some(path) = env::var_os("MAGFOX_STARTPAGE").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }

    let installed = Path::new(STARTPAGE);
    if installed.is_dir() {
        return installed.to_path_buf();
    }

    let source_tree = Path::new(env!("CARGO_MANIFEST_DIR")).join("startpage");
    if source_tree.is_dir() {
        return source_tree;
    }

    installed.to_path_buf()
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn install_signal_handlers() {
    unsafe {
        signal(SIGINT, handle_signal as *const () as usize);
        signal(SIGTERM, handle_signal as *const () as usize);
    }
}

fn launch_firefox(arguments: &[OsString]) -> io::Result<Child> {
    Command::new(FIREFOX).args(arguments).spawn()
}

fn launch_arguments(arguments: &[OsString], port: u16) -> Vec<OsString> {
    let mut launch_arguments = vec![OsString::from("--new-window")];
    if arguments.is_empty() {
        launch_arguments.push(OsString::from(format!("http://127.0.0.1:{port}/")));
    } else {
        launch_arguments.extend_from_slice(arguments);
    }
    launch_arguments
}

fn read_primary_port(path: &Path) -> io::Result<u16> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut file = File::open(path)?;
        let mut value = String::new();
        file.read_to_string(&mut value)?;
        if let Ok(port) = value.trim().parse::<u16>() {
            return Ok(port);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the primary magfox port was not published",
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_server(port: u16) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let address = socket_address(port);
    loop {
        if TcpStream::connect_timeout(&address.into(), Duration::from_millis(100)).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the primary magfox server is not responding",
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn run_secondary(lock_path: &Path, arguments: &[OsString]) -> io::Result<()> {
    let port = read_primary_port(lock_path)?;
    wait_for_server(port)?;
    let arguments = launch_arguments(arguments, port);
    let status = launch_firefox(&arguments)?.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("Firefox exited with {status}")))
    }
}

fn is_firefox_process(path: &Path, own_euid: u32) -> bool {
    let status = match fs::read_to_string(path.join("status")) {
        Ok(status) => status,
        Err(_) => return false,
    };
    let belongs_to_user = status.lines().find_map(|line| {
        line.strip_prefix("Uid:")
            .and_then(|uids| uids.split_whitespace().next())
            .and_then(|uid| uid.parse::<u32>().ok())
    }) == Some(own_euid);
    if !belongs_to_user {
        return false;
    }

    fs::read_link(path.join("exe"))
        .ok()
        .and_then(|exe| {
            exe.file_name()
                .map(|name| name == "firefox" || name == "firefox-bin")
        })
        .unwrap_or(false)
}

fn firefox_is_running() -> bool {
    let own_euid = unsafe { geteuid() };
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .bytes()
                .all(|byte| byte.is_ascii_digit())
        })
        .any(|entry| is_firefox_process(&entry.path(), own_euid))
}

fn run_primary(mut lock: LockGuard, arguments: &[OsString]) -> io::Result<()> {
    let port = find_free_port()?;
    lock.set_port(port)?;
    let mut server = ServerGuard::start(port)?;
    let arguments = launch_arguments(arguments, port);
    let mut firefox = launch_firefox(&arguments)?;
    let mut firefox_status = None;
    let mut empty_checks = 0_u8;

    loop {
        if STOP_REQUESTED.load(Ordering::Relaxed) {
            return Ok(());
        }
        server.ensure_running()?;

        if firefox_status.is_none() {
            firefox_status = firefox.try_wait()?;
        }

        if let Some(status) = firefox_status {
            if firefox_is_running() {
                empty_checks = 0;
            } else {
                // A short grace interval covers Firefox handing off to a newly
                // created or already running browser process.
                empty_checks += 1;
                if empty_checks >= 4 {
                    return if status.success() {
                        Ok(())
                    } else {
                        Err(io::Error::other(format!("Firefox exited with {status}")))
                    };
                }
            }
        }

        thread::sleep(Duration::from_millis(250));
    }
}

fn run() -> io::Result<()> {
    install_signal_handlers();
    let arguments: Vec<OsString> = env::args_os().skip(1).collect();
    match LockGuard::acquire()? {
        InstanceLock::Primary(lock) => run_primary(lock, &arguments),
        InstanceLock::Secondary(path) => run_secondary(&path, &arguments),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("magfox: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_singleton_and_is_removed_on_drop() {
        let path = env::temp_dir().join(format!("magfox-test-{}.lock", std::process::id()));
        let _ = fs::remove_file(&path);

        let primary = match LockGuard::acquire_at(path.clone()).unwrap() {
            InstanceLock::Primary(lock) => lock,
            InstanceLock::Secondary(_) => panic!("first lock acquisition was not primary"),
        };
        assert!(matches!(
            LockGuard::acquire_at(path.clone()).unwrap(),
            InstanceLock::Secondary(_)
        ));

        drop(primary);
        assert!(!path.exists());

        let replacement = match LockGuard::acquire_at(path.clone()).unwrap() {
            InstanceLock::Primary(lock) => lock,
            InstanceLock::Secondary(_) => panic!("lock was not released"),
        };
        drop(replacement);
        assert!(!path.exists());
    }
}
