/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::cmp::min;
use std::collections::VecDeque;
use std::convert::TryInto;
use std::ffi::{CStr, CString, OsStr};
use std::fmt;
use std::io::{self, copy, Cursor, LineWriter, Read, Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::mem::{self, MaybeUninit};
#[cfg(unix)]
use std::os::unix::ffi;
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::ffi;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::str::{self, FromStr};
use std::sync::mpsc::{channel, Receiver};

use bstr::ByteSlice;

#[macro_export]
macro_rules! derive_debug_display {
    ($typ:ty) => {
        impl ::std::fmt::Debug for $typ
        where
            $typ: ::std::fmt::Display,
        {
            fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
                f.debug_tuple(stringify!($typ))
                    .field(&format!("{}", self))
                    .finish()
            }
        }
    };
}

pub struct PrefixWriter<W: Write> {
    prefix: ImmutBString,
    line_writer: LineWriter<W>,
}

impl<W: Write> PrefixWriter<W> {
    pub fn new(prefix: &[u8], w: W) -> Self {
        PrefixWriter {
            prefix: prefix.to_boxed(),
            line_writer: LineWriter::new(w),
        }
    }
}

impl<W: Write> Write for PrefixWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut len = 0;
        for line in buf.lines_with_terminator() {
            self.line_writer.write_all(&self.prefix)?;
            len += self.line_writer.write(line)?;
        }
        Ok(len)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.line_writer.flush()
    }
}

pub struct BufferedReader<'a> {
    thread: Option<std::thread::JoinHandle<io::Result<()>>>,
    receiver: Option<Receiver<ImmutBString>>,
    buf: VecDeque<u8>,
    marker: PhantomData<&'a mut ()>,
}

impl<'a> BufferedReader<'a> {
    fn new_<R: Read + Send + 'static, const BUFSIZE: usize>(r: &'a mut R) -> Self {
        let (sender, receiver) = channel::<ImmutBString>();
        let r = unsafe { std::mem::transmute::<_, &'static mut R>(r) };
        let thread = std::thread::spawn(move || {
            loop {
                let mut buf = vec![0; BUFSIZE];
                let len = r.read_at_most(&mut buf[..])?;
                if len > 0 {
                    buf.truncate(len);
                    sender.send(buf.into()).unwrap();
                } else {
                    break;
                }
            }
            Ok(())
        });
        BufferedReader {
            thread: Some(thread),
            receiver: Some(receiver),
            buf: VecDeque::new(),
            marker: PhantomData,
        }
    }

    pub fn new<R: Read + Send + 'static>(r: &'a mut R) -> Self {
        Self::new_::<_, { 1024 * 1024 }>(r)
    }
}

impl<'a> Read for BufferedReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut size = min(self.buf.len(), buf.len());
        let (start, rest) = buf.split_at_mut(size);
        for (b, x) in Iterator::zip(start.iter_mut(), self.buf.drain(0..size)) {
            *b = x;
        }
        if !rest.is_empty() {
            assert!(self.buf.is_empty());
            if let Some(buf) = self.receiver.as_ref().and_then(|r| r.recv().ok()) {
                self.buf = buf.to_vec().into();
                size += self.read(rest)?;
            }
        }
        Ok(size)
    }
}

#[test]
fn test_buffered_reader() {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use itertools::Itertools;

    struct ArcRead<R: Read>(Arc<Mutex<R>>);

    impl<R: Read> Read for ArcRead<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.lock().unwrap().read(buf)
        }
    }

    let data = Arc::new(Mutex::new(Cursor::new((1..=200).collect_vec())));

    let mut r = ArcRead(Arc::clone(&data));
    let mut reader = BufferedReader::new_::<_, 3>(&mut r);

    let mut buf = vec![0; 255];
    let mut offset = 0;
    for i in 1..20 {
        assert_eq!(reader.read(&mut buf[offset..offset + i]).unwrap(), i);
        offset += i;
        if i == 1 {
            std::thread::sleep(Duration::from_millis(10));
            // Everything should have been read already.
            assert_eq!(data.lock().unwrap().position(), 200);
        }
    }
    // We've read 190 characters so far.
    assert_eq!(offset, 190);
    // There are only 10 left, although our buffer can take more.
    assert_eq!(reader.read(&mut buf[190..]).unwrap(), 10);
    drop(reader);
    drop(r);
    assert_eq!(
        &buf[..200],
        Arc::try_unwrap(data)
            .unwrap()
            .into_inner()
            .unwrap()
            .into_inner()
    );
}

