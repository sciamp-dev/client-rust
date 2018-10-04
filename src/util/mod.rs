use std::collections::hash_map::Entry;
use std::collections::vec_deque::{Iter, VecDeque};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::{io, u64};
use std::{slice, thread};

#[macro_use]
pub mod macros;
pub mod mpsc;
pub mod security;
pub mod time;
pub mod timer;

pub const NO_LIMIT: u64 = u64::MAX;

/// Take slices in the range.
///
/// ### Panic
///
/// if [low, high) is out of bound.
pub fn slices_in_range<T>(entry: &VecDeque<T>, low: usize, high: usize) -> (&[T], &[T]) {
    let (first, second) = entry.as_slices();
    if low >= first.len() {
        (&second[low - first.len()..high - first.len()], &[])
    } else if high <= first.len() {
        (&first[low..high], &[])
    } else {
        (&first[low..], &second[..high - first.len()])
    }
}

/// A handy shortcut to replace `RwLock` write/read().unwrap() pattern to
/// shortcut wl and rl.
pub trait HandyRwLock<T> {
    fn wl(&self) -> RwLockWriteGuard<T>;
    fn rl(&self) -> RwLockReadGuard<T>;
}

impl<T> HandyRwLock<T> for RwLock<T> {
    fn wl(&self) -> RwLockWriteGuard<T> {
        self.write().unwrap()
    }

    fn rl(&self) -> RwLockReadGuard<T> {
        self.read().unwrap()
    }
}

// A helper function to parse SocketAddr for mio.
// In mio example, it uses "127.0.0.1:80".parse() to get the SocketAddr,
// but it is just ok for "ip:port", not "host:port".
pub fn to_socket_addr<A: ToSocketAddrs>(addr: A) -> io::Result<SocketAddr> {
    let addrs = addr.to_socket_addrs()?;
    Ok(addrs.collect::<Vec<SocketAddr>>()[0])
}

/// A function to escape a byte array to a readable ascii string.
/// escape rules follow golang/protobuf.
/// <https://github.com/golang/protobuf/blob/master/proto/text.go#L578>
///
/// # Examples
///
/// ```
/// use tikv_client::util::escape;
///
/// assert_eq!(r"ab", escape(b"ab"));
/// assert_eq!(r"a\\023", escape(b"a\\023"));
/// assert_eq!(r"a\000", escape(b"a\0"));
/// assert_eq!("a\\r\\n\\t '\\\"\\\\", escape(b"a\r\n\t '\"\\"));
/// assert_eq!(r"\342\235\244\360\237\220\267", escape("❤🐷".as_bytes()));
/// ```
pub fn escape(data: &[u8]) -> String {
    let mut escaped = Vec::with_capacity(data.len() * 4);
    for &c in data {
        match c {
            b'\n' => escaped.extend_from_slice(br"\n"),
            b'\r' => escaped.extend_from_slice(br"\r"),
            b'\t' => escaped.extend_from_slice(br"\t"),
            b'"' => escaped.extend_from_slice(b"\\\""),
            b'\\' => escaped.extend_from_slice(br"\\"),
            _ => {
                if c >= 0x20 && c < 0x7f {
                    // c is printable
                    escaped.push(c);
                } else {
                    escaped.push(b'\\');
                    escaped.push(b'0' + (c >> 6));
                    escaped.push(b'0' + ((c >> 3) & 7));
                    escaped.push(b'0' + (c & 7));
                }
            }
        }
    }
    escaped.shrink_to_fit();
    unsafe { String::from_utf8_unchecked(escaped) }
}

