// Copyright (c) 2023 Jonathan "Razordor" Alan Thomason

#[cfg_attr(windows, path = "lazyfn/win32.rs")]
#[cfg_attr(unix, path = "lazyfn/unix.rs")]
mod os;

mod loader;

use std::{
	cell,
	ffi::CStr,
	mem,
	sync::{
		self,
		atomic::{AtomicPtr, Ordering},
	},
};

use crate::error;

struct DefaultLinker;

/// Determines what library to look up when [LazyFn::try_link] is called.
#[derive(Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Hash, Debug)]
pub enum LinkType<'a> {
	/// Specifies a specialization for loading vulkan functions using vulkan loaders.
	Vulkan,
	/// Specifies a generalization for loading functions using native system loaders.
	System(&'a [&'a CStr]),
}

/// Fundamental data type of dylink.
///
/// This can be used safely without the dylink macro, however using the `dylink` macro should be preferred.
/// The provided member functions can be used from the generated macro when `strip=true` is enabled.
#[derive(Debug)]
pub struct LazyFn<'a, F: Sync + Send + Copy> {
	// It's imperative that LazyFn manages once, so that `LazyFn::try_link` is sound.
	pub(crate) once: sync::Once,
	// this is here to track the state of the instance during `LazyFn::try_link`.
	status: cell::RefCell<Option<error::DylinkError>>,
	// this exists so that `F` is considered thread-safe
	pub(crate) addr_ptr: AtomicPtr<F>,
	// The function to be called.
	// mutating this data without locks is UB.
	pub(crate) addr: cell::Cell<F>,
	fn_name: &'a CStr,
	link_ty: LinkType<'a>,
}

unsafe impl<F: Sync + Send + Copy> Sync for LazyFn<'_, F> {}

impl<'a, F: Copy + Sync + Send> LazyFn<'a, F> {
	/// Initializes a `LazyFn` with a placeholder value `thunk`.
	/// # Panic
	/// Type `F` must be the same size as a [function pointer](fn) or `new` will panic.
	#[inline]
	pub const fn new(thunk: &'a F, fn_name: &'a CStr, link_ty: LinkType<'a>) -> Self {
		// In a const context this assert will be optimized out.
		assert!(mem::size_of::<crate::FnPtr>() == mem::size_of::<F>());
		Self {
			addr_ptr: AtomicPtr::new(thunk as *const _ as *mut _),
			once: sync::Once::new(),
			status: cell::RefCell::new(None),
			addr: cell::Cell::new(*thunk),
			fn_name,
			link_ty,
		}
	}
	
	/// This is the same as [`try_link_with`](LazyFn::try_link_with), but implicitly calls system defined linker loader, such as
	/// `GetProcAddress`, and `LoadLibraryExW` for windows, or `dlsym`, and `dlopen` for unix. This function is used by the
	/// [dylink](dylink_macro::dylink) macro by default.
	/// If successful, stores address in current instance and returns a reference of the stored value.
	/// # Example
	/// ```rust
	/// # use dylink::dylink;
	/// #[dylink(name = "MyDLL.dll", strip = true)]
	/// extern "C" {
	///     fn foo();
	/// }
	/// 
	/// match foo.try_link() {
	///     Ok(func) => unsafe {func()},
	///     Err(err) => {
	///         println!("{err}")
	///     }
	/// }
	/// ```
	pub fn try_link(&self) -> crate::Result<&F> {
		self.try_link_with::<DefaultLinker>()
	}

	/// Provides a generic argument to supply a user defined linker loader to load the library.
	/// If successful, stores address in current instance and returns a reference of the stored value.
	pub fn try_link_with<L: crate::RTLinker>(&self) -> crate::Result<&F> {
		self.once.call_once(|| {
			let maybe = match self.link_ty {
				LinkType::Vulkan => unsafe { loader::vulkan_loader(self.fn_name) },
				LinkType::System(lib_list) => {
					let mut errors = vec![];
					lib_list
						.iter()
						.find_map(|lib| {
							loader::general_loader::<L>(lib, self.fn_name)
								.map_err(|e| {
									errors.push(e)
								})
								.ok()
						})
						.ok_or_else(|| {
							let mut err = vec![];
							for e in errors {
								err.push(e.to_string());
							}
							error::DylinkError::ListNotLoaded(err)
						})
				}
			};

			match maybe {
				Ok(addr) => {
					unsafe {
						self.addr.set(mem::transmute_copy(&addr));
					}
					self.addr_ptr.store(self.addr.as_ptr(), Ordering::Release);
				}
				Err(err) => {
					let _ = self.status.replace(Some(err));
				}
			}
		});
		// `call_once` is blocking, so `self.status` is read-only
		// by this point. Race conditions shouldn't occur.
		match (*self.status.borrow()).clone() {
			None => Ok(self.load(Ordering::Acquire)),
			Some(err) => Err(err),
		}
	}

	#[inline]
	fn load(&self, order: Ordering) -> &F {
		unsafe {
			self.addr_ptr
				.load(order)
				.as_ref()
				.unwrap_unchecked()
		}
	}
	/// Consumes `LazyFn` and returns the contained value.
	/// 
	/// This is safe because passing self by value guarantees that no other threads are concurrently accessing `LazyFn`.
	pub fn into_inner(self) -> F {
		self.addr.into_inner()
	}
}

impl<F: Sync + Send + Copy> std::ops::Deref for LazyFn<'_, F> {
	type Target = F;
	/// Dereferences the value atomically.
	fn deref(&self) -> &Self::Target {
		self.load(Ordering::Relaxed)
	}
}
