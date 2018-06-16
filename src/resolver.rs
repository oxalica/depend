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

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::evaluator::Value;

    type RefVec<'t> = Rc<RefCell<Vec<UnitRef<'t, Dep<'t>>>>>;

    struct Dep<'t> {
        cnt: Rc<RefCell<(usize, usize)>>,
        v:   RefVec<'t>,
        idx: Option<usize>,
        b:   i32,
    }

    impl<'t> Evaluator<'t> for Dep<'t> {
        type Value = i32;
        fn eval(&self, r: &mut Require<'t>) -> i32 {
            self.cnt.borrow_mut().0 += 1;
            self.idx.map_or(self.b, |idx| {
                match r.require(&self.v.borrow()[idx]) {
                    Ok(&x) if x != 42 => x + self.b,
                    _ => 42,
                }
            })
        }
    }

    impl<'t> Drop for Dep<'t> {
        fn drop(&mut self) {
            self.cnt.borrow_mut().1 += 1;
        }
    }

    fn get_ans<'t>(res: &Resolver<'t>, v: &RefVec<'t>) -> Vec<i32> {
        v.borrow().iter()
          .map(|u| *res.get(u))
          .collect()
    }

    fn get_cnt(c: &Rc<RefCell<(usize, usize)>>) -> (usize, usize) {
        use std::mem::replace;
        replace(&mut c.borrow_mut(), (0, 0))
    }

    #[test]
    fn basic_test() {
        let cnt = Rc::new(RefCell::new((0, 0)));
        let _keep;

        {
            let v = Rc::new(RefCell::new(vec![]));
            let mut res = Resolver::<'static>::new();
            let dep = |idx, b| Dep { cnt: cnt.clone(), v: v.clone(), idx, b };

            res.with(|res| {
                let mut mv = v.borrow_mut();
                mv.push(res.new_unit(dep(None, 1)));
                mv.push(res.new_unit(dep(Some(0), 1)));
                mv.push(res.new_unit(dep(None, 3)));

                assert_eq!(get_cnt(&cnt), (0, 0));
            });
            assert_eq!(get_cnt(&cnt), (3, 0)); // Eval [0], [1], [2]
            assert_eq!(get_ans(&res, &v), [1, 2, 3]);
            assert_eq!(get_cnt(&cnt), (0, 0)); // Use cache
            assert_eq!(get_ans(&res, &v), [1, 2, 3]);
            assert_eq!(get_cnt(&cnt), (0, 0)); // Still cache

            res.with(|res| {
                res.set(&v.borrow()[0], dep(None, 2));
                assert_eq!(get_cnt(&cnt), (0, 1)); // Drop the old
            });
            assert_eq!(get_cnt(&cnt), (2, 0)); // re-eval [0], [1]
            assert_eq!(get_ans(&res, &v), [2, 3, 3]);

            res.with(|res| {
                res.set(&v.borrow()[0], dep(Some(1), 1));
                assert_eq!(get_cnt(&cnt), (0, 1)); // Drop the old
            });
            assert_eq!(get_cnt(&cnt), (2, 0)); // Re-eval [0], [1]
            assert_eq!(get_ans(&res, &v), [42, 42, 3]);

            _keep = v.borrow()[2].clone();
            v.borrow_mut().pop();
            assert_eq!(get_cnt(&cnt), (0, 0)); // `_keep` still hold it
            v.borrow_mut().pop();
            assert_eq!(get_cnt(&cnt), (0, 1)); // Drop if no references,
            v.borrow_mut().clear();            // regardless of dependencies
            assert_eq!(get_cnt(&cnt), (0, 1));
        }
        assert_eq!(get_cnt(&cnt), (0, 0)); // FIXME: Resolver is already died.
        drop(_keep);
        assert_eq!(get_cnt(&cnt), (0, 1));
    }

    #[test]
    fn closure_test() {
        let eval_count = Rc::new(RefCell::new(0));

        let mut a = None;
        let mut res = Resolver::<'static>::new();
        let (mut b, mut c) = (None, None);
        res.with(|res| {
            a = Some(res.new_unit(Value::new(1)));
            let (a_, eval_count_) = (a.clone().unwrap(), eval_count.clone());
            // b = Some(res.new_unit(move |r| { // Fail to infer
            // b = Some(res.new_unit(move |r: &mut Require| { // Fail to infer
            b = Some(res.new_unit(move |r: &mut Require<'static>| {
                *eval_count_.borrow_mut() += 1;
                let x = *r.require(&a_).unwrap();
                (x, x + 1)
            }));
            c = Some(res.new_unit(Value::new(3)));

            assert_eq!(*eval_count.borrow(), 0); // Not resolved now
        });
        assert_eq!(*eval_count.borrow(), 1);
        assert_eq!(res.get(&a.as_ref().unwrap()), &1);
        assert_eq!(res.get(&b.as_ref().unwrap()), &(1, 2));
        assert_eq!(res.get(&c.as_ref().unwrap()), &3);

        res.with(|res| res.set(&a.as_ref().unwrap(), Value::new(11)));
        assert_eq!(*eval_count.borrow(), 2);
        assert_eq!(res.get(&a.as_ref().unwrap()), &11);
        assert_eq!(res.get(&b.as_ref().unwrap()), &(11, 12));
        assert_eq!(res.get(&c.as_ref().unwrap()), &3);
    }
}
