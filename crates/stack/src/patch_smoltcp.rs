use smoltcp::iface::{Interface, SocketHandle};
use smoltcp::phy::Device;
use smoltcp::socket::AnySocket;

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

impl<'a, DeviceT> GetSocketSafe<'a> for Interface<'a, DeviceT>
where
    DeviceT: for<'d> Device<'d>,
{
    fn get_socket_safe<T: AnySocket<'a>>(
        &mut self,
        ref_handle: SocketHandle,
    ) -> Result<&mut T, GetSocketError> {
        // We need to loop over iterator, because `get_socket` panics in case socket is not found.
        let (_, socket) = self
            .sockets_mut()
            .find(|(handle, _)| handle == &ref_handle)
            .ok_or(GetSocketError::NotFound)?;
        T::downcast(socket).ok_or(GetSocketError::WrongType)
    }
}
