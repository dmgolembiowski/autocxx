// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! It would be highly desirable to share a lot of this code with `value_param.rs`
//! but this proves to be surprisingly fiddly.

use cxx::{memory::UniquePtrTarget, UniquePtr};
use std::{marker::PhantomPinned, pin::Pin};

/// A trait representing a parameter to a C++ function which is received
/// by rvalue (i.e. by move).
///
/// # Panics
///
/// The implementations of this trait which take a [`cxx::UniquePtr`] will
/// panic if the pointer is NULL.
///
/// # Safety
///
/// Implementers must guarantee that the pointer returned by `get_ptr`
/// is of the correct size and alignment of `T`.
pub unsafe trait RValueParam<T> {
    /// Any stack storage required. If, as part of passing to C++,
    /// we need to store a temporary copy of the value, this will be `T`,
    /// otherwise `()`.
    #[doc(hidden)]
    type StackStorage;
    /// Populate the stack storage given as a parameter. Only called if you
    /// return `true` from `needs_stack_space`.
    ///
    /// # Safety
    ///
    /// Callers must guarantee that this object will not move in memory
    /// between this call and any subsequent `get_ptr` call or drop.
    #[doc(hidden)]
    unsafe fn populate_stack_space(self, this: Pin<&mut Option<Self::StackStorage>>);
    /// Retrieve the pointer to the underlying item, to be passed to C++.
    /// Note that on the C++ side this is currently passed to `std::move`
    /// and therefore may be mutated.
    #[doc(hidden)]
    fn get_ptr(stack: Pin<&mut Self::StackStorage>) -> *mut T;
    #[doc(hidden)]
    /// Any special drop steps required for the stack storage. This is not
    /// necessary if the `StackStorage` type is something self-dropping
    /// such as `UniquePtr`; it's only necessary if it's something where
    /// manual management is required such as `MaybeUninit`.
    fn do_drop(_stack: Pin<&mut Self::StackStorage>) {}
}

unsafe impl<T> RValueParam<T> for UniquePtr<T>
where
    T: UniquePtrTarget,
{
    type StackStorage = UniquePtr<T>;

    unsafe fn populate_stack_space(self, mut stack: Pin<&mut Option<Self::StackStorage>>) {
        // Safety: we will not move the contents of the pin.
        *Pin::into_inner_unchecked(stack.as_mut()) = Some(self)
    }

    fn get_ptr(stack: Pin<&mut Self::StackStorage>) -> *mut T {
        // Safety: we won't move/swap the contents of the outer pin, nor of the
        // type stored within the UniquePtr.
        unsafe {
            (Pin::into_inner_unchecked(
                (*Pin::into_inner_unchecked(stack))
                    .as_mut()
                    .expect("Passed a NULL UniquePtr as a C++ value parameter"),
            )) as *mut T
        }
    }
}

/// Implementation detail for how we pass rvalue parameters into C++.
/// This type is instantiated by auto-generated autocxx code each time we
/// need to pass a value parameter into C++, and will take responsibility
/// for extracting that value parameter from the [`ValueParam`] and doing
/// any later cleanup.
#[doc(hidden)]
pub struct RValueParamHandler<T, RVP: RValueParam<T>> {
    // We can't populate this on 'new' because the object may move.
    // Hence this is an Option - it's None until populate is called.
    space: Option<RVP::StackStorage>,
    _pinned: PhantomPinned,
}

impl<T, RVP: RValueParam<T>> RValueParamHandler<T, RVP> {
    /// Populate this stack space if needs be. Note safety guarantees
    /// on [`get_ptr`].
    ///
    /// # Safety
    ///
    /// Callers must guarantee that this type will not move
    /// in memory between calls to [`populate`] and [`get_ptr`].
    /// Callers must call [`populate`] exactly once prior to calling [`get_ptr`].
    pub unsafe fn populate(&mut self, param: RVP) {
        // Pinning safe due to safety guarantees on `get_ptr`
        param.populate_stack_space(Pin::new_unchecked(&mut self.space));
    }

    /// Return a pointer to the underlying value which can be passed to C++.
    /// Per the unsafety contract of [`populate`], the object must not have moved
    /// since it was created, and [`populate`] has been called exactly once
    /// prior to this call.
    pub fn get_ptr(&mut self) -> *mut T {
        // Pinning safe because of the guarantees the caller gives.
        unsafe { RVP::get_ptr(Pin::new_unchecked(self.space.as_mut().unwrap())) }
    }
}

impl<T, VP: RValueParam<T>> Default for RValueParamHandler<T, VP> {
    fn default() -> Self {
        Self {
            space: None,
            _pinned: PhantomPinned,
        }
    }
}

impl<T, VP: RValueParam<T>> Drop for RValueParamHandler<T, VP> {
    fn drop(&mut self) {
        if let Some(space) = self.space.as_mut() {
            unsafe { VP::do_drop(Pin::new_unchecked(space)) }
        }
    }
}
