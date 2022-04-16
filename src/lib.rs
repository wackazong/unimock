//!
//! `unimock` is a library that makes it easy to create mock objects that implement _multiple traits_ at the same time.
//!
//! unimock exports a single type, [Unimock], that will implement all your annotated traits:
//!
//! ```rust
//! #![feature(generic_associated_types)]
//! use unimock::*;
//! #[unimock_next]
//! trait Foo {
//!     fn foo(&self) -> i32;
//! }
//!
//! #[unimock_next]
//! trait Bar {
//!     fn bar(&self) -> i32;
//! }
//!
//! fn sum(foobar: impl Foo + Bar) -> i32 {
//!     foobar.foo() + foobar.bar()
//! }
//!
//! /*
//! fn test() {
//!     let unimock = Unimock::new()
//!         .mock(|foo: &mut MockFoo| {
//!             foo.expect_foo().return_const(40);
//!         })
//!         .mock(|bar: &mut MockBar| {
//!             bar.expect_bar().return_const(2);
//!         });
//!
//!     let answer = sum(unimock);
//!     assert_eq!(42, answer);
//! }
//! */
//!
//! fn test_next() {
//!     let unimock = Unimock::union([
//!         Foo__foo.mock(|each| {
//!             each.call(matching!()).returns(40);
//!         }),
//!         Bar__bar.mock(|each| {
//!             each.call(matching!()).returns(2);
//!         })
//!     ]);
//!
//!     assert_eq!(42, sum(unimock));
//! }
//! ```
//!
//! `unimock` uses [`mockall`] to mock single traits, which is where the `MockFoo` and `MockBar` types above come from.
//!
//! [`mockall`]: https://docs.rs/mockall/latest/mockall/
//!
//! `unimock` also works with `async_trait`:
//!
//! ```rust
//! use unimock::*;
//! use async_trait::*;
//! #[unimock]
//! #[async_trait]
//! trait Baz {
//!     async fn baz(&self, arg: String) -> i32;
//! }
//! ```

#![forbid(unsafe_code)]
// For the mock-fn feature:
#![feature(generic_associated_types)]

pub mod builders;
pub mod mock;

mod counter;

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;

///
/// Autogenerate a mock implementation of a trait.
///
/// This macro does two things:
/// 1. Autogenerate a `mockall` implementation by invoking [`mockall::automock`].
/// 2. Autogenerate an implementation for [Unimock].
///
/// [`mockall::automock`]: https://docs.rs/mockall/latest/mockall/attr.automock.html
///
/// Example
/// ```rust
/// use unimock::*;
///
/// #[unimock]
/// trait MyTrait {
///     fn foo(&self);
/// }
///
/// fn do_something(my_trait: impl MyTrait) {
///     my_trait.foo();
/// }
///
/// fn using_mockall() {
///     // Since `do_something` is only bounded by one trait, we can use the mockall type directly:
///     do_something(MockMyTrait::new()); // note: this will panic!
/// }
///
/// fn using_unimock() {
///     // If `do_something` had multiple trait bounds, we would have two choices:
///     // 1. implement a specialized mock type using `mockall::mock`
///     // 2. Just use `Unimock`:
///     do_something(Unimock::new());  // note: this will panic!
/// }
///
/// # fn main() {
/// # let _ = std::panic::catch_unwind(|| using_mockall());
/// # let _ = std::panic::catch_unwind(|| using_unimock());
/// # }
/// ```
pub use unimock_macros::unimock;

pub use unimock_macros::unimock_next;

