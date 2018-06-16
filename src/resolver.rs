use std::rc::{Rc, Weak};
use std::cell::{UnsafeCell, RefCell};
use std::clone::Clone;
use std::fmt;

pub use super::evaluator::Evaluator;

type DirtyList<'t> = RefCell<Vec<Weak<AnyUnit<'t>>>>;

#[derive(Default)]
pub struct Resolver<'t> {
    dirty: Rc<DirtyList<'t>>,
}

impl<'t> Resolver<'t> {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn with(&mut self, f: impl for<'a> FnOnce(&mut MutResolver<'a, 't>)) {
        f(&mut MutResolver { resolver: self });
        self.resolve_dirty();
    }

    fn resolve_dirty(&mut self) {
        let mut v = self.dirty.borrow_mut();
        v.drain(..)
         .flat_map(|link| link.upgrade())
         .for_each(|u| unsafe { u.force(Rc::downgrade(&u)) });
    }

    pub fn get<'a, Eval: Evaluator<'t>>(
        &'a self,
        unit: &UnitRef<'t, Eval>,
    ) -> &Eval::Value {
        assert_match(&self.dirty, &unit.inner.get_dirty());

        unsafe {
            // Now all unit are resolved.
            // The returning reference will borrow *self, which prevents
            // modifying or re-resolving units.
            match *unit.inner.state.get() {
                Some(ResolveRet { value: Some(ref value), .. }) => value,
                _ => unreachable!(),
            }
        }
    }
}

fn assert_match<'t>(a: &Rc<DirtyList<'t>>, b: &Rc<DirtyList<'t>>) {
    assert!(Rc::ptr_eq(a, b), "Resolver and UnitRef mismatch");
}

pub struct MutResolver<'a, 't: 'a> {
    resolver: &'a mut Resolver<'t>,
}

impl<'a, 't: 'a> MutResolver<'a, 't> {
    pub fn new_unit<Eval>(&mut self, eval: Eval) -> UnitRef<'t, Eval>
    where Eval: Evaluator<'t> {
        let unit = Rc::new(Unit {
            dirty: Rc::downgrade(&self.resolver.dirty),
            state: UnsafeCell::new(None),
            eval:  UnsafeCell::new(eval),
        });
        let w = Rc::downgrade(&unit) as Weak<AnyUnit>;
        self.resolver.dirty.borrow_mut().push(w);
        UnitRef { inner: unit }
    }

    pub fn set<Eval>(&mut self, unit: &UnitRef<'t, Eval>, eval: Eval)
    where Eval: Evaluator<'t> {
        assert_match(&self.resolver.dirty, &unit.inner.get_dirty());

        unsafe {
            unit.inner.outdate(Rc::downgrade(&unit.inner) as Weak<AnyUnit>);

            // `self.resolver` is mutably borrowed.
            // `unit.inner.eval` is never borrowed outside the resolving
            // progress, which requires mutably borrowing the resolver.
            *unit.inner.eval.get() = eval;
        }
    }
}

pub struct UnitRef<'t, Eval: Evaluator<'t>> {
    inner: Rc<Unit<'t, Eval>>,
}

impl<'t, Eval: Evaluator<'t>> Clone for UnitRef<'t, Eval> {
    fn clone(&self) -> Self {
        UnitRef { inner: Rc::clone(&self.inner) }
    }
}

struct Unit<'t, Eval: Evaluator<'t>> { // Always pinned
    dirty: Weak<DirtyList<'t>>,
    state: UnsafeCell<Option<ResolveRet<'t, Eval::Value>>>,
    eval:  UnsafeCell<Eval>,
}

type DepLink<'t> = Rc<Weak<AnyUnit<'t>>>;
type RevDepLink<'t> = Weak<Weak<AnyUnit<'t>>>;

struct ResolveRet<'t, T> {
    rdeps: Vec<RevDepLink<'t>>,
    deps:  Vec<DepLink<'t>>,
    value: Option<T>, // None when evaluating
}

impl<'t, Eval: Evaluator<'t>> Unit<'t, Eval> {
    fn get_dirty(&self) -> Rc<DirtyList<'t>> {
        self.dirty.upgrade().expect("Unit outlives its Resolver")
    }

    unsafe fn resolve<'a>(
        &self,
        this: Weak<AnyUnit<'t>>,
    ) -> &'a mut ResolveRet<'t, Eval::Value> {
        if let Some(ref mut ret) = *self.state.get() {
            return ret;
        }
        // Mark as resolving, prevent cycle-resolving.
        *self.state.get() = Some(ResolveRet {
            rdeps: vec![],
            deps:  vec![],
            value: None,
        });

        let mut r = Require {
            unit:  this,
            deps:  RefCell::new(vec![]),
            dirty: self.get_dirty(),
        };
        // `eval` is called only once due to the resolving mark.
        let value = (*self.eval.get()).eval(&mut r);
        match *self.state.get() { // Re-get ptr. `eval` may modify `rdeps`.
            Some(ref mut ret) => {
                ret.deps = r.deps.into_inner();
                ret.value = Some(value);
                ret
            },
            None => unreachable!(),
        }
    }
}

trait AnyUnit<'t>: 't {
    unsafe fn outdate(&self, this: Weak<AnyUnit<'t>>);
    unsafe fn force(&self, this: Weak<AnyUnit<'t>>);
}

impl<'t, Eval: Evaluator<'t>> AnyUnit<'t> for Unit<'t, Eval> {
    unsafe fn outdate(&self, this: Weak<AnyUnit<'t>>) {
        if let Some(ret) = (*self.state.get()).take() {
            let dirty = self.get_dirty();
            dirty.borrow_mut().push(this);
            ret.rdeps.into_iter()
               .flat_map(|link| link.upgrade())
               .flat_map(|target| target.upgrade())
               .for_each(|down| down.outdate(Rc::downgrade(&down)));
        }
    }

    unsafe fn force(&self, this: Weak<AnyUnit<'t>>) {
        self.resolve(this);
    }
}

pub struct Require<'t> {
    unit:  Weak<AnyUnit<'t>>,
    deps:  RefCell<Vec<DepLink<'t>>>,
    dirty: Rc<DirtyList<'t>>, // For checking
}

impl<'t> Require<'t> {
    pub fn require<Eval: Evaluator<'t>>(
        &self,
        unit: &UnitRef<'t, Eval>,
    ) -> Result<&Eval::Value, CycleDepError> {
        assert_match(&self.dirty, &unit.inner.get_dirty());

        unsafe {
            let this = Rc::downgrade(&unit.inner);
            let ret = unit.inner.resolve(this as Weak<AnyUnit>);

            let link = Rc::new(self.unit.clone());
            ret.rdeps.push(Rc::downgrade(&link));
            self.deps.borrow_mut().push(link);

            match ret.value {
                Some(ref mut value) => Ok(value),
                None => Err(CycleDepError { _private: () }),
            }
        }
    }
}

pub struct CycleDepError {
    _private: (),
}

impl fmt::Debug for CycleDepError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("CycleDepError").finish()
    }
}
