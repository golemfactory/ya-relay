use std::iter::FromIterator;

use bytes::BytesMut;
use derive_more::From;

#[derive(Debug, Clone, From)]
pub enum Payload {
    BytesMut(BytesMut),
    Vec(Vec<u8>),
}

impl Payload {
    #[inline]
    pub fn len(&self) -> usize {
        match self {
            Self::BytesMut(b) => b.len(),
            Self::Vec(b) => b.len(),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::BytesMut(b) => b.is_empty(),
            Self::Vec(b) => b.is_empty(),
        }
    }

    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        match self {
            Self::BytesMut(b) => b.reserve(additional),
            Self::Vec(b) => b.reserve(additional),
        }
    }

    pub fn extend(&mut self, bytes: BytesMut) {
        match std::mem::take(self) {
            Self::BytesMut(mut b) => {
                b.extend(bytes);
                *self = Self::BytesMut(b);
            }
            Self::Vec(mut v) => {
                v.extend(bytes.into_iter());
                *self = Self::Vec(v);
            }
        }
    }

    pub fn prepend(&mut self, with: &[u8]) {
        if with.is_empty() {
            return;
        }

        match self {
            Self::BytesMut(b) => {
                if b.is_empty() {
                    b.extend(with);
                } else {
                    let len = with.len();
                    b.extend(std::iter::repeat(0).take(len));

                    for i in (0..b.len()).rev() {
                        b[i] = if i >= len { b[i - len] } else { with[i] };
                    }
                }
            }
            Self::Vec(b) => {
                b.splice(0..0, with.iter().copied());
            }
        }
    }

    #[inline]
    pub fn into_vec(self) -> Vec<u8> {
        match self {
            Self::BytesMut(b) => Vec::from_iter(b.into_iter()),
            Self::Vec(b) => b,
        }
    }

    #[inline]
    pub fn into_bytes(self) -> BytesMut {
        match self {
            Self::BytesMut(b) => b,
            Self::Vec(b) => BytesMut::from_iter(b),
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
        }
    }
}

impl AsMut<[u8]> for Payload {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        match self {
            Self::BytesMut(b) => b.as_mut(),
            Self::Vec(b) => b.as_mut(),
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
        }
    }
}

pub enum IntoIter {
    BytesMut(bytes::buf::IntoIter<BytesMut>),
    Vec(std::vec::IntoIter<u8>),
}

impl Iterator for IntoIter {
    type Item = u8;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::BytesMut(ref mut i) => i.next(),
            Self::Vec(ref mut i) => i.next(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Payload;
    use bytes::BytesMut;
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
}