pub trait ReadExt: Read {
    fn read_at_most(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut input = self.take(buf.len().try_into().unwrap());
        let mut buf = Cursor::new(buf);
        copy(&mut input, &mut buf).map(|l| l as usize)
    }
}

impl<T: Read> ReadExt for T {}

pub trait SeekExt: Seek {
    fn stream_len_(&mut self) -> io::Result<u64> {
        let old_pos = self.seek(SeekFrom::Current(0))?;
        let len = self.seek(SeekFrom::End(0))?;
        self.seek(SeekFrom::Start(old_pos))?;
        Ok(len)
    }
}

impl<T: Seek> SeekExt for T {}

pub trait SliceExt<C> {
    fn splitn_exact<const N: usize>(&self, c: C) -> Option<[&Self; N]>;
    fn rsplitn_exact<const N: usize>(&self, c: C) -> Option<[&Self; N]>;
}

// Ideally, we'd just use array_init::from_iter, but it's not usable
// both in versions of rustc with stable min_const_generics and versions
// with unstable min_const_generics.
fn array_init_from_iter_<
    'a,
    T: ?Sized,
    I: Iterator<Item = &'a T>,
    const N: usize,
    const REVERSED: bool,
>(
    mut iter: I,
) -> Option<[&'a T; N]> {
    let mut result: MaybeUninit<[&'a T; N]> = MaybeUninit::uninit();
    let ptr = result.as_mut_ptr() as *mut &'a T;
    let mut forward = 0..N;
    let mut reversed = (0..N).rev();
    let indices: &mut dyn Iterator<Item = _> = if REVERSED {
        &mut reversed
    } else {
        &mut forward
    };
    unsafe {
        for i in indices {
            #[allow(clippy::ptr_offset_with_cast)]
            ptr.offset(i as isize).write(iter.next()?);
        }
        Some(result.assume_init())
    }
}

fn array_init_from_iter<'a, T: ?Sized, const N: usize>(
    iter: impl Iterator<Item = &'a T>,
) -> Option<[&'a T; N]> {
    array_init_from_iter_::<'a, T, _, N, false>(iter)
}

fn array_init_from_rev_iter<'a, T: ?Sized, const N: usize>(
    iter: impl Iterator<Item = &'a T>,
) -> Option<[&'a T; N]> {
    array_init_from_iter_::<'a, T, _, N, true>(iter)
}

impl<T: PartialEq> SliceExt<T> for [T] {
    fn splitn_exact<const N: usize>(&self, x: T) -> Option<[&Self; N]> {
        array_init_from_iter(self.splitn(N, |i| *i == x))
    }

    fn rsplitn_exact<const N: usize>(&self, x: T) -> Option<[&Self; N]> {
        array_init_from_rev_iter(self.rsplitn(N, |i| *i == x))
    }
}

impl SliceExt<char> for str {
    fn splitn_exact<const N: usize>(&self, c: char) -> Option<[&Self; N]> {
        array_init_from_iter(self.splitn(N, c))
    }

    fn rsplitn_exact<const N: usize>(&self, c: char) -> Option<[&Self; N]> {
        array_init_from_rev_iter(self.rsplitn(N, c))
    }
}

impl<F: FnMut(&u8) -> bool> SliceExt<F> for [u8] {
    fn splitn_exact<const N: usize>(&self, f: F) -> Option<[&Self; N]> {
        array_init_from_iter(self.splitn(N, f))
    }

    fn rsplitn_exact<const N: usize>(&self, f: F) -> Option<[&Self; N]> {
        array_init_from_rev_iter(self.rsplitn(N, f))
    }
}

impl SliceExt<&[u8]> for [u8] {
    fn splitn_exact<const N: usize>(&self, b: &[u8]) -> Option<[&Self; N]> {
        // Safety: This works around ByteSlice::splitn_str being too restrictive.
        // https://github.com/BurntSushi/bstr/issues/45
        let iter = self.splitn_str(N, unsafe { mem::transmute::<_, &[u8]>(b) });
        array_init_from_iter(iter)
    }

