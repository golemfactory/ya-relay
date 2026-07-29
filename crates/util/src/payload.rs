use std::iter::FromIterator;

use bytes::{Bytes, BytesMut};
use derive_more::From;

#[derive(Debug, Clone, From)]
pub enum Payload {
    BytesMut(BytesMut),
    Vec(Vec<u8>),
    Bytes(Bytes),
}

impl Payload {
    #[inline]
    pub fn len(&self) -> usize {
        match self {
            Self::BytesMut(b) => b.len(),
            Self::Vec(b) => b.len(),
            Self::Bytes(b) => b.len(),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::BytesMut(b) => b.is_empty(),
            Self::Vec(b) => b.is_empty(),
            Self::Bytes(b) => b.is_empty(),
        }
    }

    #[inline]
    fn make_mutable(&mut self) {
        if matches!(self, Self::Bytes(_)) {
            let Self::Bytes(bytes) = std::mem::take(self) else {
                unreachable!()
            };
            *self = Self::BytesMut(
                bytes
                    .try_into_mut()
                    .unwrap_or_else(|b| BytesMut::from(b.as_ref())),
            );
        }
    }

    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.make_mutable();

        match self {
            Self::BytesMut(b) => b.reserve(additional),
            Self::Vec(b) => b.reserve(additional),
            Self::Bytes(_) => unreachable!(),
        }
    }

    pub fn extend(&mut self, bytes: BytesMut) {
        match std::mem::take(self) {
            Self::BytesMut(mut b) => {
                b.extend(bytes);
                *self = Self::BytesMut(b);
            }
            Self::Vec(mut v) => {
                v.extend(bytes);
                *self = Self::Vec(v);
            }
            Self::Bytes(b) => {
                let mut b = b
                    .try_into_mut()
                    .unwrap_or_else(|b| BytesMut::from(b.as_ref()));
                b.extend(bytes);
                *self = Self::BytesMut(b);
            }
        }
    }

    pub fn prepend(&mut self, with: &[u8]) {
        if with.is_empty() {
            return;
        }

        self.make_mutable();

        match self {
            Self::BytesMut(b) => {
                if b.is_empty() {
                    b.extend(with);
                } else {
                    let len = with.len();
                    b.extend(std::iter::repeat_n(0, len));

                    for i in (0..b.len()).rev() {
                        b[i] = if i >= len { b[i - len] } else { with[i] };
                    }
                }
            }
            Self::Vec(b) => {
                b.splice(0..0, with.iter().copied());
            }
            Self::Bytes(_) => unreachable!(),
        }
    }

    #[inline]
    pub fn into_vec(self) -> Vec<u8> {
        match self {
            Self::BytesMut(b) => Vec::from_iter(b),
            Self::Vec(b) => b,
            Self::Bytes(b) => b.to_vec(),
        }
    }

    #[inline]
    pub fn into_bytes(self) -> BytesMut {
        match self {
            Self::BytesMut(b) => b,
            Self::Vec(b) => BytesMut::from_iter(b),
            Self::Bytes(b) => b
                .try_into_mut()
                .unwrap_or_else(|b| BytesMut::from(b.as_ref())),
        }
    }
}

impl Default for Payload {
    fn default() -> Self {
        Self::Vec(Default::default())
    }
}

impl From<Box<[u8]>> for Payload {
    fn from(b: Box<[u8]>) -> Self {
        Self::Vec(b.into_vec())
    }
}

impl AsRef<[u8]> for Payload {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::BytesMut(b) => b.as_ref(),
            Self::Vec(b) => b.as_slice(),
            Self::Bytes(b) => b.as_ref(),
        }
    }
}

impl AsMut<[u8]> for Payload {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        self.make_mutable();

        match self {
            Self::BytesMut(b) => b.as_mut(),
            Self::Vec(b) => b.as_mut(),
            Self::Bytes(_) => unreachable!(),
        }
    }
}

impl FromIterator<u8> for Payload {
    fn from_iter<T: IntoIterator<Item = u8>>(iter: T) -> Self {
        Self::Vec(Vec::from_iter(iter))
    }
}

impl PartialEq<Payload> for Payload {
    fn eq(&self, other: &Payload) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl PartialEq<Payload> for BytesMut {
    fn eq(&self, other: &Payload) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl PartialEq<Payload> for Bytes {
    fn eq(&self, other: &Payload) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl PartialEq<Payload> for Vec<u8> {
    fn eq(&self, other: &Payload) -> bool {
        self.as_slice() == other.as_ref()
    }
}

impl Eq for Payload {}

impl IntoIterator for Payload {
    type Item = u8;
    type IntoIter = IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        match self {
            Self::BytesMut(b) => IntoIter::BytesMut(b.into_iter()),
            Self::Vec(v) => IntoIter::Vec(v.into_iter()),
            Self::Bytes(b) => IntoIter::Bytes(b.into_iter()),
        }
    }
}

pub enum IntoIter {
    BytesMut(bytes::buf::IntoIter<BytesMut>),
    Vec(std::vec::IntoIter<u8>),
    Bytes(bytes::buf::IntoIter<Bytes>),
}

impl Iterator for IntoIter {
    type Item = u8;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::BytesMut(ref mut i) => i.next(),
            Self::Vec(ref mut i) => i.next(),
            Self::Bytes(ref mut i) => i.next(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Payload;
    use bytes::{Bytes, BytesMut};
    use std::iter::FromIterator;

    #[test]
    fn test_prepend() {
        const EMPTY: &[u8] = &[];

        let bytes = BytesMut::from_iter(5..=8u8);
        let mut payload = Payload::BytesMut(BytesMut::new());

        payload.prepend(EMPTY);
        assert_eq!(EMPTY, payload.as_ref());
        payload.prepend(bytes.as_ref());
        assert_eq!(&[5, 6, 7, 8], payload.as_ref());
        payload.prepend(EMPTY);
        assert_eq!(&[5, 6, 7, 8], payload.as_ref());
        payload.prepend(&[4]);
        assert_eq!(&[4, 5, 6, 7, 8], payload.as_ref());
        payload.prepend(&[1, 2, 3]);
        assert_eq!(&[1, 2, 3, 4, 5, 6, 7, 8], payload.as_ref());
    }

    #[test]
    fn bytes_clone_shares_storage_until_mutated() {
        let bytes = Bytes::from_static(b"payload");
        let original = Payload::from(bytes);
        let mut cloned = original.clone();

        assert_eq!(original.as_ref().as_ptr(), cloned.as_ref().as_ptr());

        cloned.as_mut()[0] = b'P';

        assert_eq!(original.as_ref(), b"payload");
        assert_eq!(cloned.as_ref(), b"Payload");
        assert_ne!(original.as_ref().as_ptr(), cloned.as_ref().as_ptr());
    }

    #[test]
    fn uniquely_owned_bytes_become_mutable_without_copying() {
        let bytes = BytesMut::from(&b"payload"[..]).freeze();
        let ptr = bytes.as_ptr();
        let mut payload = Payload::from(bytes);

        payload.as_mut()[0] = b'P';

        assert_eq!(payload.as_ref(), b"Payload");
        assert_eq!(payload.as_ref().as_ptr(), ptr);
    }
}
