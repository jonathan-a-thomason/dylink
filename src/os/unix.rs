#![allow(clippy::let_unit_value)]

use crate::sealed::Sealed;
use crate::{img, weak, Symbol};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::{ffi, io, mem, path::PathBuf, ptr};

#[cfg(target_os = "macos")]
use std::sync::{
	atomic::{AtomicU32, Ordering},
	Once,
};

#[cfg(not(any(target_os = "linux", target_os = "macos", target_env = "gnu")))]
use std::sync::{LockResult, Mutex, MutexGuard};

mod c;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_env = "gnu")))]
#[inline]
fn dylib_guard<'a>() -> LockResult<MutexGuard<'a, ()>> {
	static LOCK: Mutex<()> = Mutex::new(());
	LOCK.lock()
}

#[cfg(any(target_os = "linux", target_os = "macos", target_env = "gnu"))]
#[inline(always)]
fn dylib_guard() {}

unsafe fn c_dlerror() -> Option<ffi::CString> {
	let raw = c::dlerror();
	if raw.is_null() {
		None
	} else {
		Some(ffi::CStr::from_ptr(raw).to_owned())
	}
}

// dlopen may return a different handle if the path is not null.
// This function solves the problem of `Library::to_library` not working with `Library::this`
fn dlopen_fname(fname: &ffi::CStr) -> *const ffi::c_char {
	if fname.to_str().unwrap() == std::env::current_exe().unwrap().to_str().unwrap() {
		std::ptr::null()
	} else {
		fname.as_ptr()
	}
}

#[derive(Debug)]
#[repr(transparent)]
pub(crate) struct InnerLibrary(pub ptr::NonNull<ffi::c_void>);

impl InnerLibrary {
	pub unsafe fn open(path: &ffi::OsStr) -> io::Result<Self> {
		let _lock = dylib_guard();
		let c_str = ffi::CString::new(path.as_bytes())?;
		let handle: *mut ffi::c_void = c::dlopen(c_str.as_ptr(), c::RTLD_NOW | c::RTLD_LOCAL);
		if let Some(ret) = ptr::NonNull::new(handle) {
			Ok(Self(ret))
		} else {
			let err = c_dlerror().unwrap();
			Err(io::Error::new(io::ErrorKind::Other, err.to_string_lossy()))
		}
	}
	pub unsafe fn this() -> io::Result<Self> {
		let _lock = dylib_guard();
		let handle: *mut ffi::c_void = c::dlopen(ptr::null(), c::RTLD_NOW | c::RTLD_LOCAL);
		if let Some(ret) = ptr::NonNull::new(handle) {
			Ok(Self(ret))
		} else {
			let err = c_dlerror().unwrap();
			Err(io::Error::new(io::ErrorKind::Other, err.to_string_lossy()))
		}
	}

	#[inline]
	pub unsafe fn raw_symbol(&self, name: &ffi::CStr) -> *const Symbol {
		c::dlsym(self.0.as_ptr(), name.as_ptr()).cast()
	}

	pub unsafe fn symbol(&self, name: &str) -> io::Result<*const Symbol> {
		let _lock = dylib_guard();
		let c_str = ffi::CString::new(name).unwrap();

		let _ = c_dlerror(); // clear existing errors
		let handle = self.raw_symbol(&c_str).cast_mut();

		if let Some(err) = c_dlerror() {
			Err(io::Error::new(io::ErrorKind::Other, err.to_string_lossy()))
		} else {
			Ok(handle)
		}
	}
	pub(crate) unsafe fn try_clone(&self) -> io::Result<Self> {
		let this = Self::this()?;
		if this.0 == self.0 {
			Ok(this)
		} else {
			std::mem::drop(this);
			let Some(hdr) = self.to_ptr().as_ref() else {
				return Err(io::Error::new(io::ErrorKind::NotFound, "header not found"));
			};
			let path = hdr.path()?;
			Self::open(path.as_os_str())
		}
	}

	// This is to handle any platforms that I cannot deal with.
	#[cfg(not(any(target_env = "gnu", target_os = "macos")))]
	pub(crate) unsafe fn to_ptr(&self) -> *const img::Image {
		std::ptr::null()
	}

	// returns null if handle is invalid
	#[cfg(target_env = "gnu")]
	pub(crate) unsafe fn to_ptr(&self) -> *const img::Image {
		let mut map_ptr = ptr::null_mut::<c::link_map>();
		if c::dlinfo(
			self.0.as_ptr(),
			c::RTLD_DI_LINKMAP,
			&mut map_ptr as *mut _ as *mut _,
		) == 0
		{
			(*map_ptr).l_addr as *const img::Image
		} else {
			ptr::null()
		}
	}