    fn rsplitn_exact<const N: usize>(&self, b: &[u8]) -> Option<[&Self; N]> {
        let iter = self.rsplitn_str(N, unsafe { mem::transmute::<_, &[u8]>(b) });
        array_init_from_rev_iter(iter)
    }
}

pub trait OsStrExt: ffi::OsStrExt {
    fn as_bytes(&self) -> &[u8];

    fn from_bytes(b: &[u8]) -> &Self;

    fn to_cstring(&self) -> CString;

    fn strip_prefix(&self, prefix: impl AsRef<OsStr>) -> Option<&Self>;
}

impl OsStrExt for OsStr {
    #[cfg(windows)]
    fn as_bytes(&self) -> &[u8] {
        // git assumes everything is UTF-8-valid on Windows
        self.to_str().unwrap().as_bytes()
    }
    #[cfg(windows)]
    fn from_bytes(b: &[u8]) -> &Self {
        b.to_str().unwrap().as_ref()
    }

    #[cfg(unix)]
    fn as_bytes(&self) -> &[u8] {
        ffi::OsStrExt::as_bytes(self)
    }
    #[cfg(unix)]
    fn from_bytes(b: &[u8]) -> &Self {
        ffi::OsStrExt::from_bytes(b)
    }

    fn to_cstring(&self) -> CString {
        CString::new(self.as_bytes()).unwrap()
    }

    #[cfg(unix)]
    fn strip_prefix(&self, prefix: impl AsRef<OsStr>) -> Option<&Self> {
        self.as_bytes()
            .strip_prefix(prefix.as_ref().as_bytes())
            .map(|b| ffi::OsStrExt::from_bytes(b))
    }
    #[cfg(windows)]
    fn strip_prefix(&self, prefix: impl AsRef<OsStr>) -> Option<&Self> {
        self.to_str()
            .unwrap()
            .strip_prefix(prefix.as_ref().to_str().unwrap())
            .map(|b| OsStr::new(b))
    }
}

pub trait CStrExt {
    fn to_osstr(&self) -> &OsStr;
}

impl CStrExt for CStr {
    #[cfg(windows)]
    fn to_osstr(&self) -> &OsStr {
        OsStr::new(self.to_str().unwrap())
    }

    #[cfg(unix)]
    fn to_osstr(&self) -> &OsStr {
        ffi::OsStrExt::from_bytes(self.to_bytes())
    }
}

pub trait FromBytes: Sized {
    type Err;
    fn from_bytes(b: &[u8]) -> Result<Self, Self::Err>;
}

impl<T: FromStr> FromBytes for T {
    type Err = <T as FromStr>::Err;
    fn from_bytes(b: &[u8]) -> Result<Self, Self::Err> {
        //TODO: convert the error from str::from_utf8 to Self::Err
        Self::from_str(str::from_utf8(b).unwrap())
    }
}

pub fn bstr_fmt<S: AsRef<[u8]>>(s: &S, f: &mut fmt::Formatter) -> fmt::Result {
    fmt::Debug::fmt(s.as_ref().as_bstr(), f)
}

pub trait OptionExt<T> {
    fn as_ptr(&self) -> *const T;
}

pub trait OptionMutExt<T>: OptionExt<T> {
    fn as_mut_ptr(&mut self) -> *mut T;
}

impl<T> OptionExt<T> for Option<&T> {
    fn as_ptr(&self) -> *const T {
        match self {
            Some(x) => *x as *const T,
            None => std::ptr::null(),
        }
    }
}

impl<T> OptionExt<T> for Option<&mut T> {
    fn as_ptr(&self) -> *const T {
        match self {
            Some(x) => *x as *const T,
            None => std::ptr::null(),
        }
    }
}

impl<T> OptionMutExt<T> for Option<&mut T> {
    fn as_mut_ptr(&mut self) -> *mut T {
        match self {
            Some(ref mut x) => *x as *mut T,
            None => std::ptr::null_mut(),
        }
    }
}

