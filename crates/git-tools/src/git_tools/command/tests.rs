//! 覆盖 G-17 / P-09：真实 Git HTTP(S) 请求的代理选择、优先级和失败不直连。
//! HKCU 仅在独占子进程内映射到自有临时键，不修改用户的 Internet 设置。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Duration,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegOpenCurrentUser, RegOverridePredefKey,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_ALL_ACCESS, REG_DWORD, REG_OPTION_VOLATILE,
    REG_SZ,
};

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

struct Registry {
    original: HKEY,
    root: HKEY,
    path: Vec<u16>,
}

impl Registry {
    fn new() -> Self {
        let mut registry = Self {
            original: std::ptr::null_mut(),
            root: std::ptr::null_mut(),
            path: wide(&format!(
                "Software\\JchToolsGitProxyTest-{}",
                std::process::id()
            )),
        };
        // SAFETY: outputs are live HKEY slots; names are NUL-terminated; optional pointers are null.
        assert_eq!(
            unsafe { RegOpenCurrentUser(KEY_ALL_ACCESS, &raw mut registry.original) },
            0
        );
        // SAFETY: original is owned and the test key is volatile and unique to this child.
        assert_eq!(
            unsafe {
                RegCreateKeyExW(
                    registry.original,
                    registry.path.as_ptr(),
                    0,
                    std::ptr::null(),
                    REG_OPTION_VOLATILE,
                    KEY_ALL_ACCESS,
                    std::ptr::null(),
                    &raw mut registry.root,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: the key stays open until this child restores its own HKCU mapping.
        assert_eq!(
            unsafe { RegOverridePredefKey(HKEY_CURRENT_USER, registry.root) },
            0
        );
        registry
    }

    fn set(&self, enabled: u32, server: &str, bypass: &str) {
        let mut key = std::ptr::null_mut();
        let path = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings");
        // SAFETY: root is owned; path and output slot remain valid through the call.
        assert_eq!(
            unsafe {
                RegCreateKeyExW(
                    self.root,
                    path.as_ptr(),
                    0,
                    std::ptr::null(),
                    REG_OPTION_VOLATILE,
                    KEY_ALL_ACCESS,
                    std::ptr::null(),
                    &raw mut key,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: key is open and DWORD data covers exactly four bytes.
        assert_eq!(
            unsafe {
                RegSetValueExW(
                    key,
                    wide("ProxyEnable").as_ptr(),
                    0,
                    REG_DWORD,
                    (&raw const enabled).cast(),
                    4,
                )
            },
            0
        );
        for (name, value) in [("ProxyServer", server), ("ProxyOverride", bypass)] {
            let value = wide(value);
            // SAFETY: key and UTF-16 buffers are live and size includes the terminator.
            assert_eq!(
                unsafe {
                    RegSetValueExW(
                        key,
                        wide(name).as_ptr(),
                        0,
                        REG_SZ,
                        value.as_ptr().cast(),
                        u32::try_from(value.len() * 2).unwrap(),
                    )
                },
                0
            );
        }
        // SAFETY: this method owns key and no longer uses it.
        assert_eq!(unsafe { RegCloseKey(key) }, 0);
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        // SAFETY: remove this process-only override before closing its backing key.
        unsafe {
            RegOverridePredefKey(HKEY_CURRENT_USER, std::ptr::null_mut());
        }
        // SAFETY: root is owned and no longer referenced by the predefined key.
        unsafe {
            RegCloseKey(self.root);
        }
        // SAFETY: the name identifies only this child's self-created test key.
        unsafe {
            RegDeleteTreeW(self.original, self.path.as_ptr());
        }
        // SAFETY: original is owned and is not used again.
        unsafe {
            RegCloseKey(self.original);
        }
    }
}

fn isolated(name: &str, body: impl FnOnce(&Registry, &Path)) {
    if let Some(root) = std::env::var_os("JCHTOOLS_GIT_PROXY_CHILD") {
        let registry = Registry::new();
        body(&registry, Path::new(&root));
        return;
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap();
    let workspace = repo.join(".tmp/git-system-proxy");
    assert!(Command::new("python")
        .arg(repo.join("scripts/make_tmp.py"))
        .args(["workspace", "--destination"])
        .arg(&workspace)
        .status()
        .unwrap()
        .success());
    let temp = tempfile::tempdir_in(workspace).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args([
            "--exact",
            &format!("git_tools::command::tests::{name}"),
            "--nocapture",
            "--test-threads=1",
        ])
        .env("JCHTOOLS_GIT_PROXY_CHILD", temp.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", temp.path().join("no-global-config"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    for name in [
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
    ] {
        child.env_remove(name);
    }
    let output = child.output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Endpoint {
    address: std::net::SocketAddr,
    requests: mpsc::Receiver<String>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Endpoint {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (sender, requests) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Windows accept 会继承监听器的非阻塞模式，读取完整头之前恢复阻塞。
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(10)))
                            .unwrap();
                        let request = read_headers(&mut stream);
                        sender.send(request).unwrap();
                        stream.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("{error}"),
                }
            }
        });
        Self {
            address,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn url(&self, scheme: &str) -> String {
        format!("{scheme}://{}/repo.git", self.address)
    }
    fn proxy(&self) -> String {
        format!("localhost:{}", self.address.port())
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn read_headers(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        if stream.read(&mut byte).unwrap() == 0 {
            break;
        }
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes).unwrap()
}

fn git_ok(root: &Path, args: &[&str]) {
    let output = Command::new(find_git().unwrap())
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repository(root: &Path) {
    git_ok(root, &["init", "-q", "-b", "main"]);
    git_ok(root, &["config", "user.name", "Git Proxy Test"]);
    git_ok(root, &["config", "user.email", "proxy@test.invalid"]);
    git_ok(root, &["commit", "-q", "--allow-empty", "-m", "seed"]);
    git_ok(
        root,
        &["remote", "add", "origin", "http://unused.invalid/repo.git"],
    );
    git_ok(root, &["config", "branch.main.remote", "origin"]);
    git_ok(root, &["config", "branch.main.merge", "refs/heads/main"]);
}

fn request(root: &Path, url: &str, push: bool) {
    git_ok(root, &["remote", "set-url", "origin", url]);
    let args: &[&str] = if push {
        &["push", "--porcelain", "--no-recurse-submodules"]
    } else {
        &["fetch", "--no-recurse-submodules", "origin"]
    };
    let output = run_git_network(
        &find_git().unwrap(),
        root,
        "origin",
        args,
        &Control::default(),
    )
    .unwrap();
    assert!(!output.status.success(), "夹具以真实 HTTP 502 拒绝请求");
}

#[test]
fn system_proxy_routes_http_and_https() {
    isolated("system_proxy_routes_http_and_https", |registry, root| {
        repository(root);
        for scheme in ["http", "https"] {
            for push in [false, true] {
                let proxy = Endpoint::new();
                let origin = Endpoint::new();
                registry.set(1, &format!("{scheme}={}", proxy.proxy()), "");
                request(root, &origin.url(scheme), push);
                let request = proxy
                    .requests
                    .try_recv()
                    .expect("真实 Git 请求必须经过系统代理（域名和动态端口）");
                assert!(
                    request.starts_with(if scheme == "https" {
                        "CONNECT "
                    } else {
                        "GET http://"
                    }),
                    "{request}"
                );
                assert!(
                    origin.requests.try_recv().is_err(),
                    "代理失败不得强制直连源站"
                );
            }
        }
    });
}

#[test]
fn explicit_proxy_and_bypass_take_precedence() {
    isolated(
        "explicit_proxy_and_bypass_take_precedence",
        |registry, root| {
            repository(root);
            let system = Endpoint::new();
            let explicit = Endpoint::new();
            let origin = Endpoint::new();
            let url = origin.url("http");
            registry.set(1, &system.proxy(), "");
            for key in [
                "http.proxy".to_string(),
                "remote.origin.proxy".to_string(),
                format!("http.{url}.proxy"),
            ] {
                git_ok(
                    root,
                    &["config", &key, &format!("http://{}", explicit.address)],
                );
                request(root, &url, false);
                assert!(explicit
                    .requests
                    .try_recv()
                    .unwrap()
                    .starts_with("GET http://"));
                assert!(system.requests.try_recv().is_err());
                git_ok(root, &["config", &key, ""]);
                request(root, &url, false);
                assert!(origin
                    .requests
                    .try_recv()
                    .unwrap()
                    .starts_with("GET /repo.git/"));
                assert!(system.requests.try_recv().is_err());
                git_ok(root, &["config", "--unset", &key]);
            }
            for (name, scheme) in [
                ("http_proxy", "http"),
                ("HTTPS_PROXY", "https"),
                ("ALL_PROXY", "http"),
            ] {
                std::env::set_var(name, format!("http://{}", explicit.address));
                request(root, &origin.url(scheme), false);
                assert!(explicit
                    .requests
                    .try_recv()
                    .unwrap()
                    .starts_with(if scheme == "https" {
                        "CONNECT "
                    } else {
                        "GET http://"
                    }));
                assert!(system.requests.try_recv().is_err());
                std::env::remove_var(name);
            }
            std::env::set_var("no_proxy", "127.0.0.1");
            request(root, &url, false);
            assert!(origin
                .requests
                .try_recv()
                .unwrap()
                .starts_with("GET /repo.git/"));
            assert!(system.requests.try_recv().is_err());
            std::env::set_var("no_proxy", "unrelated.invalid");
            request(root, &url, false);
            assert!(system
                .requests
                .try_recv()
                .unwrap()
                .starts_with("GET http://"));
            assert!(origin.requests.try_recv().is_err());
        },
    );
}

#[test]
fn system_bypass_and_disabled_proxy_remain_direct() {
    isolated(
        "system_bypass_and_disabled_proxy_remain_direct",
        |registry, root| {
            repository(root);
            let system = Endpoint::new();
            let origin = Endpoint::new();
            for (enabled, bypass) in [(1, "unrelated.invalid;127.*"), (0, "")] {
                registry.set(enabled, &system.proxy(), bypass);
                request(root, &origin.url("http"), false);
                assert!(origin
                    .requests
                    .try_recv()
                    .unwrap()
                    .starts_with("GET /repo.git/"));
                assert!(system.requests.try_recv().is_err());
            }
            registry.set(1, &system.proxy(), "<local>");
            request(
                root,
                &format!("http://localhost:{}/repo.git", origin.address.port()),
                false,
            );
            assert!(origin
                .requests
                .try_recv()
                .unwrap()
                .starts_with("GET /repo.git/"));
            assert!(system.requests.try_recv().is_err());
        },
    );
}