/// A function to unescape an escaped string to a byte array.
///
/// # Panic
///
/// If s is not a properly encoded string.
pub fn unescape(s: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b != b'\\' {
            buf.push(b);
            continue;
        }
        match bytes.next().unwrap() {
            b'"' => buf.push(b'"'),
            b'\'' => buf.push(b'\''),
            b'\\' => buf.push(b'\\'),
            b'n' => buf.push(b'\n'),
            b't' => buf.push(b'\t'),
            b'r' => buf.push(b'\r'),
            b'x' => {
                macro_rules! next_hex {
                    () => {
                        bytes.next().map(char::from).unwrap().to_digit(16).unwrap()
                    };
                }
                // Can coerce as u8 since the range of possible values is constrained to
                // between 00 and FF.
                buf.push(((next_hex!() << 4) + next_hex!()) as u8);
            }
            b => {
                let b1 = b - b'0';
                let b2 = bytes.next().unwrap() - b'0';
                let b3 = bytes.next().unwrap() - b'0';
                buf.push((b1 << 6) + (b2 << 3) + b3);
            }
        }
    }
    buf.shrink_to_fit();
    buf
}

/// Convert a borrow to a slice.
pub fn as_slice<T>(t: &T) -> &[T] {
    unsafe {
        let ptr = t as *const T;
        slice::from_raw_parts(ptr, 1)
    }
}

/// `TryInsertWith` is a helper trait for `Entry` to accept a failable closure.
pub trait TryInsertWith<'a, V, E> {
    fn or_try_insert_with<F: FnOnce() -> Result<V, E>>(self, default: F) -> Result<&'a mut V, E>;
}

impl<'a, T: 'a, V: 'a, E> TryInsertWith<'a, V, E> for Entry<'a, T, V> {
    fn or_try_insert_with<F: FnOnce() -> Result<V, E>>(self, default: F) -> Result<&'a mut V, E> {
        match self {
            Entry::Occupied(e) => Ok(e.into_mut()),
            Entry::Vacant(e) => {
                let v = default()?;
                Ok(e.insert(v))
            }
        }
    }
}

pub fn get_tag_from_thread_name() -> Option<String> {
    thread::current()
        .name()
        .and_then(|name| name.split("::").skip(1).last())
        .map(From::from)
}

/// `DeferContext` will invoke the wrapped closure when dropped.
pub struct DeferContext<T: FnOnce()> {
    t: Option<T>,
}

impl<T: FnOnce()> DeferContext<T> {
    pub fn new(t: T) -> DeferContext<T> {
        DeferContext { t: Some(t) }
    }
}

impl<T: FnOnce()> Drop for DeferContext<T> {
    fn drop(&mut self) {
        self.t.take().unwrap()()
    }
}

/// Represents a value of one of two possible types (a more generic Result.)
#[derive(Debug, Clone)]
pub enum Either<L, R> {
    Left(L),
    Right(R),
}

impl<L, R> Either<L, R> {
    #[inline]
    pub fn as_ref(&self) -> Either<&L, &R> {
        match *self {
            Either::Left(ref l) => Either::Left(l),
            Either::Right(ref r) => Either::Right(r),
        }
    }

    #[inline]
    pub fn as_mut(&mut self) -> Either<&mut L, &mut R> {
        match *self {
            Either::Left(ref mut l) => Either::Left(l),
            Either::Right(ref mut r) => Either::Right(r),
        }
    }

    #[inline]
    pub fn left(self) -> Option<L> {
        match self {
            Either::Left(l) => Some(l),
            _ => None,
        }
    }

    #[inline]
    pub fn right(self) -> Option<R> {
        match self {
            Either::Right(r) => Some(r),
            _ => None,
        }
    }
}

/// A simple ring queue with fixed capacity.
pub struct RingQueue<T> {
    buf: VecDeque<T>,
    cap: usize,
}

impl<T> RingQueue<T> {
    #[inline]
    fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn with_capacity(cap: usize) -> RingQueue<T> {
        RingQueue {
            buf: VecDeque::with_capacity(cap),
            cap,
        }
    }

    pub fn push(&mut self, t: T) {
        if self.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(t);
    }

    pub fn iter(&self) -> Iter<T> {
        self.buf.iter()
    }

    pub fn swap_remove_front<F>(&mut self, f: F) -> Option<T>
    where
        F: FnMut(&T) -> bool,
    {
        if let Some(pos) = self.buf.iter().position(f) {
            self.buf.swap_remove_front(pos)
        } else {
            None
        }
    }
}

