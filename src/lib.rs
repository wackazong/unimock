//!
//! `unimock` is a library that makes it easy to create mock objects that implement _multiple traits_ at the same time.
//!
//! unimock exports a single type, [Unimock], that will implement all your annotated traits:
//!
//! ```rust
//! #![feature(generic_associated_types)]
//! use unimock::*;
//! #[unimock]
//! trait Foo {
//!     fn foo(&self) -> i32;
//! }
//!
//! #[unimock]
//! trait Bar {
//!     fn bar(&self) -> i32;
//! }
//!
//! fn sum(foobar: impl Foo + Bar) -> i32 {
//!     foobar.foo() + foobar.bar()
//! }
//!
//! fn test() {
//!     assert_eq!(
//!         42,
//!         sum(
//!             mock(Foo__foo, |each| {
//!                 each.call(matching!()).returns(40);
//!             })
//!             .also(Bar__bar, |each| {
//!                 each.call(matching!()).returns(2);
//!             })
//!         )
//!     );
//! }
//! ```
//!
//! `unimock` also works with `async_trait`:
//!
//! ```rust
//! #![feature(generic_associated_types)]
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

/// Types for used for building and defining mock behaviour.
pub mod build;

mod counter;
mod error;
mod mock;

use std::any::TypeId;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

///
/// Autogenerate mocks for all methods in the annotated traits, and `impl` it for [Unimock].
///
/// Mock generation happens by declaring a new [MockFn]-implementing struct for each method.
///
/// Example
/// ```rust
/// #![feature(generic_associated_types)]
/// use unimock::*;
///
/// #[unimock]
/// trait Trait1 {
///     fn a(&self) -> i32;
///     fn b(&self) -> i32;
/// }
///
/// #[unimock]
/// trait Trait2 {
///     fn c(&self) -> i32;
/// }
///
/// fn sum(obj: impl Trait1 + Trait2) -> i32 {
///     obj.a() + obj.b() + obj.c()
/// }
///
/// fn test() {
///     let single_mock: Unimock = mock(Trait1__a, |_| {});
///     sum(single_mock); // note: panics at runtime!
/// }
/// ```
pub use unimock_macros::unimock;

///
/// Macro to ease argument pattern matching.
/// This macro produces a closure expression suitable for passing to [build::Each::call].
///
/// Takes inspiration from [std::matches] and works similarly, except that the value to match
/// can be removed as a macro argument, since it is instead received as the closure argument.
///
/// Unimock uses tuples to represent multiple arguments. A single argument is not a tuple.
/// To avoid the extra set of parentheses for simple multi-argument matchers, there is
/// a special syntax that accepts several top-level patterns:
/// `matching!("a", "b")` will expand to `matching!(("a", "b"))`.
///
/// # Example
///
/// ```rust
/// # use unimock::*;
///
/// fn one_str() {
///     fn args(_: impl Fn(&(&str)) -> bool) {}
///     args(matching!("a"));
/// }
///
/// fn three_strs() {
///     fn args(_: impl Fn(&(&str, &str, &str)) -> bool) {}
///     args(matching!("a", _, "c"));
///     args(matching!(("a", "b", "c") | ("d", "e", "f")));
///     args(matching!(("a", b, "c") if b.contains("foo")));
/// }
/// ```
///
/// # Auto-"coercions"
///
/// Since the input expression being matched is generated by the macro, you would
/// normally suffer from the following problem when matching some non-`&str` function input:
///
/// ```compile_fail
/// # fn test() -> bool {
/// let string = String::new();
/// match &string {
///     "foo" => true, // expected struct `String`, found `str`
///     _ => false,
/// # }
/// }
/// ```
///
/// To help ergonomics, the `matching` macro recognizes certain literals used in the
/// patterns, and performs appropriate type conversion at the correct places:
///
/// ```rust
/// # use unimock::*;
/// struct Newtype(String);
///
/// fn exotic_strings() {
///     fn args(_: impl Fn(&(String, std::borrow::Cow<'static, str>, Newtype, i32)) -> bool) {}
///     args(matching!(("a", _, "c", _) | (_, "b", _, 42)));
/// }
///
/// // Newtype works by implementing the following:
/// impl std::convert::AsRef<str> for Newtype {
///     fn as_ref(&self) -> &str {
///         self.0.as_str()
///     }
/// }
/// ```
///
/// Internally it works by calling [as_str_ref] on inputs matched by a string literal.
pub use unimock_macros::matching;

#[derive(Clone, Copy)]
enum FallbackMode {
    Error,
    Unmock,
}

/// Unimock's purpose is to be an implementor of downstream traits via mock objects.
/// A single mock object provides the implementation of a single trait method.
///
/// The interaction with these mock objects always happen via the Unimock facade and
/// the traits that it implements.
pub struct Unimock {
    fallback_mode: FallbackMode,
    original_instance: bool,
    state: Arc<SharedState>,
}

struct SharedState {
    impls: HashMap<TypeId, mock::DynImpl>,
    panic_reason: Mutex<Option<error::MockError>>,
}

