// build-pass

// A test to ensure generators capture values that were conditionally dropped,
// and also that values that are dropped along all paths to a yield do not get
// included in the generator type.

#![feature(generators, negative_impls)]

#![allow(unused_assignments, dead_code)]

struct Ptr;
impl<'a> Drop for Ptr {
    fn drop(&mut self) {}
}

struct NonSend {}
impl !Send for NonSend {}

fn assert_send<T: Send>(_: T) {}

// This test case is reduced from src/test/ui/drop/dynamic-drop-async.rs
fn one_armed_if(arg: bool) {
    let _ = || {
        let arr = [Ptr];
        if arg {
            drop(arr);
        }
        yield;
    };
}

fn two_armed_if(arg: bool) {
    assert_send(|| {
        let arr = [Ptr];
        if arg {
            drop(arr);
        } else {
            drop(arr);
        }
        yield;
    })
}

fn if_let(arg: Option<i32>) {
    let _ = || {
        let arr = [Ptr];
        if let Some(_) = arg {
            drop(arr);
        }
        yield;
    };
}

fn reinit() {
    let _ = || {
        let mut arr = [Ptr];
        drop(arr);
        arr = [Ptr];
        yield;
    };
}

fn loop_uninit() {
    let _ = || {
        let mut arr = [Ptr];
        let mut count = 0;
        drop(arr);
        while count < 3 {
            yield;
            arr = [Ptr];
            count += 1;
        }
    };
}

fn main() {
    one_armed_if(true);
    if_let(Some(41));
    reinit();
    loop_uninit();
}
