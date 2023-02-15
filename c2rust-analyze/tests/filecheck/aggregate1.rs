// CHECK-LABEL: final labeling for "aggregate1_array"
// CHECK-DAG: ([[@LINE+1]]: p): &std::cell::Cell<i32>
pub unsafe fn aggregate1_array(p: *mut i32) {
    // CHECK-DAG: ([[@LINE+1]]: arr): [&std::cell::Cell<i32>; 3]
    let arr = [p, p, p];
    *arr[0] = 1;
}

// CHECK-LABEL: final labeling for "aggregate1_array1"
// CHECK-DAG: ([[@LINE+1]]: p): &mut i32
pub unsafe fn aggregate1_array1(p: *mut i32) {
    // CHECK-DAG: ([[@LINE+1]]: arr): [&mut i32; 1]
    let arr = [p];
    *arr[0] = 1;
}

pub unsafe fn repeat() {
    let x = 22;
    let _buf: [u32; 22] = [x; 22];
}

struct S<T> {
    y: u32,
    x: T,
    px: *const T,
}

pub fn aggregate() {
    let x = 0;
    let p: u16 = 0;
    let i = S {
        y: x,
        x: p,
        px: std::ptr::addr_of!(p),
    };
}
