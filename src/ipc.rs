//! The local line between the thin clients (CLI, MCP) and core. It differs by platform;
//! everything else talks to this module.
//!
//! - Unix: a socket at `~/.nekorisu/run/core.sock`, inside the private nekorisu home.
//! - Windows: a named pipe `\\.\pipe\nekorisu-<16 hex>`, the hex taken from the SHA-256
//!   of the nekorisu home's path, so two homes (tests, say) never meet. Only this user
//!   (by account, whether elevated or not) and SYSTEM may open it, and only from this machine.
//!   Pipe names are open to every account on the machine, so a client also checks that the
//!   pipe it reached is owned by this user before talking to it.

use std::path::Path;

use sha2::{Digest, Sha256};

pub use imp::*;

/// The pipe's name for a nekorisu home. Clients and core of different versions must
/// agree on it, so the derivation never changes.
#[cfg_attr(unix, allow(dead_code))]
pub fn pipe_name(home: &Path) -> String {
    let digest = Sha256::digest(home.as_os_str().as_encoded_bytes());
    format!(r"\\.\pipe\nekorisu-{}", &hex::encode(digest)[..16])
}

#[cfg(unix)]
mod imp {
    use std::io;

    use anyhow::Context;
    use tokio::net::{UnixListener, UnixStream};
    use tokio_stream::wrappers::UnixListenerStream;

    use crate::paths::Paths;

    pub type Connection = UnixStream;
    pub type Incoming = UnixListenerStream;

    /// What core listens on, for messages.
    pub fn describe(paths: &Paths) -> String {
        paths.socket().display().to_string()
    }

    pub async fn listen(paths: &Paths) -> anyhow::Result<Incoming> {
        let socket = paths.socket();
        let _ = std::fs::remove_file(&socket); // left over from a core that didn't exit cleanly
        let listener = UnixListener::bind(&socket).with_context(|| format!("监听不了 {}", socket.display()))?;
        Ok(UnixListenerStream::new(listener))
    }

    /// After core stops serving.
    pub fn after_shutdown(paths: &Paths) {
        let _ = std::fs::remove_file(paths.socket());
    }

    pub async fn connect(paths: &Paths) -> io::Result<Connection> {
        UnixStream::connect(paths.socket()).await
    }
}

#[cfg(windows)]
mod imp {
    use std::future::Future;
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::pin::Pin;
    use std::task::{Context, Poll, ready};
    use std::time::{Duration, Instant};

