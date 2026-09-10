use actix_rt::net::UdpSocket as BaseUpdSocket;
use bytes::BytesMut;
use std::net::SocketAddr;
use std::{io, mem, ptr};

pub struct UdpSocketConfig {
    min_recv_buffer: Option<usize>,
    min_send_buffer: Option<usize>,
    bind_multi: bool,
    recv_err: bool,
}

pub struct UdpSocket {
    inner: BaseUpdSocket,
}

#[derive(Debug)]
pub enum PacketType {
    Data,
    Unreachable(UnreachableReason),
    PacketTooBig { mtu: u32 },
    Other,
}

#[derive(Debug)]
pub enum UnreachableReason {
    Network,
    Host,
    Protocol,
    Port,
    Other(u8),
}

#[cfg(target_os = "linux")]
mod icmp {
    use libc::*;

    use super::{PacketType, UnreachableReason};

    // from: /include/linux/icmp.h
    const DEST_UNREACH: u8 = 3;

    fn classify_error(error: &sock_extended_err) -> PacketType {
        if error.ee_errno == EMSGSIZE as u32 {
            return PacketType::PacketTooBig { mtu: error.ee_info };
        }
        if error.ee_origin == SO_EE_ORIGIN_ICMP && error.ee_type == DEST_UNREACH {
            return match error.ee_code {
                4 => PacketType::PacketTooBig { mtu: error.ee_info },
                code => PacketType::Unreachable(decode_reason(code)),
            };
        }
        PacketType::Other
    }

    #[cfg(test)]
    #[test]
    fn distinguishes_unreachable_from_mtu_errors() {
        let mut error: sock_extended_err = unsafe { std::mem::zeroed() };
        error.ee_origin = SO_EE_ORIGIN_ICMP;
        error.ee_type = DEST_UNREACH;
        error.ee_code = 3;
        error.ee_errno = ECONNREFUSED as u32;
        assert!(matches!(
            classify_error(&error),
            PacketType::Unreachable(UnreachableReason::Port)
        ));
        error.ee_code = 4;
        error.ee_info = 1280;
        assert!(matches!(
            classify_error(&error),
            PacketType::PacketTooBig { mtu: 1280 }
        ));
        error.ee_origin = SO_EE_ORIGIN_LOCAL;
        error.ee_type = 0;
        error.ee_code = 0;
        error.ee_errno = EMSGSIZE as u32;
        assert!(matches!(
            classify_error(&error),
            PacketType::PacketTooBig { mtu: 1280 }
        ));
        error.ee_errno = EACCES as u32;
        assert!(matches!(classify_error(&error), PacketType::Other));
    }

    fn decode_reason(code: u8) -> UnreachableReason {
        match code {
            0 => UnreachableReason::Network,
            1 => UnreachableReason::Host,
            2 => UnreachableReason::Protocol,
            3 => UnreachableReason::Port,
            code => UnreachableReason::Other(code),
        }
    }

