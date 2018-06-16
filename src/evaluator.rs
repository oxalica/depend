use std::cell::Cell;
use super::resolver::Require;

pub trait Evaluator<'t>: 't {
    type Value: 't;

    fn eval(&self, r: &mut Require<'t>) -> Self::Value;
}

impl<'t, T: 't, F: Fn(&mut Require<'t>) -> T + 't> Evaluator<'t> for F {
    type Value = T;

    fn eval(&self, r: &mut Require<'t>) -> Self::Value {
        self(r)
    }
}

pub struct Value<T> {
    value: Cell<Option<T>>,
}

impl<T> Value<T> {
    pub fn new(value: T) -> Self {
        Value { value: Cell::new(Some(value)) }
    }
}

impl<'t, T: 't> Evaluator<'t> for Value<T> {
    type Value = T;

    fn eval(&self, _r: &mut Require<'t>) -> Self::Value {
        self.value.replace(None).unwrap() // Only run once
    }
}