    use anyhow::Context as _;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions};
    use tokio_stream::Stream;
    use tonic::transport::server::Connected;
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
        SE_KERNEL_OBJECT,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::pipe_name;
    use crate::paths::Paths;
    use crate::timefmt::now_ms;

    pub type Connection = NamedPipeClient;

    pub fn describe(paths: &Paths) -> String {
        pipe_name(&paths.home)
    }

    pub async fn listen(paths: &Paths) -> anyhow::Result<Incoming> {
        let name = pipe_name(&paths.home);
        let descriptor = Descriptor::new().context("建不了管道的访问权限")?;
        // Right after a handover the previous core may still be letting go of its pipe.
        let deadline = Instant::now() + Duration::from_secs(5);
        let first = loop {
            match descriptor.create(&name, true) {
                Ok(server) => break server,
                Err(err) if is(&err, ERROR_ACCESS_DENIED) && Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(err) => return Err(err).with_context(|| format!("监听不了 {name}")),
            }
        };
        Ok(Incoming {
            name,
            descriptor,
            accept: Some(accept(first)),
        })
    }

    pub fn after_shutdown(_paths: &Paths) {}

    pub async fn connect(paths: &Paths) -> io::Result<Connection> {
        let name = pipe_name(&paths.home);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match ClientOptions::new().open(&name) {
                Ok(pipe) => {
                    check_owner(&pipe, &name)?;
                    return Ok(pipe);
                }
                // Every instance is taken for the moment; core makes a new one right away.
                Err(err) if is(&err, ERROR_PIPE_BUSY) && Instant::now() < deadline => {}
                Err(err) => return Err(err),
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Core makes this user the pipe's owner. Another account on this machine can create a
    /// pipe of the same name while core isn't running, but can't make this user its owner;
    /// talking to that pipe would hand it our requests and let it answer in core's place.
    fn check_owner(pipe: &NamedPipeClient, name: &str) -> io::Result<()> {
        if owner_sid(pipe.as_raw_handle())? == user_sid()? {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{name} 不是你自己的 core 建的，不连它；可能是这台电脑上的别的账户占了这个名字"),
        ))
    }

    /// The owner of the object behind `handle`, as a SID string.
    fn owner_sid(handle: HANDLE) -> io::Result<String> {
        // SAFETY: a valid handle and out-pointers. `owner` points into `descriptor`, which is
        // freed with LocalFree only after the SID has been copied into a String.
        unsafe {
            let mut owner = std::ptr::null_mut();
            let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let status = GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut descriptor,
            );
            if status != 0 {
                return Err(io::Error::from_raw_os_error(status as i32));
            }
            let sid = sid_string(owner);
            LocalFree(descriptor);
            sid
        }
    }

    fn is(err: &io::Error, code: u32) -> bool {
        err.raw_os_error() == Some(code as i32)
    }

    /// The pipe's owner is this user's account; who may open it: that account and SYSTEM,
    /// nobody else; nothing inherited. The account is named by its SID rather than as the
    /// owner (`OW`), and the owner is set explicitly: an elevated administrator's objects are
    /// owned by the Administrators group by default, which would lock out the same user's
    /// non-elevated agents and fail their owner check (`check_owner`).
    pub(super) fn sddl(user_sid: &str) -> String {
        format!("O:{user_sid}D:P(A;;GA;;;{user_sid})(A;;GA;;;SY)")
    }

    /// This process's user account as a SID string (`S-1-5-21-…`).
    pub fn user_sid() -> io::Result<String> {
        // SAFETY: plain Win32 calls with valid out-pointers. The token handle is closed by
        // OwnedHandle; the buffer is u64-aligned, which TOKEN_USER needs, and sized as the first
        // call asked; the SID string is freed with LocalFree as documented.
        unsafe {
            let mut raw: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) == 0 {
                return Err(io::Error::last_os_error());
            }
            let token = OwnedHandle::from_raw_handle(raw);
            let mut len = 0u32;
            GetTokenInformation(raw, TokenUser, std::ptr::null_mut(), 0, &mut len);
            let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
            if GetTokenInformation(raw, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
                return Err(io::Error::last_os_error());
            }
            drop(token);
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            sid_string(user.User.Sid)
        }
    }

    /// A SID as a string (`S-1-5-…`).
    ///
    /// # Safety
    /// `sid` must point to a valid SID.
    unsafe fn sid_string(sid: *mut std::ffi::c_void) -> io::Result<String> {
        // SAFETY: the caller passes a valid SID; the string Windows allocates is NUL-terminated
        // and freed with LocalFree as documented.
        unsafe {
            let mut text: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(sid, &mut text) == 0 {
                return Err(io::Error::last_os_error());
            }
            let len = (0..).take_while(|&i| *text.add(i) != 0).count();
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            Ok(sid)
        }
    }

    /// The pipe's security descriptor, built once from `sddl`.
    struct Descriptor(PSECURITY_DESCRIPTOR);

    // SAFETY: the descriptor is plain memory owned by this value, only read after creation.
    unsafe impl Send for Descriptor {}

    impl Descriptor {
        fn new() -> io::Result<Self> {
            Self::from_sddl(&sddl(&user_sid()?))
        }

        fn from_sddl(sddl: &str) -> io::Result<Self> {
            let wide: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
            let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: `wide` is NUL-terminated; the descriptor it allocates is freed in Drop.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(descriptor))
        }

        fn create(&self, name: &str, first: bool) -> io::Result<NamedPipeServer> {
            let mut attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.0,
                bInheritHandle: 0,
            };
            // SAFETY: `attributes` is a valid SECURITY_ATTRIBUTES for the duration of the call,
            // and its descriptor lives as long as `self`.
            unsafe {
                ServerOptions::new()
                    .first_pipe_instance(first)
                    .reject_remote_clients(true)
                    .create_with_security_attributes_raw(name, (&raw mut attributes).cast())
            }
        }
    }

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW, freed once.
            unsafe { LocalFree(self.0) };
        }
    }

    type Accept = Pin<Box<dyn Future<Output = (NamedPipeServer, io::Result<()>)> + Send>>;

    fn accept(server: NamedPipeServer) -> Accept {
        Box::pin(async move {
            let result = server.connect().await;
            (server, result)
        })
    }

    /// Accepted connections, for tonic. One instance always waits for the next client;
    /// dropping this closes it, so the pipe is gone as soon as core stops serving.
    pub struct Incoming {
        name: String,
        descriptor: Descriptor,
        accept: Option<Accept>,
    }

    impl Stream for Incoming {
        type Item = io::Result<Pipe>;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let this = self.get_mut();
            let Some(waiting) = this.accept.as_mut() else {
                return Poll::Ready(None);
            };
            let (server, result) = ready!(waiting.as_mut().poll(cx));
            this.accept = match this.descriptor.create(&this.name, false) {
                Ok(next) => Some(accept(next)),
                Err(err) => {
                    eprintln!("{} 建不了新的管道实例，core 不再接受连接：{err}", now_ms());
                    None
                }
            };
            Poll::Ready(Some(result.map(|()| Pipe(server))))
        }
    }

    /// One accepted connection.
    pub struct Pipe(NamedPipeServer);

    impl Connected for Pipe {
        type ConnectInfo = ();

        fn connect_info(&self) -> Self::ConnectInfo {}
    }

    impl AsyncRead for Pipe {
        fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for Pipe {
        fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
        }

        fn poll_write_vectored(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bufs: &[io::IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.get_mut().0).poll_write_vectored(cx, bufs)
        }

        fn is_write_vectored(&self) -> bool {
            self.0.is_write_vectored()
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().0).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
        }
    }

    #[cfg(test)]
    mod tests {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio_stream::StreamExt;

        use super::*;

        /// What Windows reports for the pipe: exactly this user and SYSTEM, and no integrity
        /// label above medium even when core runs elevated, so the same user's non-elevated
        /// agents can still connect.
        #[tokio::test]
        async fn the_pipe_admits_only_this_user_and_system() {
            use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
            use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, LABEL_SECURITY_INFORMATION};

            let home = tempfile::tempdir().unwrap();
            let server = Descriptor::new()
                .unwrap()
                .create(&pipe_name(home.path()), true)
                .unwrap();
            let info = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION;
            // SAFETY: valid handle and out-pointers; both returned buffers are freed with LocalFree.
            let sddl = unsafe {
                let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
                let status = GetSecurityInfo(
                    server.as_raw_handle(),
                    SE_KERNEL_OBJECT,
                    info,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut descriptor,
                );
                assert_eq!(status, 0, "GetSecurityInfo");
                let mut text: *mut u16 = std::ptr::null_mut();
                let ok = ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    descriptor,
                    SDDL_REVISION_1,
                    info,
                    &mut text,
                    std::ptr::null_mut(),
                );
                assert_ne!(ok, 0, "ConvertSecurityDescriptorToStringSecurityDescriptorW");
                let len = (0..).take_while(|&i| *text.add(i) != 0).count();
                let sddl = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
                LocalFree(text.cast());
                LocalFree(descriptor);
                sddl
            };
            // Owned by this user; GA is stored as the pipe's own "all access" (FA).
            let me = user_sid().unwrap();
            let expected = format!("O:{me}D:P(A;;FA;;;{me})(A;;FA;;;SY)");
            assert!(
                sddl.starts_with(&expected),
                "{}",
                sddl.replace(&user_sid().unwrap(), "<user>")
            );
            assert!(
                !sddl.contains(";;;HI)") && !sddl.contains(";;;SI)"),
                "{}",
                sddl.replace(&user_sid().unwrap(), "<user>")
            );
        }

        /// A pipe of core's name that someone else owns is refused. Here it is owned by the
        /// Administrators group, which only an elevated process may set; otherwise skipped.
        #[tokio::test]
        async fn a_pipe_someone_else_owns_is_refused() {
            let home = tempfile::tempdir().unwrap();
            let paths = Paths {
                home: home.path().to_path_buf(),
            };
            let me = user_sid().unwrap();
            let squatter = Descriptor::from_sddl(&format!("O:BAD:P(A;;GA;;;{me})"))
                .and_then(|descriptor| descriptor.create(&pipe_name(&paths.home), true));
            let Ok(squatter) = squatter else {
                eprintln!("not elevated, skipped");
                return;
            };
            let err = connect(&paths).await.expect_err("a pipe owned by someone else");
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            drop(squatter);
        }

        /// No pipe to open: core isn't serving.
        fn gone(paths: &Paths) -> bool {
            matches!(ClientOptions::new().open(pipe_name(&paths.home)), Err(err) if !is(&err, ERROR_PIPE_BUSY))
        }

        #[tokio::test]
        async fn one_core_per_pipe_and_a_round_trip() {
            let home = tempfile::tempdir().unwrap();
            let paths = Paths {
                home: home.path().to_path_buf(),
            };
            assert!(gone(&paths));
            let mut incoming = listen(&paths).await.unwrap();
            let second = Descriptor::new().unwrap().create(&pipe_name(&paths.home), true);
            assert!(second.is_err(), "a second core can't take the pipe");
            assert!(user_sid().unwrap().starts_with("S-1-5-"));

            let client = tokio::spawn({
                let paths = paths.clone();
                async move {
                    let mut pipe = connect(&paths).await.unwrap();
                    pipe.write_all(b"ping").await.unwrap();
                    let mut answer = [0u8; 4];
                    pipe.read_exact(&mut answer).await.unwrap();
                    answer
                }
            });
            let mut server = incoming.next().await.unwrap().unwrap();
            let mut got = [0u8; 4];
            server.read_exact(&mut got).await.unwrap();
            assert_eq!(&got, b"ping");
            server.write_all(b"pong").await.unwrap();
            assert_eq!(&client.await.unwrap(), b"pong");

            assert!(!gone(&paths));
            drop(server);
            drop(incoming);
            // The handles close once the runtime has cancelled their pending I/O.
            let deadline = Instant::now() + Duration::from_secs(2);
            while !gone(&paths) {
                assert!(Instant::now() < deadline, "no pipe once core stops serving");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_is_fixed_per_home() {
        let name = pipe_name(Path::new(r"C:\Users\me\.nekorisu"));
        // sha256("C:\Users\me\.nekorisu"), first 16 hex: never change it, old clients look for it.
        assert_eq!(name, r"\\.\pipe\nekorisu-879b0dacaeeaced9");
        assert_eq!(name, pipe_name(Path::new(r"C:\Users\me\.nekorisu")));
        assert_ne!(name, pipe_name(Path::new(r"C:\Users\me\other-home")));
    }
}
