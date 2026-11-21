// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The event loop (`php_cli_server_do_event_loop`): a `poll()`-driven,
//! single-threaded server that reads requests from many connections,
//! runs PHP one request at a time and streams static files and error
//! pages without blocking.

use crate::log::{self, LOG_ERROR, LOG_MESSAGE, LOG_PROCESS};
use crate::parser::{Method, Parser, State};
use crate::request::RequestReader;
use crate::sapi::{Outcome, RequestCtx, ServerConfig};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// Cleared by SIGINT (`php_cli_server_sigint_handler`).
static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn on_sigint(_sig: libc::c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}

/// `strerror(err)` without std's ` (os error N)` suffix.
pub fn strerror(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error ") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

/// Pending output of a connection (`php_cli_server_content_sender`).
#[derive(Debug, Default)]
struct Sender {
    buf: Vec<u8>,
    pos: usize,
    file: Option<File>,
}

/// One connection (`php_cli_server_client`).
#[derive(Debug)]
struct Client {
    stream: TcpStream,
    addr: String,
    parser: Parser,
    reader: RequestReader,
    sender: Option<Sender>,
}

/// Result of reading from a connection.
enum ReadResult {
    /// More data is needed.
    Pending,
    /// The request is complete.
    Complete,
    /// The connection failed; the message (if any) is logged.
    Failed(Option<String>),
}

/// The listening server (`php_cli_server`).
#[derive(Debug)]
pub struct Server {
    listener: TcpListener,
    clients: BTreeMap<RawFd, Client>,
    config: Arc<ServerConfig>,
    post_max_size: i64,
    /// The VM after module startup; each request runs on a forked copy.
    template: Option<crate::sapi::PreparedVm>,
}

/// `php_cli_server_parse_addr()`: `host:port` or `[host]:port`.
pub fn parse_addr(addr: &str) -> Option<(String, u16)> {
    fn port_of(s: &str) -> Option<u16> {
        // strtol: optional sign and digits, then nothing else.
        let digits = s.strip_prefix('+').unwrap_or(s);
        if s.starts_with('-') {
            let v: i64 = s.parse().ok()?;
            return u16::try_from(v).ok();
        }
        if digits.is_empty() {
            return Some(0);
        }
        if !digits.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let v: u64 = digits.parse().ok()?;
        u16::try_from(v).ok()
    }
    if let Some(rest) = addr.strip_prefix('[') {
        let end = rest.find(']')?;
        let after = &rest[end + 1..];
        let port = port_of(after.strip_prefix(':')?)?;
        return Some((rest[..end].to_string(), port));
    }
    let colon = addr.find(':')?;
    let port = port_of(&addr[colon + 1..])?;
    Some((addr[..colon].to_string(), port))
}

/// `php_network_listen_socket()`: binds the first usable address of
/// `host`; returns the listener or the error text.
pub fn listen(host: &str, port: u16) -> Result<TcpListener, String> {
    let addrs: Vec<SocketAddr> = match (host, port).to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(e) => {
            let msg = e.to_string();
            let reason = msg
                .strip_prefix("failed to lookup address information: ")
                .unwrap_or(&msg)
                .to_string();
            return Err(format!(
                "php_network_getaddresses: getaddrinfo for {host} failed: {reason}"
            ));
        }
    };
    let mut last_err = None;
    for a in addrs {
        match TcpListener::bind(a) {
            Ok(l) => return Ok(l),
            Err(e) => {
                let fatal = matches!(e.raw_os_error(), Some(libc::EADDRINUSE | libc::EINVAL));
                last_err = Some(strerror(&e));
                if fatal {
                    break;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| "?".to_string()))
}

/// `poll()` over `fds` with a timeout in milliseconds.
fn poll(fds: &mut [libc::pollfd], timeout_ms: i32) -> std::io::Result<usize> {
    let n = libc::nfds_t::try_from(fds.len()).unwrap_or(libc::nfds_t::MAX);
    // SAFETY: `fds` is a valid, exclusively borrowed slice of pollfd
    // structures and `n` is its length.
    let r = unsafe { libc::poll(fds.as_mut_ptr(), n, timeout_ms) };
    if r < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(usize::try_from(r).unwrap_or(0))
    }
}

/// Installs the SIGINT handler that stops the loop.
pub fn install_sigint() {
    let handler = on_sigint as extern "C" fn(libc::c_int);
    // SAFETY: the handler only stores to an atomic, which is
    // async-signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
    }
}

