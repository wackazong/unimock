use std::panic;

use crate::mock_impl::TypedMockImpl;
use crate::property::*;
use crate::*;

impl<I: IntoIterator<Item = Clause>> From<I> for Clause {
    fn from(clauses: I) -> Self {
        Clause(ClausePrivate::Multiple(clauses.into_iter().collect()))
    }
}

pub(crate) enum ClausePrivate {
    Single(mock_impl::DynMockImpl),
    Multiple(Vec<Clause>),
}

/// Builder for defining a series of cascading call patterns on a specific [MockFn].
pub struct Each<F: MockFn> {
    patterns: Vec<mock_impl::CallPattern<F>>,
}

impl<F> Each<F>
where
    F: MockFn + 'static,
{
    /// Define the next call pattern, given some input matcher.
    ///
    /// The new call pattern will be matched after any previously defined call patterns on the same [Each] instance.
    ///
    /// The method returns a [Match], which is used to define how unimock responds to the matched call.
    pub fn call<'e, M>(&'e mut self, matching: M) -> Match<'e, F, InAnyOrder>
    where
        M: (for<'i> Fn(&<F as MockInputs<'i>>::Inputs) -> bool) + Send + Sync + 'static,
    {
        self.patterns.push(mock_impl::CallPattern {
            non_generic: mock_impl::CallPatternNonGeneric {
                call_index_range: Default::default(),
                call_counter: counter::CallCounter::new(0, counter::Exactness::AtLeast),
            },
            input_matcher: Box::new(matching),
            responders: vec![],
        });

        Match {
            pattern: PatternWrapper::Borrowed(self.patterns.last_mut().unwrap()),
            response_index: 0,
            ordering: InAnyOrder,
        }
    }

    pub(crate) fn new() -> Self {
        Self { patterns: vec![] }
    }

    pub(crate) fn to_clause(self) -> Clause {
        Clause(ClausePrivate::Single(mock_impl::DynMockImpl::new(
            Box::new(mock_impl::TypedMockImpl::from_stub_patterns(self.patterns)),
            mock_impl::PatternMatchMode::InAnyOrder,
        )))
    }
}

pub(crate) enum PatternWrapper<'p, F: MockFn> {
    Borrowed(&'p mut mock_impl::CallPattern<F>),
    Owned(mock_impl::CallPattern<F>),
}

impl<'p, F: MockFn> PatternWrapper<'p, F> {
    fn get_mut(&mut self) -> &mut mock_impl::CallPattern<F> {
        match self {
            PatternWrapper::Borrowed(pattern) => *pattern,
            PatternWrapper::Owned(pattern) => pattern,
        }
    }
}

/// A matched call pattern, ready for setting up a response.
pub struct Match<'p, F: MockFn, O: Ordering> {
    pattern: PatternWrapper<'p, F>,
    response_index: usize,
    ordering: O,
}