// TODO: Should implement Clone using Arc for data.
// When the _original_ gets dropped, it should verify that there are no copies left alive.

impl Unimock {
    /// Evaluate a [MockFn] given some inputs, to produce its output.
    /// Any failure will result in panic.
    pub fn eval<'i, F: MockFn + 'static>(&'i self, inputs: F::Inputs<'i>) -> F::Output {
        match mock::eval::<F>(
            self.state.impls.get(&TypeId::of::<F>()),
            inputs,
            self.fallback_mode,
        ) {
            Ok(ConditionalEval::Yes(output)) => output,
            Ok(ConditionalEval::No(_)) => panic!(
                "{}",
                self.prepare_panic(error::MockError::CannotUnmock { name: F::NAME })
            ),
            Err(mock_error) => panic!("{}", self.prepare_panic(mock_error)),
        }
    }

    /// Conditionally evaluate a [MockFn] given some inputs.
    /// Unimock conditionally evaluates it based on internal state.
    /// There are two outcomes, either it is evaluated producing outputs,
    /// or it stays unevaluated and returns its inputs back to the caller,
    /// with the intention of the caller then _unmocking_ the call.
    pub fn conditional_eval<'i, F: MockFn + 'static>(
        &'i self,
        inputs: F::Inputs<'i>,
    ) -> ConditionalEval<'i, F> {
        match mock::eval(
            self.state.impls.get(&TypeId::of::<F>()),
            inputs,
            self.fallback_mode,
        ) {
            Ok(evaluated) => evaluated,
            Err(mock_error) => panic!("{}", self.prepare_panic(mock_error)),
        }
    }

    fn prepare_panic(&self, error: error::MockError) -> String {
        let msg = error.to_string();

        let mut panic_reason = self.state.panic_reason.lock().unwrap();
        if panic_reason.is_none() {
            *panic_reason = Some(error.clone());
        }

        msg
    }
}

impl Clone for Unimock {
    fn clone(&self) -> Unimock {
        Unimock {
            fallback_mode: self.fallback_mode,
            original_instance: false,
            state: self.state.clone(),
        }
    }
}

impl Drop for Unimock {
    fn drop(&mut self) {
        // skip verification if not the original instance.
        if !self.original_instance {
            return;
        }

        // skip verification if already panicking in the original thread.
        if std::thread::panicking() {
            return;
        }

        // if already panicked, it must be in another thread. Forward that panic.
        {
            let mut panic_reason = self.state.panic_reason.lock().unwrap();
            if let Some(panic_reason) = panic_reason.take() {
                panic!("{}", panic_reason.to_string());
            }
        }

        let mut mock_errors = Vec::new();
        for (_, dyn_impl) in self.state.impls.iter() {
            dyn_impl.0.verify(&mut mock_errors);
        }

        if !mock_errors.is_empty() {
            let error_strings = mock_errors
                .into_iter()
                .map(|err| err.to_string())
                .collect::<Vec<_>>();
            panic!("{}", error_strings.join("/n"));
        }
    }
}

///
/// The main trait used for unimock configuration.
///
/// `MockFn` describes functional APIs that may be called via dispatch, a.k.a. _Inversion of Control_.
/// Virtuality should be regarded as as test-time virtuality: A virtual function is either the real deal (see [Unmock]) OR it is mocked.
///
/// In Rust, the most convenient way to perform a virtualized/dispatched function call is to
/// call a trait method.
///
/// `MockFn` only provides metadata about an API, it is not directly callable.
///
/// As this is a trait itself, it needs to be implemented to be useful. Methods are not types,
/// so we cannot implement `MockFn` for those. But a surrogate type can be introduced:
///
/// ```rust
/// trait ILoveToMock {
///     fn method(&self);
/// }
///
/// // The method can be referred to via the following empty surrogate struct:
/// struct ILoveToMock__method;
///
/// /* impl MockFn for Mockable_method ... */
/// ```
///
pub trait MockFn: Sized + 'static {
    /// The inputs to the function.
    type Inputs<'i>;

    /// The output of the function.
    type Output;

    /// The number of inputs.
    const N_INPUTS: u8;

    /// The name to use for runtime errors.
    const NAME: &'static str;

    fn stub<'c, S>(setup: S) -> build::Clause
    where
        for<'i> Self::Inputs<'i>: std::fmt::Debug,
        S: FnOnce(&mut build::Each<Self>) + 'c,
    {
        let mut each = build::Each::new(mock::InputDebugger::new_debug());
        setup(&mut each);
        each.to_clause()
    }

    fn nodebug_stub<'c, S>(setup: S) -> build::Clause
    where
        S: FnOnce(&mut build::Each<Self>) + 'c,
    {
        let mut each = build::Each::new(mock::InputDebugger::new_nodebug());
        setup(&mut each);
        each.to_clause()
    }

    fn next_call<'c, M>(matching: M) -> build::ResponseBuilder<'c, Self>
    where
        for<'i> Self::Inputs<'i>: std::fmt::Debug,
        M: (for<'i> Fn(&Self::Inputs<'i>) -> bool) + Send + Sync + 'static,
    {
        build::ResponseBuilder::new_standalone(mock::TypedMockImpl::new_standalone(
            mock::InputDebugger::new_debug(),
            Box::new(matching),
        ))
    }
}