impl Server {
    /// A server on `listener`.
    pub fn new(listener: TcpListener, config: ServerConfig, post_max_size: i64) -> Self {
        let config = Arc::new(config);
        // Module startup happens once, before the banner (as in PHP);
        // requests run in forked children of this process.
        let template = Some(crate::sapi::prepare_vm(&config));
        Server {
            listener,
            clients: BTreeMap::new(),
            config,
            post_max_size,
            template,
        }
    }

    /// `php_cli_server_do_event_loop()`: returns the exit status.
    pub fn run(&mut self) -> i32 {
        while RUNNING.load(Ordering::SeqCst) {
            let mut fds = vec![libc::pollfd {
                fd: self.listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            for (&fd, c) in &self.clients {
                let events = if c.sender.is_some() {
                    libc::POLLOUT
                } else {
                    libc::POLLIN
                };
                fds.push(libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                });
            }
            match poll(&mut fds, 1000) {
                Ok(0) => {}
                Ok(_) => {
                    for p in &fds {
                        if p.revents == 0 {
                            continue;
                        }
                        if p.fd == self.listener.as_raw_fd() {
                            self.accept();
                        } else if p.revents & libc::POLLOUT != 0 {
                            self.send_event(p.fd);
                        } else {
                            self.read_event(p.fd);
                        }
                    }
                }
                Err(e) => {
                    if e.kind() != ErrorKind::Interrupted {
                        log::logf(LOG_ERROR, &strerror(&e));
                        return 1;
                    }
                }
            }
        }
        0
    }

    fn accept(&mut self) {
        match self.listener.accept() {
            Ok((stream, addr)) => {
                if stream.set_nonblocking(true).is_err() {
                    return;
                }
                let addr = addr.to_string();
                log::logf(LOG_MESSAGE, &format!("{addr} Accepted"));
                let fd = stream.as_raw_fd();
                self.clients.insert(
                    fd,
                    Client {
                        stream,
                        addr,
                        parser: Parser::new(),
                        reader: RequestReader::new(&self.config.document_root, self.post_max_size),
                        sender: None,
                    },
                );
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => {
                log::logf(
                    LOG_ERROR,
                    &format!("Failed to accept a client (reason: {})", strerror(&e)),
                );
            }
        }
    }

    /// `php_cli_server_close_connection()`.
    fn close(&mut self, fd: RawFd) {
        if let Some(c) = self.clients.remove(&fd) {
            log::logf(LOG_MESSAGE, &format!("{} Closing", c.addr));
            let _ = c.stream.shutdown(std::net::Shutdown::Both);
        }
    }

    /// `php_cli_server_client_read_request()`.
    fn read_request(c: &mut Client) -> ReadResult {
        if c.reader.request_read {
            return ReadResult::Complete;
        }
        let mut buf = vec![0u8; 16383];
        let n = match c.stream.read(&mut buf) {
            Ok(0) => return ReadResult::Failed(Some("Unexpected EOF".to_string())),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return ReadResult::Pending,
            Err(e) if e.kind() == ErrorKind::Interrupted => return ReadResult::Pending,
            Err(e) => return ReadResult::Failed(Some(strerror(&e))),
        };
        let consumed = c.parser.execute(&mut c.reader, &buf[..n]);
        if consumed != n && !c.reader.too_large_post {
            let msg = if buf[0] & 0x80 != 0 || buf[0] == 0x16 {
                "Unsupported SSL request"
            } else {
                "Malformed HTTP request"
            };
            return ReadResult::Failed(Some(msg.to_string()));
        }
        if c.reader.expect_continue && !c.reader.request_read {
            c.reader.expect_continue = false;
            if let Err(e) = send_blocking(&c.stream, b"HTTP/1.1 100 Continue\r\n\r\n") {
                return ReadResult::Failed(Some(strerror(&e)));
            }
        }
        if c.reader.request_read {
            ReadResult::Complete
        } else {
            ReadResult::Pending
        }
    }

    /// `php_cli_server_recv_event_read_request()`.
    fn read_event(&mut self, fd: RawFd) {
        let Some(c) = self.clients.get_mut(&fd) else {
            return;
        };
        match Self::read_request(c) {
            ReadResult::Pending => {}
            ReadResult::Failed(err) => {
                if let Some(err) = err {
                    if err == "Unexpected EOF" && c.parser.state() == State::StartReq {
                        log::logf(
                            LOG_MESSAGE,
                            &format!(
                                "{} Closed without sending a request; it was probably just an unused speculative preconnection",
                                c.addr
                            ),
                        );
                    } else {
                        log::logf(LOG_ERROR, &format!("{} Invalid request ({err})", c.addr));
                    }
                }
                self.close(fd);
            }
            ReadResult::Complete => {
                if c.reader.request.method == Method::NotImplemented {
                    self.send_error_page(fd, 501, None);
                } else if c.reader.too_large_post {
                    self.send_error_page(fd, 413, None);
                } else {
                    self.dispatch(fd);
                }
            }
        }
    }

    /// `php_cli_server_send_error_page()`: queues the page.
    fn send_error_page(&mut self, fd: RawFd, status: i32, errstr: Option<String>) {
        let errstr = errstr.unwrap_or_else(|| strerror(&std::io::Error::last_os_error()));
        let Some(c) = self.clients.get_mut(&fd) else {
            return;
        };
        let req = &c.reader.request;
        let page = crate::status::error_page(
            req.protocol_version,
            status,
            req.uri(),
            req.method == Method::Head,
            self.post_max_size,
            crate::status::now(),
        );
        crate::sapi::log_response(&c.addr, req, status, Some(&errstr), None);
        c.sender = Some(Sender {
            buf: page,
            pos: 0,
            file: None,
        });
    }

    /// `php_cli_server_begin_send_static()`.
    fn begin_send_static(&mut self, fd: RawFd) {
        let Some(c) = self.clients.get(&fd) else {
            return;
        };
        let req = &c.reader.request;
        if matches!(req.method, Method::Delete | Method::Put | Method::Patch) {
            self.send_error_page(fd, 405, None);
            return;
        }
        if req.path_translated.as_ref().is_some_and(|p| p.contains(&0)) {
            self.send_error_page(fd, 400, None);
            return;
        }
        let file = match &req.path_translated {
            Some(p) => File::open(std::ffi::OsStr::from_bytes(p)),
            None => Err(std::io::Error::from_raw_os_error(libc::ENOENT)),
        };
        let file = match file {
            Ok(f) => f,
            Err(e) => {
                let msg = strerror(&e);
                self.send_error_page(fd, 404, Some(msg));
                return;
            }
        };
        let mut head = crate::status::status_line(req.protocol_version, 200);
        crate::status::essential_headers(&mut head, true, crate::status::now());
        if let Some(mime) = crate::mime::lookup(&req.ext) {
            head.extend_from_slice(b"Content-Type: ");
            head.extend_from_slice(mime.as_bytes());
            if mime.starts_with("text/") {
                head.extend_from_slice(b"; charset=UTF-8");
            }
            head.extend_from_slice(b"\r\n");
        }
        head.extend_from_slice(format!("Content-Length: {}\r\n\r\n", req.file_size).as_bytes());
        crate::sapi::log_response(&c.addr, req, 200, None, None);
        let file = (req.method != Method::Head).then_some(file);
        if let Some(c) = self.clients.get_mut(&fd) {
            c.sender = Some(Sender {
                buf: head,
                pos: 0,
                file,
            });
        }
    }

    /// `php_cli_server_send_event()`.
    fn send_event(&mut self, fd: RawFd) {
        let Some(c) = self.clients.get_mut(&fd) else {
            return;
        };
        let Some(s) = c.sender.as_mut() else {
            return;
        };
        let close = 'send: {
            if s.pos >= s.buf.len() {
                if let Some(f) = s.file.as_mut() {
                    let mut chunk = vec![0u8; 131_072];
                    match f.read(&mut chunk) {
                        Ok(0) => s.file = None,
                        Ok(n) => {
                            chunk.truncate(n);
                            s.buf = chunk;
                            s.pos = 0;
                        }
                        Err(e) => {
                            log::logf(LOG_ERROR, &strerror(&e));
                            break 'send true;
                        }
                    }
                }
            }
            while s.pos < s.buf.len() {
                match c.stream.write(&s.buf[s.pos..]) {
                    Ok(n) => s.pos += n,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break 'send false,
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(_) => break 'send true,
                }
            }
            s.file.is_none()
        };
        if close {
            self.close(fd);
        }
    }

    /// `php_cli_server_dispatch()`.
    fn dispatch(&mut self, fd: RawFd) {
        let Some(c) = self.clients.get_mut(&fd) else {
            return;
        };
        let is_static = !c.reader.request.is_php_script();
        if self.config.router.is_none() && is_static {
            self.begin_send_static(fd);
            return;
        }
        let outcome = if self.template.is_some() {
            run_php_forked(c, &mut self.template, &self.config, is_static)
        } else {
            run_php(c, Prepared::spawn(&self.config), is_static)
        };
        match outcome {
            Outcome::Done => self.close(fd),
            Outcome::ServeStatic => self.begin_send_static(fd),
        }
    }
}

/// Sends `data` on a non-blocking socket, waiting up to 10 seconds for
/// it to become writable (`php_cli_server_client_send_through`).
fn send_blocking(stream: &TcpStream, data: &[u8]) -> std::io::Result<()> {
    let mut pos = 0;
    let mut s = stream;
    while pos < data.len() {
        match s.write(&data[pos..]) {
            Ok(n) => pos += n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                let mut fds = [libc::pollfd {
                    fd: stream.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                }];
                if poll(&mut fds, 10_000)? == 0 {
                    return Err(std::io::Error::from(ErrorKind::TimedOut));
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// A request handed to a [`Prepared`] thread.
struct Job {
    request: crate::request::Request,
    stream: TcpStream,
    addr: String,
    is_static: bool,
}

/// A thread holding a ready VM, waiting for its request (fresh
/// thread-locals and a large stack per request).
#[derive(Debug)]
struct Prepared {
    jobs: mpsc::Sender<Job>,
    done: mpsc::Receiver<Outcome>,
}

impl Prepared {
    fn spawn(config: &Arc<ServerConfig>) -> Option<Prepared> {
        let (jtx, jrx) = mpsc::channel::<Job>();
        let (dtx, drx) = mpsc::channel::<Outcome>();
        let config = config.clone();
        std::thread::Builder::new()
            .name("php-request".to_string())
            .stack_size(256 << 20)
            .spawn(move || {
                let vm = crate::sapi::prepare_vm(&config);
                let Ok(job) = jrx.recv() else {
                    return;
                };
                let ctx = Rc::new(RequestCtx::new(job.request, job.stream, job.addr));
                let (outcome, vm) = crate::sapi::run_request(vm, &ctx, &config, job.is_static);
                let _ = dtx.send(outcome);
                drop(vm);
                drop(ctx);
            })
            .ok()?;
        Some(Prepared {
            jobs: jtx,
            done: drx,
        })
    }
}

/// Runs the PHP part of a request on the prepared thread and waits for it.
fn run_php(c: &mut Client, prepared: Option<Prepared>, is_static: bool) -> Outcome {
    let Ok(stream) = c.stream.try_clone() else {
        return Outcome::Done;
    };
    let Some(p) = prepared else {
        return Outcome::Done;
    };
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    // The body moves to the request thread (it can be large); the rest of
    // the request stays for logging and static serving.
    let content = c.reader.request.content.take();
    let mut request = c.reader.request.clone();
    request.content = content;
    let job = Job {
        request,
        stream,
        addr: c.addr.clone(),
        is_static,
    };
    let outcome = if p.jobs.send(job).is_ok() {
        p.done.recv().unwrap_or(Outcome::Done)
    } else {
        Outcome::Done
    };
    let _ = c.stream.set_nonblocking(true);
    outcome
}

/// Exit status of a request child that asks for the static file.
const EXIT_SERVE_STATIC: i32 = 10;

/// Runs the PHP part of a request in a forked child of the server, which
/// inherits the started-up `template` VM (PHP's per-request startup
/// without paying for module startup again) and leaves the server's state
/// untouched.  Waits for the child, as PHP's server is single-threaded.
fn run_php_forked(
    c: &mut Client,
    template: &mut Option<crate::sapi::PreparedVm>,
    config: &Arc<ServerConfig>,
    is_static: bool,
) -> Outcome {
    let _ = std::io::stderr().flush();
    let server_pid = crate::sapi::server_pid();
    // SAFETY: the server thread is the only one doing work; the child only
    // continues on this thread with its copy of the address space and ends
    // with `_exit`.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return run_php(c, Prepared::spawn(config), is_static);
    }
    if pid == 0 {
        crate::sapi::set_server_pid(server_pid);
        // SAFETY: prctl has no memory-safety preconditions.
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
        }
        let code = match (c.stream.try_clone(), template.take()) {
            (Ok(stream), Some(mut vm)) => {
                // The child ends with `_exit` after this request.
                vm.vm.set_fast_shutdown(true);
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
                let request = std::mem::take(&mut c.reader.request);
                let ctx = Rc::new(RequestCtx::new(request, stream, c.addr.clone()));
                let (outcome, vm) = crate::sapi::run_request(vm, &ctx, config, is_static);
                // The process ends right away: skip the VM's destruction.
                std::mem::forget(vm);
                if outcome == Outcome::Done {
                    // The response is complete: let the client see EOF now.
                    if let Some(stream) = &ctx.stream {
                        let _ = stream.shutdown(std::net::Shutdown::Write);
                    }
                }
                std::mem::forget(ctx);
                match outcome {
                    Outcome::Done => 0,
                    Outcome::ServeStatic => EXIT_SERVE_STATIC,
                }
            }
            _ => 0,
        };
        let _ = std::io::stderr().flush();
        // SAFETY: ends the child without running the parent's destructors.
        unsafe { libc::_exit(code) }
    }
    let mut status = 0;
    loop {
        // SAFETY: waitpid writes the status into our local.
        let r = unsafe { libc::waitpid(pid, &raw mut status, 0) };
        if r == pid {
            break;
        }
        if r < 0 && std::io::Error::last_os_error().kind() != ErrorKind::Interrupted {
            return Outcome::Done;
        }
    }
    let _ = c.stream.set_nonblocking(true);
    if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == EXIT_SERVE_STATIC {
        Outcome::ServeStatic
    } else {
        Outcome::Done
    }
}

/// `php_cli_server_startup_workers()`: forks `PHP_CLI_SERVER_WORKERS`
/// processes sharing the listening socket.  Returns the worker PIDs (in
/// the master; empty in workers and without workers).
pub fn startup_workers() -> Vec<libc::pid_t> {
    let Some(workers) = std::env::var_os("PHP_CLI_SERVER_WORKERS") else {
        return Vec::new();
    };
    let n = crate::atol(workers.as_bytes());
    if n <= 1 {
        log::raw(b"number of workers must be larger than 1\n");
        return Vec::new();
    }
    log::set_with_pid(true);
    // SAFETY: getpid has no preconditions.
    let master = unsafe { libc::getpid() };
    let mut pids = Vec::new();
    for _ in 0..n {
        // SAFETY: the process is single-threaded at this point (no request
        // thread has been started), so the child may continue normally.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            break;
        }
        if pid == 0 {
            // SAFETY: prctl/getppid have no memory-safety preconditions.
            unsafe {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                if libc::getppid() != master {
                    libc::_exit(1);
                }
            }
            log::set_pid(std::process::id());
            return Vec::new();
        }
        pids.push(pid);
    }
    pids
}

/// Waits for the workers to exit (`php_cli_server_dtor`).
pub fn wait_workers(pids: &[libc::pid_t]) {
    for &pid in pids {
        let mut status = 0;
        loop {
            // SAFETY: waitpid writes the status into our local.
            let r = unsafe { libc::waitpid(pid, &raw mut status, 0) };
            if r == -1 || libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
                break;
            }
        }
    }
}

/// Logs the startup banner.
pub fn banner(host: &str, port: u16) {
    let (l, r) = if host.contains(':') {
        ("[", "]")
    } else {
        ("", "")
    };
    log::logf(
        LOG_PROCESS,
        &format!(
            "PHP {} Development Server (http://{l}{host}{r}:{port}) started",
            php_vm::PHP_VERSION
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(
            parse_addr("localhost:8080"),
            Some(("localhost".into(), 8080))
        );
        assert_eq!(parse_addr("[::1]:80"), Some(("::1".into(), 80)));
        assert_eq!(parse_addr("localhost:0"), Some(("localhost".into(), 0)));
        assert_eq!(parse_addr("localhost"), None);
        assert_eq!(parse_addr("localhost:x"), None);
        assert_eq!(parse_addr("localhost:70000"), None);
        assert_eq!(parse_addr("[::1]80"), None);
        assert_eq!(parse_addr("[::1:80"), None);
        assert_eq!(parse_addr("h:"), Some(("h".into(), 0)));
    }

    #[test]
    fn listen_errors() {
        let e = listen("192.168.1.999", 8080).unwrap_err();
        assert!(
            e.starts_with("php_network_getaddresses: getaddrinfo for 192.168.1.999 failed"),
            "{e}"
        );
        let l = listen("127.0.0.1", 0).unwrap();
        let port = l.local_addr().unwrap().port();
        assert_eq!(
            listen("127.0.0.1", port).unwrap_err(),
            "Address already in use"
        );
    }

    #[test]
    fn error_strings() {
        assert_eq!(
            strerror(&std::io::Error::from_raw_os_error(libc::ENOENT)),
            "No such file or directory"
        );
    }
}
