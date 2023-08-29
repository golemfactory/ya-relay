use smoltcp::iface::SocketHandle;
use smoltcp::socket::AnySocket;

use crate::interface::CaptureInterface;

#[derive(thiserror::Error, Clone, Debug)]
pub enum GetSocketError {
    #[error("Socket not found")]
    NotFound,
    #[error("Wrong socket type")]
    WrongType,
}

pub trait GetSocketSafe<'a> {
    fn get_socket_safe<T: AnySocket<'a>>(
        &mut self,
        handle: SocketHandle,
    ) -> Result<&mut T, GetSocketError>;
}

impl<'a> GetSocketSafe<'a> for CaptureInterface<'a> {
    fn get_socket_safe<T: AnySocket<'a>>(
        &mut self,
        ref_handle: SocketHandle,
    ) -> Result<&mut T, GetSocketError> {
        // We need to loop over iterator, because `get_socket` panics in case socket is not found.
        let (_, socket) = self
            .sockets_mut()
            .find(|(handle, _)| handle == &ref_handle)
            .ok_or(GetSocketError::NotFound)?;
        T::downcast_mut(socket).ok_or(GetSocketError::WrongType)
    }
}