impl<'p, F, O> Match<'p, F, O>
where
    F: MockFn + 'static,
    O: Ordering,
{
    /// Create a new owned call pattern match.
    pub(crate) fn new_owned(pattern: mock_impl::CallPattern<F>, ordering: O) -> Self {
        Match {
            pattern: PatternWrapper::Owned(pattern),
            response_index: 0,
            ordering,
        }
    }

    /// Specify the output of the call pattern by providing a value.
    /// The output type must implement [Clone] and cannot contain non-static references.
    /// It must also be [Send] and [Sync] because unimock needs to store it.
    pub fn returns(self, value: impl Into<F::Output>) -> QuantifyResponse<'p, F, O>
    where
        F::Output: Send + Sync + Clone + 'static,
    {
        let value = value.into();
        self.responder(mock_impl::Responder::Value(Box::new(
            mock_impl::StoredValueSlot(value),
        )))
    }

    /// Specify the output of the call pattern by calling `Default::default()`.
    pub fn returns_default(self) -> QuantifyResponse<'p, F, O>
    where
        F::Output: Default,
    {
        self.responder(mock_impl::Responder::Closure(Box::new(|_| {
            Default::default()
        })))
    }

    /// Specify the output of the call to be a borrow of the provided value.
    /// This works well when the lifetime of the returned reference is the same as `self`.
    /// Using this for `'static` references will produce a runtime error. For static references, use [Match::returns_static].
    pub fn returns_ref<T>(self, value: T) -> QuantifyResponse<'p, F, O>
    where
        T: std::borrow::Borrow<F::Output> + Sized + Send + Sync + 'static,
    {
        self.responder(mock_impl::Responder::Borrowable(Box::new(value)))
    }

    /// Specify the output of the call to be a reference to static value.
    /// This must be used when the returned reference in the mocked trait is `'static`.
    pub fn returns_static(self, value: &'static F::Output) -> QuantifyResponse<'p, F, O>
    where
        F::Output: Send + Sync + 'static,
    {
        self.responder(mock_impl::Responder::StaticRefClosure(Box::new(
            move |_| value,
        )))
    }

    /// Specify the output of the call pattern by invoking the given closure that can then compute it based on input parameters.
    pub fn answers<A, R>(self, func: A) -> QuantifyResponse<'p, F, O>
    where
        A: (for<'i> Fn(<F as MockInputs<'i>>::Inputs) -> R) + Send + Sync + 'static,
        R: Into<F::Output>,
        F::Output: Sized,
    {
        self.responder(mock_impl::Responder::Closure(Box::new(move |inputs| {
            func(inputs).into()
        })))
    }

    /// Specify the output of the call pattern to be a static reference to leaked memory.
    ///
    /// The value may be based on the value of input parameters.
    ///
    /// This version will produce a new memory leak for _every invocation_ of the answer function.
    ///
    /// This method should only be used when computing a reference based
    /// on input parameters is necessary, which should not be a common use case.
    pub fn answers_leaked_ref<A, R>(self, func: A) -> QuantifyResponse<'p, F, O>
    where
        A: (for<'i> Fn(<F as MockInputs<'i>>::Inputs) -> R) + Send + Sync + 'static,
        R: std::borrow::Borrow<F::Output> + 'static,
        F::Output: Sized,
    {
        self.responder(mock_impl::Responder::StaticRefClosure(Box::new(
            move |inputs| {
                let value = func(inputs);
                let leaked_ref = Box::leak(Box::new(value));
                <R as std::borrow::Borrow<F::Output>>::borrow(leaked_ref)
            },
        )))
    }

    /// Prevent this call pattern from succeeding by explicitly panicking with a custom message.
    pub fn panics(self, message: impl Into<String>) -> QuantifyResponse<'p, F, O> {
        let message = message.into();

        self.responder(mock_impl::Responder::Panic(message))
    }

    /// Instruct this call pattern to invoke the [Unmock]ed function.
    pub fn unmocked(self) -> QuantifyResponse<'p, F, O>
    where
        F: Unmock,
    {
        self.responder(mock_impl::Responder::Unmock)
    }

    fn responder(mut self, responder: mock_impl::Responder<F>) -> QuantifyResponse<'p, F, O> {
        self.pattern
            .get_mut()
            .responders
            .push(mock_impl::CallOrderResponder {
                response_index: self.response_index,
                responder,
            });
        QuantifyResponse {
            pattern: self.pattern,
            response_index: self.response_index,
            ordering: self.ordering,
        }
    }
}

/// Builder for defining how a call pattern gets verified.
pub struct QuantifyResponse<'p, F: MockFn, O> {
    pattern: PatternWrapper<'p, F>,
    response_index: usize,
    ordering: O,
}

impl<'p, F, O> QuantifyResponse<'p, F, O>
where
    F: MockFn + 'static,
    O: Ordering,
{
    /// Expect this call pattern to be called exactly once.
    pub fn once(mut self) -> QuantifiedResponse<'p, F, O, Exact> {
        self.pattern_call_counter()
            .add_to_minimum(1, counter::Exactness::Exact);
        self.into_exact(1)
    }

    /// Expect this call pattern to be called exactly the specified number of times.
    pub fn n_times(mut self, times: usize) -> QuantifiedResponse<'p, F, O, Exact> {
        self.pattern_call_counter()
            .add_to_minimum(times, counter::Exactness::Exact);
        self.into_exact(times)
    }

    /// Expect this call pattern to be called at least the specified number of times.
    pub fn at_least_times(mut self, times: usize) -> QuantifiedResponse<'p, F, O, AtLeast> {
        self.pattern_call_counter()
            .add_to_minimum(times, counter::Exactness::AtLeast);
        QuantifiedResponse {
            pattern: self.pattern,
            response_index: self.response_index + times,
            ordering: self.ordering,
            _repetition: AtLeast,
        }
    }

    /// Turn the call pattern into a stubbing clause, without any overall call order verification.
    pub fn in_any_order(self) -> Clause
    where
        O: Ordering<Kind = InAnyOrder>,
    {
        match self.pattern {
            PatternWrapper::Owned(pattern) => {
                Clause(ClausePrivate::Single(mock_impl::DynMockImpl::new(
                    Box::new(TypedMockImpl::from_pattern(pattern)),
                    mock_impl::PatternMatchMode::InAnyOrder,
                )))
            }
            _ => panic!("Cannot expect a next call among group of call patterns"),
        }
    }

    fn pattern_call_counter(&mut self) -> &mut counter::CallCounter {
        &mut self.pattern.get_mut().non_generic.call_counter
    }

    fn into_exact(self, times: usize) -> QuantifiedResponse<'p, F, O, Exact> {
        QuantifiedResponse {
            pattern: self.pattern,
            response_index: self.response_index + times,
            ordering: self.ordering,
            _repetition: Exact,
        }
    }
}

