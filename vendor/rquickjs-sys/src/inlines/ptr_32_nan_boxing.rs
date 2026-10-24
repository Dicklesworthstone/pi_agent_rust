#[inline]
pub unsafe fn JS_VALUE_GET_TAG(v: JSValue) -> i32 {
    (v >> 32) as _
}

#[inline]
pub unsafe fn JS_VALUE_GET_INT(v: JSValue) -> i32 {
    v as i32
}

#[inline]
pub unsafe fn JS_VALUE_GET_BOOL(v: JSValue) -> bool {
    v as i32 != 0
}

#[inline]
pub unsafe fn JS_VALUE_GET_PTR(v: JSValue) -> *mut c_void {
    v as isize as *mut c_void
}

#[inline]
pub const fn JS_MKVAL(tag: i32, val: i32) -> JSValue {
    ((tag as u64) << 32) | (val as u32 as u64)
}

#[inline]
pub fn JS_MKPTR(tag: i32, ptr: *mut c_void) -> JSValue {
    ((tag as u64) << 32) | (ptr as usize as u64)
}

/* quiet NaN encoding */
const JS_FLOAT64_TAG_ADDEND: i32 = 0x7ff80000 - JS_TAG_FIRST + 1;

#[cfg(test)]
#[test]
fn test_JS_FLOAT64_TAG_ADDEND() {
    assert_eq!(JS_FLOAT64_TAG_ADDEND, 0x7ff8000a);
}

#[inline]
pub unsafe fn JS_VALUE_GET_FLOAT64(v: JSValue) -> f64 {
    union U {
        v: JSValue,
        d: f64,
    }
    let u = U {
        v: v.wrapping_add((JS_FLOAT64_TAG_ADDEND as u64) << 32),
    };
    u.d
}

pub const JS_NAN: JSValue =
    (0x7ff8000000000000i64 - ((JS_FLOAT64_TAG_ADDEND as i64) << 32)) as JSValue;
//((0x7ff80000 - JS_FLOAT64_TAG_ADDEND) as u64) << 32

#[cfg(test)]
#[test]
fn test_JS_NAN() {
    assert_eq!(JS_NAN, 0xfffffff600000000);
}

#[inline]
pub fn __JS_NewFloat64(d: f64) -> JSValue {
    union U {
        v: JSValue,
        d: f64,
    }
    let u = U { d };
    unsafe {
        /* normalize NaN */
        if (u.v & 0x7fffffffffffffff) > 0x7ff0000000000000 {
            JS_NAN
        } else {
            u.v.wrapping_sub((JS_FLOAT64_TAG_ADDEND as u64) << 32)
        }
    }
}

#[inline]
pub unsafe fn JS_TAG_IS_FLOAT64(tag: i32) -> bool {
    (tag - JS_TAG_FIRST) as c_uint >= (JS_TAG_FLOAT64 - JS_TAG_FIRST) as c_uint
}

#[inline]
pub unsafe fn JS_VALUE_GET_NORM_TAG(v: JSValue) -> i32 {
    let tag = JS_VALUE_GET_TAG(v);
    if JS_TAG_IS_FLOAT64(tag) {
        JS_TAG_FLOAT64
    } else {
        tag
    }
}

#[inline]
pub unsafe fn JS_VALUE_IS_NAN(v: JSValue) -> bool {
    let tag = JS_VALUE_GET_TAG(v);
    tag == (JS_NAN >> 32) as i32
}
