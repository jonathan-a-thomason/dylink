// Copyright (c) 2023 Jonathan "Razordor" Alan Thomason
use std::{
	cell, mem,
	path::Path,
	sync::{
		self,
		atomic::{AtomicPtr, Ordering},
	},
};

use crate::*;

mod loader;

/// Determines what library to look up when [LazyFn::load] is called.
#[derive(Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Hash, Debug)]
pub enum LinkType {
	/// Specifies a specialization for loading vulkan functions using vulkan loaders.
	Vulkan,
	/// Specifies a generalization for loading functions using native system loaders.
	System(&'static [&'static str]),
}

/// Fundamental data type of dylink.
///
/// This can be used safely without the dylink macro, however using the `dylink` macro should be preferred.
/// This structure can be used seperate from the dylink macro to check if the libraries exist before calling a dylink generated function.
pub struct LazyFn<F: 'static + Sync + Send> {
	// It's imperative that LazyFn manages once, so that `LazyFn::try_link` is sound.
	pub(crate) once: sync::Once,
	// this is here to track the state of the instance during `LazyFn::try_link`.
	status: cell::RefCell<Option<error::DylinkError>>,
	// The function to be called.
	// Non-function types can be stored, but obviously can't be called (call ops aren't overloaded).
	pub(crate) addr_ptr: AtomicPtr<F>,
	// This may look like a large type, but UnsafeCell is transparent, and Option can use NPO,
	// so when LazyFn is used properly `addr` is only the size of a pointer.
	pub(crate) addr: cell::UnsafeCell<F>,
	fn_name: &'static str,
	link_ty: LinkType,
}

impl<F: 'static + Copy + Sync + Send> LazyFn<F> {
	/// Initializes a `LazyFn` with a placeholder value `thunk`.
	/// # Panic
	/// Type `F` must be the same size as a [function pointer](fn) or `new` will panic.
	#[inline]
	pub const fn new(
		thunk: &'static F,
		fn_name: &'static str,
		link_ty: LinkType,
	) -> Self {
		// In a const context this assert will be optimized out.
		assert!(mem::size_of::<FnPtr>() == mem::size_of::<F>());
		Self {
			addr_ptr: AtomicPtr::new(thunk as *const _ as *mut _),
			once: sync::Once::new(),
			status: cell::RefCell::new(None),
			addr: cell::UnsafeCell::new(*thunk),
			fn_name,
			link_ty,
		}
	}
	
	/// If successful, stores address in current instance and returns a copy of the stored value.
	pub fn try_link(&self) -> Result<&F> {
		self.once.call_once(|| {
			let mut str_name: String = self.fn_name.to_owned();
			str_name.push('\0');
			let maybe = match self.link_ty {
				LinkType::Vulkan => unsafe { loader::vulkan_loader(&str_name) },
				LinkType::System(lib_list) => {
					// why is this branch so painful when I just want to call system_loader?
					// FIXME: make this branch less painful to look at

					let default_error = {
						if lib_list.len() > 1 {
							error::DylinkError::ListNotLoaded(vec![])
						} else {
							error::DylinkError::LibNotLoaded(String::new())
						}
					};
					let mut result = Err(default_error);
					for lib_name in lib_list {
						match loader::system_loader(Path::new(lib_name), str_name.as_ref()) {
							Ok(addr) => {
								result = Ok(addr);
								// success! lib and function retrieved!
								break;
							}
							Err(err) => {
								// lib detected, but function failed to load
								result = Err(err);
							}
						}
					}
					result
				}
			};

			match maybe {
				Ok(addr) => {
					let addr_ptr = self.addr.get();
					unsafe {
						addr_ptr.write(mem::transmute_copy(&addr));
					}
					self.addr_ptr.store(addr_ptr as *mut F, Ordering::Release);
				}
				Err(err) => {
					let _ = self.status.replace(Some(err));
				}
			}
		});
		// `call_once` is blocking, so `self.status` is read-only
		// by this point. Race conditions shouldn't occur.
		match (*self.status.borrow()).clone() {
			None => Ok(self.as_ref()),
			Some(err) => Err(err),
		}
	}

	// moved to private since loader should be used instead.
	#[inline]
	fn as_ref(&self) -> &F {
		unsafe {
			self.addr_ptr
				.load(Ordering::Acquire)
				.as_ref()
				.unwrap_unchecked()
		}
	}
}

unsafe impl<F: 'static + Sync + Send> Send for LazyFn<F> {}
unsafe impl<F: 'static + Sync + Send> Sync for LazyFn<F> {}

impl<F: 'static + Copy + Sync + Send> std::ops::Deref for LazyFn<F> {
	type Target = F;

	fn deref(&self) -> &Self::Target {
		self.as_ref()
	}
}