	// returns null if handle is invalid
	#[cfg(target_os = "macos")]
	pub(crate) unsafe fn to_ptr(&self) -> *const img::Image {
		let handle = self.0;
		let mut result = ptr::null();
		let _ = get_image_count().fetch_update(Ordering::SeqCst, Ordering::SeqCst, |image_index| {
			for image_index in (0..image_index).rev() {
				let image_name = c::_dyld_get_image_name(image_index);
				let filename = dlopen_fname(ffi::CStr::from_ptr(image_name));
				let active_handle =
					c::dlopen(filename, c::RTLD_NOW | c::RTLD_LOCAL | c::RTLD_NOLOAD);
				if !active_handle.is_null() {
					let _ = c::dlclose(active_handle);
				}
				if (handle.as_ptr() as isize & (-4)) == (active_handle as isize & (-4)) {
					result = c::_dyld_get_image_header(image_index) as *const img::Image;
					break;
				}
			}
			Some(image_index)
		});
		result
	}
	pub(crate) unsafe fn from_ptr(addr: *const img::Image) -> Option<Self> {
		let mut info = mem::MaybeUninit::zeroed();
		if c::dladdr(addr.cast(), info.as_mut_ptr()) != 0 {
			let info = info.assume_init();
			let filename = dlopen_fname(ffi::CStr::from_ptr(info.dli_fname));
			let handle = c::dlopen(filename, c::RTLD_NOW | c::RTLD_LOCAL);
			ptr::NonNull::new(handle).map(Self)
		} else {
			None
		}
	}
}
impl Drop for InnerLibrary {
	fn drop(&mut self) {
		unsafe { c::dlclose(self.0.as_ptr()) };
	}
}

#[cfg(target_os = "macos")]
fn get_image_count() -> &'static AtomicU32 {
	static IMAGE_COUNT: AtomicU32 = AtomicU32::new(0);
	static START: Once = Once::new();
	extern "C" fn increment_count(_: *const c::mach_header, _: isize) {
		IMAGE_COUNT.fetch_add(1, Ordering::SeqCst);
	}
	extern "C" fn decrement_count(_: *const c::mach_header, _: isize) {
		IMAGE_COUNT.fetch_sub(1, Ordering::SeqCst);
	}
	START.call_once(|| unsafe {
		c::_dyld_register_func_for_add_image(increment_count);
		c::_dyld_register_func_for_remove_image(decrement_count);
	});

	&IMAGE_COUNT
}

pub(crate) unsafe fn base_addr(symbol: *const std::ffi::c_void) -> *mut img::Image {
	#[cfg(not(target_os = "aix"))]
	{
		let mut info = mem::MaybeUninit::<c::Dl_info>::zeroed();
		if c::dladdr(symbol, info.as_mut_ptr()) != 0 {
			let info = info.assume_init();
			info.dli_fbase.cast()
		} else {
			ptr::null_mut()
		}
	}
	#[cfg(target_os = "aix")]
	{
		// aix doesn't have dladdr
		ptr::null_mut()
	}
}

#[derive(Debug)]
pub struct DlInfo {
	pub dli_fname: ffi::CString,
	pub dli_fbase: *mut img::Image,
	pub dli_sname: ffi::CString,
	pub dli_saddr: *mut ffi::c_void,
}

pub trait SymExt: Sealed {
	fn info(this: *const Symbol) -> io::Result<DlInfo>;
}

impl SymExt for Symbol {
	#[doc(alias = "dladdr")]
	fn info(this: *const Symbol) -> io::Result<DlInfo> {
		let mut info = mem::MaybeUninit::<c::Dl_info>::zeroed();
		unsafe {
			if c::dladdr(this.cast(), info.as_mut_ptr()) != 0 {
				let info = info.assume_init();
				Ok(DlInfo {
					dli_fname: ffi::CStr::from_ptr(info.dli_fname).to_owned(),
					dli_fbase: info.dli_fbase.cast(),
					dli_sname: ffi::CStr::from_ptr(info.dli_sname).to_owned(),
					dli_saddr: info.dli_saddr,
				})
			} else {
				// dlerror isn't available for dlinfo, so I can only provide a general error message here
				Err(io::Error::new(
					io::ErrorKind::Other,
					"Failed to retrieve symbol information",
				))
			}
		}
	}
}

