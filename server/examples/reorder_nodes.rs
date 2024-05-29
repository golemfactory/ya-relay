use libc::socket;
use ya_relay_server::{Selector, SessionManager};

struct Bits<'a> {
    data : &'a [u8],
    mask : u8
}

impl<'a> Bits<'a> {

    fn for_slice(data : &'a [u8]) -> Self {
        Self {
            data,
            mask: 0x80
        }
    }

}

impl<'a> Iterator for Bits<'a> {
    type Item = bool;

    fn next(&mut self) -> Option<Self::Item> {
        let v = (self.data.first()? & self.mask) != 0;
        self.mask >>= 1;
        if self.mask == 0 {
            self.data = &self.data[1..];
            self.mask = 0x80;
        }
        return Some(v)
    }
}


#[derive(Default)]
struct PrefixTree {
    left : Option<Box<PrefixTree>>,
    right : Option<Box<PrefixTree>>
}

impl PrefixTree {

    fn insert(mut self : Box<Self>, mut bits : Bits<'_>) -> Box<Self> {
        if let Some(bit) = bits.next() {
            if bit {
                self.right = Some(self.right.take().unwrap_or_default().insert(bits));
            }
            else {
                self.left = Some(self.left.take().unwrap_or_default().insert(bits));
            }
        }
        self
    }

    fn dump(&self, prefix : &str) {
        if self.left.is_none() && self.right.is_none() {
            println!("{prefix}");
        }
        let space = if self.left.is_some() & self.right.is_some() {
            " "
        }
        else {
            ""
        };

        if let Some(l) = &self.left {
            l.dump(&format!("{prefix}{space}0"))
        }
        if let Some(r) = &self.right {
            r.dump(&format!("{prefix}{space}1"))
        }
    }

}

fn main() -> anyhow::Result<()> {
    let sm = SessionManager::load("./sessions_v2.state".as_ref())?;
    let mut pt : Box<PrefixTree> = Box::default();
    for (node_id, sessions) in sm.nodes_for(Selector::All, usize::MAX) {
        let n_sessions = sessions.len();
        let is_valid = if let Some(v) = sessions.into_iter().filter_map(|r| r.upgrade()).next() {
            v.addr_status.lock().is_valid()
        }
        else {
            false
        };
        println!("{node_id} {n_sessions} {is_valid:?}");
        pt = pt.insert(Bits::for_slice(node_id.as_ref()))
    }
    pt.dump("=");
    Ok(())
}