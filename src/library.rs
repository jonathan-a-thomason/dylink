// Copyright (c) 2023 Jonathan "Razordor" Alan Thomason

use crate::loader::Loader;
use crate::SymAddr;
use std::io;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{LockResult, Mutex, MutexGuard, PoisonError};

use crate::loader::Close;

mod guard;
mod lock;

/// An RAII implementation of a “scoped lock” of a mutex.
/// When this structure is dropped (falls out of scope), the lock will be unlocked.
///
/// The data protected by the mutex can be accessed through this guard via its [`find_and_swap`](LibraryGuard::find_and_swap) implementation.
///
/// This structure is created by the [`lock`](Library::lock) method on [`Library`].
#[derive(Debug)]
pub struct LibraryGuard<'a, L: Loader> {
	libs: &'a [&'a str],
	guard: MutexGuard<'a, Option<L>>,
}

// An RAII implementation of a “scoped lock” of a mutex.
/// When this structure is dropped (falls out of scope), the lock will be unlocked.
///
/// The data protected by the mutex can be accessed through this guard via its [`find_and_swap`](CloseableLibraryGuard::find_and_swap) implementation.
///
/// This structure is created by the [`lock`](CloseableLibrary::lock) method on [`CloseableLibrary`].
#[derive(Debug)]
pub struct CloseableLibraryGuard<'a, L: Loader> {
	libs: &'a [&'a str],
	guard: MutexGuard<'a, (Option<L>, Vec<(&'static AtomicPtr<()>, AtomicSymAddr)>)>,
}

type AtomicSymAddr = AtomicPtr<()>;

mod sealed {
	use super::*;
	pub trait Sealed {}
	impl<L: Loader> Sealed for Library<'_, L> {}
	impl<L: Close> Sealed for CloseableLibrary<'_, L> {}
}

/// Implements constraint to use the [`dylink`](crate::dylink) attribute macro `library` parameter.
pub trait LibraryLock<'a>: sealed::Sealed {
	type Guard: 'a;
	/// Acquires a mutex, blocking the current thread until it is able to do so.
	///
	/// This function will block the local thread until it is available to acquire the mutex. Upon returning, the thread is the only thread with the lock held. An RAII guard is returned to allow scoped unlock of the lock. When the guard goes out of scope, the mutex will be unlocked.
	///
	/// The exact behavior on locking a mutex in the thread which already holds the lock is left unspecified. However, this function will not return on the second call (it might panic or deadlock, for example).
	/// # Errors
	///
	/// If another user of this mutex panicked while holding the mutex, then this call will return an error once the mutex is acquired.
	/// # Panics
	///
	/// This function might panic when called if the lock is already held by the current thread.
	fn lock(&'a self) -> LockResult<Self::Guard>;
}

/// An object providing access to a lazily loaded library on the filesystem.
///
/// This object is designed to be used with [`dylink`](crate::dylink) for subsequent zero overhead calls.
#[derive(Debug)]
pub struct Library<'a, L: Loader> {
	libs: &'a [&'a str],
	// library handle
	hlib: Mutex<Option<L>>,
}

impl<'a, L: Loader> Library<'a, L> {
	/// Constructs a new `Library`.
	///
	/// This function accepts a slice of paths the Library will attempt to load from
	/// by priority (where `0..n`, index `0` is highest, and `n` is lowest), but only the first
	/// library successfully loaded will be used. The reason is to provide fallback
	/// mechanism in case the shared library is in a seperate directory or may have a variety
	/// of names.
	///
	/// *Note: If `libs` is empty, the library cannot load.*
	///
	/// # Examples
	/// ```rust
	/// # use dylink::*;
	/// static KERNEL32: Library<SelfLoader> = Library::new(&["kernel32.dll"]);
	/// ```
	pub const fn new(libs: &'a [&'a str]) -> Self {
		Self {
			libs,
			hlib: Mutex::new(None),
		}
	}
	/// Immediately loads library.
	///
	/// If library is loaded, [`true`] is returned, otherwise [`false`].
	pub fn force(this: &Library<'_, L>) -> bool {
		let mut lock = this.lock().unwrap();
		if let None = *lock.guard {
			*lock.guard = unsafe {guard::force_unchecked(this.libs)};
		}
		lock.guard.is_some()
	}
}

/// An object providing access to a lazily loaded closeable library on the filesystem.
///
/// This object is designed to be used with [`dylink`](crate::dylink) for subsequent zero overhead calls.
pub struct CloseableLibrary<'a, L: Close> {
	libs: &'a [&'a str],
	inner: Mutex<(Option<L>, Vec<(&'static AtomicPtr<()>, AtomicSymAddr)>)>,
}

impl<'a, L: Close> CloseableLibrary<'a, L> {
	/// Constructs a new `CloseableLibrary`.
	///
	/// This function accepts a slice of paths the Library will attempt to load from
	/// by priority (where `0..n`, index `0` is highest, and `n` is lowest), but only the first
	/// library successfully loaded will be used. The reason is to provide fallback
	/// mechanism in case the shared library is in a seperate directory or may have a variety
	/// of names.
	///
	/// *Note: If `libs` is empty, the library cannot load*
	pub const fn new(libs: &'a [&'a str]) -> Self {
		Self {
			libs,
			inner: Mutex::new((None, Vec::new())),
		}
	}
	/// Immediately loads library.
	///
	/// If library is loaded, [`true`] is returned, otherwise [`false`].
	pub fn force(this: &CloseableLibrary<'_, L>) -> bool {
		let mut lock = this.lock().unwrap();
		if let None = lock.guard.0 {
			lock.guard.0 = unsafe {guard::force_unchecked(this.libs)};
		}
		lock.guard.0.is_some()
	}
}
