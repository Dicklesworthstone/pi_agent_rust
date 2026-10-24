#[inline]
pub unsafe fn JS_VALUE_GET_TAG(v: JSValue) -> i32 {
    v.tag as _
}

#[inline]
pub unsafe fn JS_VALUE_GET_NORM_TAG(v: JSValue) -> i32 {
    JS_VALUE_GET_TAG(v)
}

#[inline]
pub unsafe fn JS_VALUE_GET_INT(v: JSValue) -> i32 {
    v.u.int32
}

#[inline]
pub unsafe fn JS_VALUE_GET_BOOL(v: JSValue) -> bool {
    v.u.int32 != 0
}

#[inline]
pub unsafe fn JS_VALUE_GET_FLOAT64(v: JSValue) -> f64 {
    v.u.float64
}

#[inline]
pub unsafe fn JS_VALUE_GET_PTR(v: JSValue) -> *mut c_void {
    v.u.ptr
}

#[inline]
pub const fn JS_MKVAL(tag: i32, val: i32) -> JSValue {
    JSValue {
        u: JSValueUnion { int32: val },
        tag: tag as _,
    }
}

#[inline]
pub fn JS_MKPTR(tag: i32, ptr: *mut c_void) -> JSValue {
    JSValue {
        u: JSValueUnion { ptr },
        tag: tag as _,
    }
}

#[inline]
pub unsafe fn JS_TAG_IS_FLOAT64(tag: i32) -> bool {
    tag == JS_TAG_FLOAT64
}

pub const JS_NAN: JSValue = JSValue {
    tag: JS_TAG_FLOAT64 as _,
    u: JSValueUnion { float64: f64::NAN },
};

#[inline]
pub fn __JS_NewFloat64(d: f64) -> JSValue {
    JSValue {
        tag: JS_TAG_FLOAT64 as _,
        u: JSValueUnion { float64: d },
    }
}

#[inline]
pub unsafe fn JS_VALUE_IS_NAN(v: JSValue) -> bool {
    union U {
        d: f64,
        u: u64,
    }
    if v.tag != JS_TAG_FLOAT64 as _ {
        return false;
    }
    let u = U { d: v.u.float64 };
    (u.u & 0x7fffffffffffffff) > 0x7ff0000000000000
}