#[inline]
pub fn is_even(n: usize) -> bool {
    n & 1 == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::Message;
    use std::net::{AddrParseError, SocketAddr};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::*;

    #[test]
    fn test_to_socket_addr() {
        let tbls = vec![
            ("", false),
            ("127.0.0.1", false),
            ("localhost", false),
            ("127.0.0.1:80", true),
            ("localhost:80", true),
        ];

        for (addr, ok) in tbls {
            assert_eq!(to_socket_addr(addr).is_ok(), ok);
        }

        let tbls = vec![("localhost:80", false), ("127.0.0.1:80", true)];

        for (addr, ok) in tbls {
            let ret: Result<SocketAddr, AddrParseError> = addr.parse();
            assert_eq!(ret.is_ok(), ok);
        }
    }

    #[test]
    fn test_fixed_ring_queue() {
        let mut queue = RingQueue::with_capacity(10);
        for num in 0..20 {
            queue.push(num);
            assert_eq!(queue.len(), cmp::min(num + 1, 10));
        }
        assert_eq!(None, queue.swap_remove_front(|i| *i == 20));
        for i in 0..6 {
            assert_eq!(Some(12 + i), queue.swap_remove_front(|e| *e == 12 + i));
            assert_eq!(queue.len(), 9 - i);
        }

        let left: Vec<_> = queue.iter().cloned().collect();
        assert_eq!(vec![10, 11, 18, 19], left);
        for _ in 0..4 {
            queue.swap_remove_front(|_| true).unwrap();
        }
        assert_eq!(None, queue.swap_remove_front(|_| true));
    }

    #[test]
    fn test_defer() {
        let should_panic = Rc::new(AtomicBool::new(true));
        let sp = Rc::clone(&should_panic);
        defer!(assert!(!sp.load(Ordering::SeqCst)));
        should_panic.store(false, Ordering::SeqCst);
    }

    #[test]
    fn test_rwlock_deadlock() {
        // If the test runs over 60s, then there is a deadlock.
        let mu = RwLock::new(Some(1));
        {
            let _clone = foo(&mu.rl());
            let mut data = mu.wl();
            assert!(data.is_some());
            *data = None;
        }

        {
            match foo(&mu.rl()) {
                Some(_) | None => {
                    let res = mu.try_write();
                    assert!(res.is_err());
                }
            }
        }

        #[cfg_attr(feature = "cargo-clippy", allow(clone_on_copy))]
        fn foo(a: &Option<usize>) -> Option<usize> {
            a.clone()
        }
    }

    #[test]
    fn test_slices_vec_deque() {
        for first in 0..10 {
            let mut v = VecDeque::with_capacity(10);
            for i in 0..first {
                v.push_back(i);
            }
            for i in first..10 {
                v.push_back(i - first);
            }
            v.drain(..first);
            for i in 0..first {
                v.push_back(10 + i - first);
            }
            for len in 0..10 {
                for low in 0..len + 1 {
                    for high in low..len + 1 {
                        let (p1, p2) = super::slices_in_range(&v, low, high);
                        let mut res = vec![];
                        res.extend_from_slice(p1);
                        res.extend_from_slice(p2);
                        let exp: Vec<_> = (low..high).collect();
                        assert_eq!(
                            res, exp,
                            "[{}, {}) in {:?} with first: {}",
                            low, high, v, first
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_unescape() {
        // No escapes
        assert_eq!(unescape(r"ab"), b"ab");
        // Escaped backslash
        assert_eq!(unescape(r"a\\023"), b"a\\023");
        // Escaped three digit octal
        assert_eq!(unescape(r"a\000"), b"a\0");
        assert_eq!(
            unescape(r"\342\235\244\360\237\220\267"),
            "❤🐷".as_bytes()
        );
        // Whitespace
        assert_eq!(unescape("a\\r\\n\\t '\\\"\\\\"), b"a\r\n\t '\"\\");
        // Hex Octals
        assert_eq!(unescape(r"abc\x64\x65\x66ghi"), b"abcdefghi");
        assert_eq!(unescape(r"JKL\x4d\x4E\x4fPQR"), b"JKLMNOPQR");
    }
}