    pub unsafe fn decode_error(
        msg: *const msghdr,
        peer: std::net::SocketAddr,
    ) -> Option<PacketType> {
        let mut hdr_it = CMSG_FIRSTHDR(msg);
        while let Some(hdr) = hdr_it.as_ref() {
            if hdr.cmsg_level == SOL_IP
                && hdr.cmsg_type == IP_RECVERR
                && hdr.cmsg_len >= CMSG_LEN(std::mem::size_of::<sock_extended_err>() as _) as _
            {
                let sock_err_ptr = CMSG_DATA(hdr) as *const libc::sock_extended_err;
                let error = sock_err_ptr.read_unaligned();
                log::debug!(
                    "[{peer}] UDP extended error: origin={}, errno={}, type={}, code={}, info={}",
                    error.ee_origin,
                    error.ee_errno,
                    error.ee_type,
                    error.ee_code,
                    error.ee_info
                );
                return Some(classify_error(&error));
            }
            hdr_it = CMSG_NXTHDR(msg, hdr_it);
        }
        None
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;

mod helpers {
    use super::*;
    #[cfg(unix)]
    pub use libc::*;
    pub use std::ffi::c_int;
    #[cfg(unix)]
    pub use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
    #[cfg(windows)]
    pub use std::os::windows::prelude::*;
    use std::sync::Once;
    #[cfg(windows)]
    use windows_sys::Win32::Networking::WinSock::{setsockopt as syssetsockopt, SOCKET_ERROR};
    #[cfg(windows)]
    pub use windows_sys::Win32::Networking::WinSock::{INVALID_SOCKET, SOCKET, SOCK_DGRAM};

    #[cfg(unix)]
    pub unsafe fn setsockopt<T>(
        fd: BorrowedFd,
        opt: c_int,
        val: c_int,
        payload: T,
    ) -> io::Result<()> {
        let payload = ptr::addr_of!(payload).cast();
        let res = libc::setsockopt(
            fd.as_raw_fd(),
            opt,
            val,
            payload,
            mem::size_of::<T>() as libc::socklen_t,
        );
        if res == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(windows)]
    pub unsafe fn setsockopt<T>(
        fd: BorrowedSocket,
        opt: c_int,
        val: c_int,
        payload: T,
    ) -> io::Result<()> {
        let payload = ptr::addr_of!(payload).cast();
        let res = syssetsockopt(
            fd.as_raw_socket() as SOCKET,
            opt,
            val,
            payload,
            mem::size_of::<T>() as i32,
        );
        if res == SOCKET_ERROR {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn init() {
        static INIT: Once = Once::new();

        INIT.call_once(|| {
            // Initialize winsock through the standard library by just creating a
            // dummy socket. Whether this is successful or not we drop the result as
            // libstd will be sure to have initialized winsock.
            let _ = std::net::UdpSocket::bind("127.0.0.1:34254");
        });
    }
}

impl UdpSocketConfig {
    #[cfg(windows)]
    pub fn bind(self, bind_addr: SocketAddr) -> io::Result<UdpSocket> {
        use helpers::*;

        #[allow(non_camel_case_types)]
        type sa_family_t = windows_sys::Win32::Networking::WinSock::ADDRESS_FAMILY;

        use windows_sys::Win32::Networking::WinSock::{
            bind, WSASocketW, AF_INET, IN_ADDR, SOCKADDR_IN as sockaddr_in, SO_RCVBUF,
            SO_REUSEADDR, SO_SNDBUF, WSA_FLAG_OVERLAPPED,
        };
        const IPPROTO_IP: c_int = windows_sys::Win32::Networking::WinSock::IPPROTO_IP as c_int;
        const SOL_SOCKET: c_int = windows_sys::Win32::Networking::WinSock::SOL_SOCKET as c_int;

        let bind_addr = match bind_addr {
            SocketAddr::V4(v) => v,
            _ => return Err(io::Error::other("wrong protocol")),
        };

        init();

        let fd = unsafe {
            let res = WSASocketW(
                AF_INET as c_int,
                SOCK_DGRAM,
                0,
                ptr::null_mut(),
                0,
                WSA_FLAG_OVERLAPPED,
            );

            if res == INVALID_SOCKET {
                return Err(std::io::Error::last_os_error());
            }

            let fd = OwnedSocket::from_raw_socket(res as RawSocket);

            if self.bind_multi {
                setsockopt(fd.as_socket(), SOL_SOCKET, SO_REUSEADDR, 1 as c_int)?;
            }

            if let Some(min_recv_buffer) = self.min_recv_buffer {
                setsockopt(
                    fd.as_socket(),
                    SOL_SOCKET,
                    SO_RCVBUF,
                    min_recv_buffer as c_int,
                )?;
            }

            if let Some(min_send_buffer) = self.min_send_buffer {
                setsockopt(
                    fd.as_socket(),
                    SOL_SOCKET,
                    SO_SNDBUF,
                    min_send_buffer as c_int,
                )?;
            }

            let addr = sockaddr_in {
                sin_family: AF_INET,
                sin_port: bind_addr.port().to_be(),
                sin_addr: mem::transmute::<[u8; 4], IN_ADDR>(bind_addr.ip().octets()),
                sin_zero: mem::zeroed(),
            };

            let res = bind(
                fd.as_raw_socket() as SOCKET,
                ptr::addr_of!(addr).cast(),
                mem::size_of_val(&addr) as i32,
            );
            if res != 0 {
                return Err(std::io::Error::last_os_error());
            }

            fd
        };

        let s = std::net::UdpSocket::from(fd);
        s.set_nonblocking(true)?;

        Ok(UdpSocket {
            inner: BaseUpdSocket::from_std(s)?,
        })
    }

    #[cfg(unix)]
    pub fn bind(self, bind_addr: SocketAddr) -> io::Result<UdpSocket> {
        use helpers::*;

        let bind_addr = match bind_addr {
            SocketAddr::V4(v) => v,
            _ => return Err(io::Error::other("wrong protocol")),
        };

        let fd = unsafe {
            let res = socket(AF_INET, SOCK_DGRAM, 0);
            if res == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let fd = OwnedFd::from_raw_fd(res);

            if self.bind_multi {
                setsockopt(fd.as_fd(), SOL_SOCKET, SO_REUSEPORT, 1 as c_int)?;
            } else if bind_addr.port() != 0 {
                setsockopt(fd.as_fd(), SOL_SOCKET, SO_REUSEADDR, 1 as c_int)?;
            }

            #[cfg(target_os = "linux")]
            if self.recv_err {
                setsockopt(fd.as_fd(), SOL_IP, IP_RECVERR, 1 as c_int)?;
            }

            if let Some(min_recv_buffer) = self.min_recv_buffer {
                setsockopt(fd.as_fd(), SOL_SOCKET, SO_RCVBUF, min_recv_buffer as c_int)?;
            }

            if let Some(min_send_buffer) = self.min_send_buffer {
                setsockopt(fd.as_fd(), SOL_SOCKET, SO_SNDBUF, min_send_buffer as c_int)?;
            }

            let addr = sockaddr_in {
                sin_family: AF_INET as sa_family_t,
                sin_port: bind_addr.port().to_be(),
                sin_addr: mem::transmute::<[u8; 4], libc::in_addr>(bind_addr.ip().octets()),
                sin_zero: mem::zeroed(),
                #[cfg(target_os = "macos")]
                sin_len: mem::size_of::<sockaddr_in>() as u8,
            };

            let res = bind(
                fd.as_raw_fd(),
                ptr::addr_of!(addr).cast(),
                mem::size_of_val(&addr) as socklen_t,
            );
            if res == -1 {
                return Err(std::io::Error::last_os_error());
            }

            fd
        };
        let s = std::net::UdpSocket::from(fd);
        s.set_nonblocking(true)?;

        Ok(UdpSocket {
            inner: BaseUpdSocket::from_std(s)?,
        })
    }

    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            min_recv_buffer: None,
            min_send_buffer: None,
            bind_multi: false,
            recv_err: false,
        }
    }

    pub fn min_recv_buffer(mut self, size: usize) -> Self {
        self.min_recv_buffer = Some(size);
        self
    }

    #[inline]
    pub fn min_send_buffer(mut self, min_send_buffer: usize) -> Self {
        self.min_send_buffer = Some(min_send_buffer);
        self
    }

    #[inline]
    pub fn multi_bind(mut self) -> Self {
        self.bind_multi = true;
        self
    }

    #[inline]
    pub fn recv_err(mut self) -> Self {
        self.recv_err = true;
        self
    }
}

impl UdpSocket {
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    pub async fn send_to(&self, buffer: &[u8], dst: SocketAddr) -> io::Result<usize> {
        let len = self.inner.send_to(buffer, dst).await?;
        Ok(len)
    }

    pub async fn recv_from(&self, buffer: &mut BytesMut) -> io::Result<SocketAddr> {
        let (_len, src) = self.inner.recv_buf_from(buffer).await?;
        Ok(src)
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn recv_any(&self, buffer: &mut BytesMut) -> io::Result<(SocketAddr, PacketType)> {
        let (_len, src) = self.inner.recv_buf_from(buffer).await?;
        Ok((src, PacketType::Data))
    }

    #[cfg(target_os = "linux")]
    pub async fn recv_any(&self, buffer: &mut BytesMut) -> io::Result<(SocketAddr, PacketType)> {
        use bytes::BufMut;
        use helpers::*;
        use std::net::{Ipv4Addr, SocketAddrV4};
        use tokio::io::Interest;

        self.inner
            .async_io(Interest::READABLE | Interest::ERROR, || unsafe {
                // Ancillary headers require native alignment.
                let mut control_buffer = [mem::MaybeUninit::<usize>::uninit(); 128];
                let mut remote: sockaddr_in = mem::zeroed();

                let mut msg: msghdr = mem::zeroed();
                let buf = buffer.spare_capacity_mut();
                let mut iov = iovec {
                    iov_base: buf.as_mut_ptr().cast(),
                    iov_len: buf.len(),
                };

                msg.msg_name = ptr::addr_of_mut!(remote).cast();
                msg.msg_namelen = mem::size_of_val(&remote) as socklen_t;

                msg.msg_iov = ptr::addr_of_mut!(iov);
                msg.msg_iovlen = 1;

                msg.msg_control = ptr::addr_of_mut!(control_buffer).cast();
                cfg_if::cfg_if! {
                    if #[cfg(target_env = "musl")] {
                        msg.msg_controllen = mem::size_of_val(&control_buffer) as socklen_t;
                    } else {
                        msg.msg_controllen = mem::size_of_val(&control_buffer) as size_t;
                    }
                }
                let mut res = recvmsg(self.inner.as_raw_fd(), ptr::addr_of_mut!(msg), MSG_DONTWAIT);
                if res == -1 {
                    let err = std::io::Error::last_os_error();
                    // recvmsg has value-result fields; restore capacity before retrying.
                    msg.msg_namelen = mem::size_of_val(&remote) as socklen_t;
                    msg.msg_controllen = mem::size_of_val(&control_buffer) as _;
                    msg.msg_flags = 0;
                    res = recvmsg(
                        self.inner.as_raw_fd(),
                        ptr::addr_of_mut!(msg),
                        MSG_ERRQUEUE | MSG_DONTWAIT,
                    );
                    if res == -1 {
                        return Err(err);
                    }
                }

                buffer.advance_mut(res as usize);
                let ip = Ipv4Addr::from(mem::transmute::<libc::in_addr, [u8; 4]>(remote.sin_addr));
                let addr = SocketAddrV4::new(ip, remote.sin_port.to_be());

                if msg.msg_flags & MSG_ERRQUEUE == MSG_ERRQUEUE {
                    metrics::increment_counter!("ya-relay.udp.transport-errors");
                    log::debug!(
                        "[{addr}] UDP error quote: len={res}, truncated={}, metadata_truncated={}",
                        msg.msg_flags & MSG_TRUNC != 0,
                        msg.msg_flags & MSG_CTRUNC != 0
                    );
                    if msg.msg_flags & MSG_CTRUNC != 0 {
                        return Ok((addr.into(), PacketType::Other));
                    }
                    if let Some(v) = icmp::decode_error(ptr::addr_of!(msg), addr.into()) {
                        Ok((addr.into(), v))
                    } else {
                        Ok((addr.into(), PacketType::Other))
                    }
                } else {
                    Ok((addr.into(), PacketType::Data))
                }
            })
            .await
    }
}
