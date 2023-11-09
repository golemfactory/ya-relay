use actix_rt::net::UdpSocket as BaseUpdSocket;
use bytes::{BufMut, BytesMut};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::{io, mem, ptr};
use tokio::io::Interest;

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

mod icmp {
    use libc::*;

    use super::{PacketType, UnreachableReason};

    // from: /include/linux/icmp.h
    const DEST_UNREACH: u8 = 3;

    fn decode_reason(code: u8) -> UnreachableReason {
        match code {
            0 => UnreachableReason::Network,
            1 => UnreachableReason::Host,
            2 => UnreachableReason::Protocol,
            3 => UnreachableReason::Port,
            code => UnreachableReason::Other(code),
        }
    }

    pub unsafe fn decode_error(msg: *const msghdr) -> Option<PacketType> {
        let mut hdr_it = CMSG_FIRSTHDR(msg);
        while let Some(hdr) = hdr_it.as_ref() {
            if hdr.cmsg_level == SOL_IP && hdr.cmsg_type == IP_RECVERR {
                let sock_err_ptr = CMSG_DATA(hdr) as *const libc::sock_extended_err;
                if let Some(sock_err) = sock_err_ptr.as_ref() {
                    if sock_err.ee_origin == SO_EE_ORIGIN_ICMP && sock_err.ee_type == DEST_UNREACH {
                        return Some(PacketType::Unreachable(decode_reason(sock_err.ee_code)));
                    }
                }
            }
            hdr_it = CMSG_NXTHDR(msg, hdr_it);
        }
        None
    }
}

mod helpers {
    use super::*;
    pub use libc::*;
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
}

impl UdpSocketConfig {
    pub fn bind(self, bind_addr: SocketAddr) -> io::Result<UdpSocket> {
        use helpers::*;

        let bind_addr = match bind_addr {
            SocketAddr::V4(v) => v,
            _ => return Err(io::Error::new(io::ErrorKind::Other, "wrong protocol")),
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
                sin_addr: mem::transmute(bind_addr.ip().octets()),
                sin_zero: mem::zeroed(),
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

    pub async fn recv_any(&self, buffer: &mut BytesMut) -> io::Result<(SocketAddr, PacketType)> {
        use libc::*;

        self.inner
            .async_io(Interest::READABLE | Interest::ERROR, || unsafe {
                let mut control_buffer = [mem::MaybeUninit::<u8>::uninit(); 1024];
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

                msg.msg_flags = MSG_ERRQUEUE;
                msg.msg_control = ptr::addr_of_mut!(control_buffer).cast();
                msg.msg_controllen = mem::size_of_val(&control_buffer);

                let mut res = recvmsg(self.inner.as_raw_fd(), ptr::addr_of_mut!(msg), MSG_DONTWAIT);
                if res == -1 {
                    let err = std::io::Error::last_os_error();
                    /*if err.kind() == io::ErrorKind::WouldBlock {
                        return Err(err);
                    }*/
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
                let ip = Ipv4Addr::from(mem::transmute::<_, [u8; 4]>(remote.sin_addr));
                let addr = SocketAddrV4::new(ip, remote.sin_port.to_be());

                if msg.msg_flags & MSG_ERRQUEUE == MSG_ERRQUEUE {
                    if let Some(v) = icmp::decode_error(ptr::addr_of!(msg)) {
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