///
/// Macro to ease argument pattern matching.
/// This macro produces a closure expression suitable for passing to [builders::Each::call].
///
/// Takes inspiration from [std::matches] and works similarly, except that the value to match
/// can be removed as a macro argument, since it is instead received as the closure argument.
///
/// Unimock uses tuples to represent multiple arguments. A single argument is not a tuple.
/// To avoid the extra set of parentheses for simple multi-argument matchers, there is
/// a special syntax that accepts several top-level patterns:
/// `matching!("a", "b")` will expand to `matching!(("a", "b"))`.
///
/// # Examples
///
/// ```rust
/// # use unimock::*;
/// fn one_str(_: impl Fn(&(&str)) -> bool) {}
/// fn three_strs(_: impl Fn(&(&str, &str, &str)) -> bool) {}
///
/// fn test() {
///     one_str(matching!("a"));
///     three_strs(matching!("a", _, "c"));
///     three_strs(matching!(("a", "b", "c") | ("d", "e", "f")));
///     three_strs(matching!(("a", b, "c") if b.contains("foo")));
/// }
///
/// ```
pub use unimock_macros::matching;

enum FallbackMode {
    Panic,
    CallOriginal,
}

impl FallbackMode {
    fn union(self, other: FallbackMode) -> Self {
        match (self, other) {
            (Self::Panic, other) => other,
            (other, Self::Panic) => other,
            (_, other) => other,
        }
    }
}

/// Unimock stores a collection of mock objects, with the end goal of implementing
/// all the mocked traits. The `impl` is convieniently generated by using the [unimock] attribute on a trait.
pub struct Unimock {
    fallback_mode: FallbackMode,
    mocks: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
    impls: HashMap<TypeId, mock::DynImpl>,
}

impl Unimock {
    /// Create a new, empty Unimock. Attempting to call implemented traits on an empty instance will panic at runtime.
    pub fn new() -> Self {
        Self {
            fallback_mode: FallbackMode::Panic,
            mocks: HashMap::new(),
            impls: HashMap::new(),
        }
    }

    ///
    /// Compose a new unimock by consuming an array of simpler unimocks by a union operation.
    /// The passed unimocks
    ///
    pub fn union<const N: usize>(unimocks: [Unimock; N]) -> Self {
        let mut impls = HashMap::new();
        let mut fallback_mode = FallbackMode::Panic;

        for unimock in unimocks.into_iter() {
            fallback_mode = fallback_mode.union(unimock.fallback_mode);
            impls.extend(unimock.impls);
        }

        Self {
            fallback_mode,
            mocks: HashMap::new(),
            impls,
        }
    }

    /// Create a unimock that instead of trying to mock, tries to call some original implementation of any API.
    ///
    /// What is considered an original implementation, is not something that unimock concerns itself with,
    /// the behaviour is completely customized for each trait and it is also opt-in.
    ///
    /// If some implementation has no way to call an "original implementation", it should panic.
    ///
    /// A call-original unimock may be `union`ed together with normal mocks, effectively
    /// creating a mix of real and mocked APIs, allowing deeper tests than mock-only mode can provide.
    pub fn call_original() -> Self {
        Self {
            fallback_mode: FallbackMode::CallOriginal,
            mocks: HashMap::new(),
            impls: HashMap::new(),
        }
    }

    pub(crate) fn with_single_mock(type_id: TypeId, dyn_impl: mock::DynImpl) -> Self {
        Self {
            fallback_mode: FallbackMode::Panic,
            mocks: HashMap::new(),
            impls: [(type_id, dyn_impl)].into(),
        }
    }

    /// Configure a specific mock. The type must implement [Default]. Each stored mock is keyed by its [TypeId],
    /// so repeatedly calling this method with the same receiving type will use the same instance.
    ///
    /// When a trait is mocked using [unimock], its _mocked implementation_ must be used in this function.
    ///
    /// # Example
    /// ```rust
    /// use unimock::*;
    ///
    /// #[unimock]
    /// trait MyTrait {}
    ///
    /// # fn main() {
    /// let unimock = Unimock::new().mock(|my_trait: &mut MockMyTrait| {
    ///     /* ... */
    /// });
    /// # }
    /// ```
    pub fn mock<M, F>(mut self, f: F) -> Self
    where
        M: Default + Send + Sync + 'static,
        F: FnOnce(&mut M),
    {
        f(self
            .mocks
            .entry(TypeId::of::<M>())
            .or_insert_with(|| Box::new(M::default()))
            .downcast_mut()
            .unwrap());

        self
    }