#[cfg(target_env = "gnu")]
unsafe fn iter_phdr<F>(mut f: F) -> ffi::c_int
where
	F: FnMut(*mut c::dl_phdr_info, usize) -> ffi::c_int,
{
	unsafe extern "C" fn callback<F>(
		info: *mut c::dl_phdr_info,
		size: usize,
		data: *mut ffi::c_void,
	) -> ffi::c_int
	where
		F: FnMut(*mut c::dl_phdr_info, usize) -> ffi::c_int,
	{
		let f = data as *mut F;
		(*f)(info, size)
	}
	c::dl_iterate_phdr(callback::<F>, &mut f as *mut _ as *mut _)
}

#[cfg(target_env = "gnu")]
pub(crate) unsafe fn load_objects() -> io::Result<Vec<weak::Weak>> {
	let mut data = Vec::new();
	let _ = iter_phdr(|info, _| {
		let path_name = if (*info).dlpi_name.is_null() {
			None
		} else if (*info).dlpi_name.read() == 0i8 {
			std::env::current_exe().ok()
		} else {
			let path = ffi::CStr::from_ptr((*info).dlpi_name);
			let path = ffi::OsStr::from_bytes(path.to_bytes());
			Some(PathBuf::from(path))
		};
		let weak_ptr = weak::Weak {
			base_addr: (*info).dlpi_addr as *mut img::Image,
			path_name,
		};
		data.push(weak_ptr);
		0
	});
	Ok(data)
}

#[cfg(target_os = "macos")]
pub(crate) unsafe fn load_objects() -> io::Result<Vec<weak::Weak>> {
	let mut data = Vec::new();
	let _ = get_image_count().fetch_update(Ordering::SeqCst, Ordering::SeqCst, |image_index| {
		data.clear();
		for image_index in 0..image_index {
			let path = ffi::CStr::from_ptr(c::_dyld_get_image_name(image_index));
			let path = ffi::OsStr::from_bytes(path.to_bytes());
			let weak_ptr = weak::Weak {
				base_addr: c::_dyld_get_image_header(image_index) as *const img::Image,
				path_name: Some(PathBuf::from(path)),
			};
			data.push(weak_ptr);
		}
		Some(image_index)
	});
	Ok(data)
}

pub(crate) unsafe fn hdr_size(hdr: *const img::Image) -> io::Result<usize> {
	const MH_MAGIC: &[u8] = &0xfeedface_u32.to_le_bytes();
	const MH_MAGIC_64: &[u8] = &0xfeedfacf_u32.to_le_bytes();
	const ELF_MAGIC: &[u8] = &[0x7f, b'E', b'L', b'F'];

	let magic_len: usize = if cfg!(windows) { 2 } else { 4 };
	let magic: &[u8] = std::slice::from_raw_parts(hdr.cast(), magic_len);
	match magic {
		MH_MAGIC => {
			let hdr = hdr as *const c::mach_header;
			Ok(mem::size_of::<c::mach_header>() + (*hdr).sizeofcmds as usize)
		}
		MH_MAGIC_64 => {
			let hdr = hdr as *const c::mach_header_64;
			Ok(mem::size_of::<c::mach_header_64>() + (*hdr).sizeofcmds as usize)
		}
		ELF_MAGIC => {
			let data: *const u8 = hdr as *const u8;
			match *data.offset(4) {
				c::ELFCLASS32 => {
					let hdr = hdr as *const c::Elf32_Ehdr;
					Ok((*hdr).e_ehsize as usize)
				}
				c::ELFCLASS64 => {
					let hdr = hdr as *const c::Elf64_Ehdr;
					Ok((*hdr).e_ehsize as usize)
				}
				_ => Err(io::Error::new(
					io::ErrorKind::InvalidData,
					"invalid ELF file",
				)),
			}
		}
		_ => Err(io::Error::new(
			io::ErrorKind::Other,
			"unknown header detected",
		)),
	}
}

pub(crate) unsafe fn hdr_path(hdr: *const img::Image) -> io::Result<PathBuf> {
	#[cfg(not(target_os = "aix"))]
	{
		let mut result = Err(io::Error::new(
			io::ErrorKind::NotFound,
			"Header path not found",
		));
		let mut info = mem::MaybeUninit::<c::Dl_info>::zeroed();
		if c::dladdr(hdr as *const _, info.as_mut_ptr()) != 0 {
			let info = info.assume_init();
			let path = ffi::CStr::from_ptr(info.dli_fname);
			let path = ffi::OsStr::from_bytes(path.to_bytes());
			result = Ok(path.into());
		} else {
			let this = InnerLibrary::this()?;
			if this.to_ptr() == hdr {
				result = std::env::current_exe();
			}
		}
		result
	}
	#[cfg(target_os = "aix")]
	{
		Err(io::Error::new(
			io::ErrorKind::Unsupported,
			"path retrieval is unsupported on AIX",
		))
	}
}