#[test]
fn test_optionext() {
    use std::sync::atomic::{AtomicBool, Ordering};

    static DROPPED: AtomicBool = AtomicBool::new(false);

    struct Foo;
    impl Drop for Foo {
        fn drop(&mut self) {
            assert!(!DROPPED.load(Ordering::SeqCst));
            DROPPED.store(true, Ordering::SeqCst);
        }
    }

    fn callback(ptr: *const Foo) {
        assert_ne!(ptr, std::ptr::null());
        assert!(!DROPPED.load(Ordering::SeqCst));
    }

    fn callback_mut(ptr: *mut Foo) {
        assert_ne!(ptr, std::ptr::null_mut());
        assert!(!DROPPED.load(Ordering::SeqCst));
    }

    // For good measure, ensure that lifetimes workout fine.
    callback(Some(Foo).as_ref().as_ptr());
    assert!(DROPPED.load(Ordering::SeqCst));
    DROPPED.store(false, Ordering::SeqCst);
    callback(Some(Foo).as_mut().as_ptr());
    assert!(DROPPED.load(Ordering::SeqCst));
    DROPPED.store(false, Ordering::SeqCst);
    callback_mut(Some(Foo).as_mut().as_mut_ptr());
    assert!(DROPPED.load(Ordering::SeqCst));
    assert_eq!(std::ptr::null(), (None as Option<&usize>).as_ptr());
}

pub trait IteratorExt: Iterator {
    fn try_find_<E, F: FnMut(&Self::Item) -> Result<bool, E>>(
        &mut self,
        mut f: F,
    ) -> Result<Option<Self::Item>, E>
    where
        Self: Sized,
    {
        let result = self.try_for_each(|i| match f(&i) {
            Ok(false) => Ok(()),
            Ok(true) => Err(Ok(i)),
            Err(e) => Err(Err(e)),
        });
        match result {
            Ok(()) => Ok(None),
            Err(Ok(item)) => Ok(Some(item)),
            Err(Err(e)) => Err(e),
        }
    }
}

impl<I: Iterator> IteratorExt for I {}

pub trait Duplicate {
    fn dup_inheritable(&self) -> DuplicateFd;
}

#[cfg(unix)]
pub struct DuplicateFd(RawFd);

#[cfg(windows)]
pub struct DuplicateFd(RawHandle);

impl Drop for DuplicateFd {
    fn drop(&mut self) {
        unsafe {
            #[cfg(unix)]
            libc::close(self.0);
            #[cfg(windows)]
            winapi::um::handleapi::CloseHandle(self.0);
        }
    }
}

impl fmt::Display for DuplicateFd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0 as usize)
    }
}

#[cfg(unix)]
impl AsRawFd for DuplicateFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

#[cfg(unix)]
impl<T: AsRawFd> Duplicate for T {
    fn dup_inheritable(&self) -> DuplicateFd {
        let fd = unsafe { libc::dup(self.as_raw_fd()) };
        if fd < 0 {
            panic!("Failed to duplicate file descriptor");
        }
        DuplicateFd(fd)
    }
}

#[cfg(windows)]
impl AsRawHandle for DuplicateFd {
    fn as_raw_handle(&self) -> RawHandle {
        self.0
    }
}

#[cfg(windows)]
impl<T: AsRawHandle> Duplicate for T {
    fn dup_inheritable(&self) -> DuplicateFd {
        let mut handle: RawHandle = std::ptr::null_mut();
        unsafe {
            let curproc = winapi::um::processthreadsapi::GetCurrentProcess();
            if winapi::um::handleapi::DuplicateHandle(
                curproc,
                self.as_raw_handle(),
                curproc,
                &mut handle,
                /* dwDesiredAccess */ 0,
                /* bInheritHandle */ 1,
                winapi::um::winnt::DUPLICATE_SAME_ACCESS,
            ) == 0
            {
                panic!("Failed to duplicate handle");
            }
        }
        DuplicateFd(handle)
    }
}

pub type ImmutBString = Box<[u8]>;
pub type ImmutString = Box<str>;

pub trait ToBoxed {
    fn to_boxed(&self) -> Box<Self>;
}

impl<T: Clone> ToBoxed for [T] {
    fn to_boxed(&self) -> Box<Self> {
        self.to_vec().into()
    }
}

impl ToBoxed for str {
    fn to_boxed(&self) -> Box<Self> {
        self.to_string().into()
    }
}