/// An exactly quantified response, i.e. the number of times it is expected to respond is an exact number.
pub struct QuantifiedResponse<'p, F: MockFn, O, R> {
    pattern: PatternWrapper<'p, F>,
    response_index: usize,
    ordering: O,
    _repetition: R,
}

impl<'p, F, O, R> QuantifiedResponse<'p, F, O, R>
where
    F: MockFn + 'static,
    O: Ordering,
    R: Repetition,
{
    /// Prepare to set up a new response, which will take effect after the current response has been yielded.
    /// In order to make an output sequence, the preceding output must be exactly quantified.
    pub fn then(mut self) -> Match<'p, F, O>
    where
        R: Repetition<Kind = Exact>,
    {
        // Opening for a new response, which will be non-exactly quantified unless otherwise specified, set the exactness to AtLeastPlusOne now.
        // The reason it is AtLeastPlusOne is the additive nature.
        // We do not want to add anything to the number now, because it could be added to later in QuantifyResponse.
        // We just want to express that when using `then`, it should be called at least one time, if not `then` would be unnecessary.
        self.pattern
            .get_mut()
            .non_generic
            .call_counter
            .add_to_minimum(0, counter::Exactness::AtLeastPlusOne);

        Match {
            pattern: self.pattern,
            response_index: self.response_index,
            ordering: self.ordering,
        }
    }

    /// Turn this _exactly quantified_ definition into a [Clause] expectation.
    /// The clause can be included in a sequence of ordered clauses that specify calls to different functions that must be called in the exact order specified.
    ///
    /// # Example
    /// ```rust
    /// use unimock::*;
    ///
    /// #[unimock]
    /// trait Trait {
    ///     fn method(&self, arg: i32) -> &'static str;
    /// }
    ///
    /// let m = mock([
    ///     // the first call MUST be method(1) and it will return "a"
    ///     Trait__method.next_call(matching!(1)).returns_static("a").once().in_order(),
    ///     // the second call MUST be method(2) and it will return "b"
    ///     Trait__method.next_call(matching!(2)).returns_static("b").once().in_order(),
    ///     // there may be no more calls to this mock, as it has no stubs in it
    /// ]);
    ///
    /// assert_eq!("a", m.method(1));
    /// assert_eq!("b", m.method(2));
    /// ```
    pub fn in_order(self) -> Clause
    where
        O: Ordering<Kind = InOrder>,
        R: Repetition<Kind = Exact>,
    {
        match self.pattern {
            PatternWrapper::Owned(pattern) => {
                Clause(ClausePrivate::Single(mock_impl::DynMockImpl::new(
                    Box::new(TypedMockImpl::from_pattern(pattern)),
                    mock_impl::PatternMatchMode::InOrder,
                )))
            }
            _ => panic!(),
        }
    }

    /// Turn the call pattern into a stubbing clause, without any overall call order verification.
    pub fn in_any_order(self) -> Clause
    where
        O: Ordering<Kind = InAnyOrder>,
    {
        match self.pattern {
            PatternWrapper::Owned(pattern) => {
                Clause(ClausePrivate::Single(mock_impl::DynMockImpl::new(
                    Box::new(TypedMockImpl::from_pattern(pattern)),
                    mock_impl::PatternMatchMode::InAnyOrder,
                )))
            }
            _ => panic!(),
        }
    }
}