    /// Get a specific mock created with `mock`. Panics at runtime if the type is not registered.
    pub fn get<T: std::any::Any>(&self, trait_name: &'static str) -> &T {
        self.mocks
            .get(&TypeId::of::<T>())
            .and_then(|any| any.downcast_ref())
            .unwrap_or_else(|| panic!("{}", self.missing_trait_error(trait_name)))
    }

    pub fn get_impl<'s, M: Mock + 'static>(&'s self) -> mock::Impl<'s, M> {
        self.impls
            .get(&TypeId::of::<M>())
            .map(mock::Impl::from_storage)
            .unwrap_or_else(|| mock::Impl::from_fallback(&self.fallback_mode))
    }

    fn missing_trait_error(&self, trait_name: &'static str) -> String {
        format!("Missing mock for trait {trait_name}")
    }
}

impl std::ops::Add<Unimock> for Unimock {
    type Output = Unimock;

    fn add(self, rhs: Unimock) -> Self::Output {
        Unimock::union([self, rhs])
    }
}

impl<const N: usize> From<[Unimock; N]> for Unimock {
    fn from(array: [Unimock; N]) -> Self {
        Unimock::union(array)
    }
}

/// Union mocks together to create a single mock object
pub trait Union {
    fn union(self) -> Unimock;
}

impl<const N: usize> Union for [Unimock; N] {
    fn union(self) -> Unimock {
        Unimock::union(self)
    }
}

///
/// Trait describing a single mockable function interface.
///
/// To be useful, traits need to be implemented by types. But things we want to
/// mock are _functions_, not types. Unimock works by defining an empty struct
/// that _represents_ some trait method:
///
/// ```rust
/// trait Mockable {
///     fn method(&self);
/// }
///
/// // The method can be referred to via the following empty struct:
/// struct Mockable_method;
///
/// /* impl Mock for Mockable_method ... */
/// ```
///
pub trait Mock: Sized {
    /// The direct inputs to the mock function.
    type Inputs<'i>;

    /// The output of the mock function.
    type Output;

    const N_ARGS: u8;

    /// The name to use for runtime errors.
    const NAME: &'static str;

    /// Create a unimock instance that mocks this function.
    fn mock<F>(self, f: F) -> Unimock
    where
        Self: 'static,
        F: FnOnce(&mut builders::Each<Self>),
    {
        let mut each = builders::Each::new();
        f(&mut each);
        Unimock::with_single_mock(
            TypeId::of::<Self>(),
            mock::DynImpl::Mock(Box::new(mock::MockImpl::from_each(each))),
        )
    }
}

pub trait CallOriginal {
    // Call the original implementation of this API
    fn call_original() -> Unimock
    where
        Self: 'static,
    {
        Unimock {
            fallback_mode: FallbackMode::Panic,
            mocks: HashMap::new(),
            impls: [(TypeId::of::<Self>(), mock::DynImpl::CallOriginal)].into(),
        }
    }
}

///
/// Internal trait implemented by references that allows transforming from `&T` to `&'static T`
/// by leaking memory.
/// The trait is implemented for all `&T`. It allows functions to refer to the non-referenced owned value `T`,
/// and leak that.
///
pub trait LeakOutput {
    type Owned: 'static;

    fn leak(value: Self::Owned) -> Self;
}

impl<T: 'static> LeakOutput for &T {
    type Owned = T;

    fn leak(value: Self::Owned) -> Self {
        Box::leak(Box::new(value))
    }
}

///
/// Trait for borrowing string slices.
/// Used by string literals in the [matching!] macro.
///
pub trait AsStrRef {
    fn as_str_ref(&self) -> &str;
}

impl<T: AsRef<str>> AsStrRef for T {
    fn as_str_ref(&self) -> &str {
        self.as_ref()
    }
}