/// [MockFn] with the ability to unmock into a unique true implementation.
///
/// A true implementation must be a standalone function, not part of a trait,
/// where the first parameter is generic (a `self`-replacement), and the rest of the parameters are
/// identical to [MockFn::Inputs]:
///
/// ```rust
/// # #![feature(generic_associated_types)]
/// # use unimock::*;
/// #[unimock(unmocked=[my_original])]
/// trait DoubleNumber {
///     fn double_number(&self, a: i32) -> i32;
/// }
///
/// // The true implementation is a regular, generic function which performs number doubling!
/// fn my_original<T>(_: T, a: i32) -> i32 {
///     a * 2
/// }
/// ```
///
/// The unmock feature makes sense when the reason to define a mockable trait
/// is _solely_ for the purpose of inversion-of-control at test-time: Release code
/// need only one way to double a number.
///
/// Standalone functions enables arbitrarily deep integration testing
/// in unimock-based application architectures. When unimock calls the true implementation,
/// it inserts itself as the generic first parameter. When this parameter is
/// bounded by traits, the original `fn` is given capabilities to call other APIs,
/// though only indirectly. Each method invocation happening during a test will invisibly pass
/// through unimock, resulting in a great level of control. Consider:
///
/// ```rust
/// # #![feature(generic_associated_types)]
/// # use unimock::*;
/// #[unimock(unmocked=[my_factorial])]
/// trait Factorial {
///     fn factorial(&self, input: u32) -> u32;
/// }
///
/// // will it eventually panic?
/// fn my_factorial(f: &impl Factorial, input: u32) -> u32 {
///     f.factorial(input - 1) * input
/// }
///
/// assert_eq!(
///     120,
///     // well, not in the test, at least!
///     mock(Factorial__factorial, |each| {
///         each.call(matching!((input) if *input <= 1)).returns(1_u32); // unimock controls the API call
///         each.call(matching!(_)).unmock();
///     })
///     .factorial(5)
/// );
/// ```
///
pub trait Unmock: MockFn {}

/// Construct a unimock instance that works like a mock or a stub, from a set of [build::Clause]es.
///
/// Every call hitting the instance must be declared in advance as an input clause,
/// or else panic will ensue.
#[inline]
pub fn mock<I>(clauses: I) -> Unimock
where
    I: IntoIterator<Item = build::Clause>,
{
    mock_from_iterator(&mut clauses.into_iter(), FallbackMode::Error)
}

/// Construct a unimock instance that works like a spy, where every clause
/// acts as an override over the default behaviour, which is to hit
/// "real world" code using the [Unmock] feature.
#[inline]
pub fn spy<I>(clauses: I) -> Unimock
where
    I: IntoIterator<Item = build::Clause>,
{
    mock_from_iterator(&mut clauses.into_iter(), FallbackMode::Unmock)
}

fn mock_from_iterator(
    clause_iterator: &mut dyn Iterator<Item = build::Clause>,
    fallback_mode: FallbackMode,
) -> Unimock {
    let mut impls = HashMap::new();

    for clause in clause_iterator {
        match clause.0 {
            build::ClauseKind::Stub(mut dyn_impl) => {
                dyn_impl.0.assemble_into(&mut impls);
            }
        }
    }

    Unimock {
        fallback_mode,
        original_instance: true,
        state: Arc::new(SharedState {
            impls,
            panic_reason: Mutex::new(None),
        }),
    }
}

/// The conditional evaluation result of a [MockFn].
///
/// Used to tell trait implementations whether to do perform their own
/// evaluation of a call.
pub enum ConditionalEval<'i, F: MockFn> {
    /// Function evaluated to its output.
    Yes(F::Output),
    /// Function not yet evaluated.
    No(F::Inputs<'i>),
}

/// Conveniently leak some value to produce a static reference.
pub trait Leak {
    fn leak(self) -> &'static Self;
}

impl<T: 'static> Leak for T {
    fn leak(self) -> &'static Self {
        Box::leak(Box::new(self))
    }
}

///
/// Internal trait implemented by references that allows transforming from `&T` to `&'static T`
/// by leaking memory.
///
/// The trait is implemented for all `&T`. It allows functions to refer to the non-referenced owned value `T`,
/// and leak that.
///
pub trait LeakInto {
    type Owned: 'static;

    fn leak_into(value: Self::Owned) -> Self;
}

impl<T: 'static> LeakInto for &T {
    type Owned = T;

    fn leak_into(value: Self::Owned) -> Self {
        Box::leak(Box::new(value))
    }
}

/// Convert any type implementing `AsRef<str>` to a `&str`.
/// Used by [matching].
pub fn as_str_ref<T>(input: &T) -> &str
where
    T: AsRef<str>,
{
    input.as_ref()
}
