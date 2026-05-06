//! Native bindings for the npm `decimal.js` / `bignumber.js`
//! arbitrary-precision-arithmetic packages. Sync, handle-based,
//! uses only perry-ffi v0.5 strings + handles.

use perry_ffi::{
    alloc_string, get_handle_mut, read_string, register_handle, Handle, JsString, StringHeader,
};
use rust_decimal::prelude::*;
use rust_decimal::Decimal;

pub struct DecimalHandle {
    value: Decimal,
}

unsafe fn read_str(ptr: *const StringHeader) -> Option<String> {
    let handle = JsString::from_raw(ptr as *mut StringHeader);
    read_string(handle).map(String::from)
}

fn register(d: Decimal) -> Handle {
    register_handle(DecimalHandle { value: d })
}

fn val(h: Handle) -> Option<Decimal> {
    get_handle_mut::<DecimalHandle>(h).map(|d| d.value)
}

#[inline]
fn b(v: bool) -> f64 {
    if v {
        1.0
    } else {
        0.0
    }
}

#[no_mangle]
pub extern "C" fn js_decimal_from_number(value: f64) -> Handle {
    register(Decimal::from_f64(value).unwrap_or(Decimal::ZERO))
}

/// # Safety
///
/// `value_ptr` must be null or a Perry-runtime `StringHeader`.
#[no_mangle]
pub unsafe extern "C" fn js_decimal_from_string(value_ptr: *const StringHeader) -> Handle {
    let s = read_str(value_ptr).unwrap_or_default();
    register(Decimal::from_str(&s).unwrap_or(Decimal::ZERO))
}

macro_rules! binop_handle {
    ($name:ident, $op:tt) => {
        #[no_mangle]
        pub extern "C" fn $name(handle: Handle, other: Handle) -> Handle {
            let a = val(handle).unwrap_or(Decimal::ZERO);
            let b = val(other).unwrap_or(Decimal::ZERO);
            register(a $op b)
        }
    };
}

macro_rules! binop_number {
    ($name:ident, $op:tt) => {
        #[no_mangle]
        pub extern "C" fn $name(handle: Handle, other: f64) -> Handle {
            let a = val(handle).unwrap_or(Decimal::ZERO);
            let b = Decimal::from_f64(other).unwrap_or(Decimal::ZERO);
            register(a $op b)
        }
    };
}

binop_handle!(js_decimal_plus, +);
binop_number!(js_decimal_plus_number, +);
binop_handle!(js_decimal_minus, -);
binop_number!(js_decimal_minus_number, -);
binop_handle!(js_decimal_times, *);
binop_number!(js_decimal_times_number, *);

#[no_mangle]
pub extern "C" fn js_decimal_div(handle: Handle, other: Handle) -> Handle {
    let a = val(handle).unwrap_or(Decimal::ZERO);
    let b = val(other).unwrap_or(Decimal::ZERO);
    if b.is_zero() {
        return register(Decimal::ZERO);
    }
    register(a / b)
}

#[no_mangle]
pub extern "C" fn js_decimal_div_number(handle: Handle, other: f64) -> Handle {
    let a = val(handle).unwrap_or(Decimal::ZERO);
    let b = Decimal::from_f64(other).unwrap_or(Decimal::ZERO);
    if b.is_zero() {
        return register(Decimal::ZERO);
    }
    register(a / b)
}

#[no_mangle]
pub extern "C" fn js_decimal_mod(handle: Handle, other: Handle) -> Handle {
    let a = val(handle).unwrap_or(Decimal::ZERO);
    let b = val(other).unwrap_or(Decimal::ZERO);
    if b.is_zero() {
        return register(Decimal::ZERO);
    }
    register(a % b)
}

#[no_mangle]
pub extern "C" fn js_decimal_pow(handle: Handle, n: f64) -> Handle {
    let a = val(handle).unwrap_or(Decimal::ZERO);
    // rust_decimal lacks an exponent op for arbitrary f64 exponents;
    // approximate via f64 round-trip — same trick perry-stdlib uses.
    let result = a
        .to_f64()
        .and_then(|af| Decimal::from_f64(af.powf(n)))
        .unwrap_or(Decimal::ZERO);
    register(result)
}

#[no_mangle]
pub extern "C" fn js_decimal_sqrt(handle: Handle) -> Handle {
    let a = val(handle).unwrap_or(Decimal::ZERO);
    let result = a
        .to_f64()
        .filter(|x| *x >= 0.0)
        .and_then(|x| Decimal::from_f64(x.sqrt()))
        .unwrap_or(Decimal::ZERO);
    register(result)
}

#[no_mangle]
pub extern "C" fn js_decimal_abs(handle: Handle) -> Handle {
    register(val(handle).unwrap_or(Decimal::ZERO).abs())
}

#[no_mangle]
pub extern "C" fn js_decimal_neg(handle: Handle) -> Handle {
    register(-val(handle).unwrap_or(Decimal::ZERO))
}

#[no_mangle]
pub extern "C" fn js_decimal_round(handle: Handle) -> Handle {
    register(val(handle).unwrap_or(Decimal::ZERO).round())
}

#[no_mangle]
pub extern "C" fn js_decimal_floor(handle: Handle) -> Handle {
    register(val(handle).unwrap_or(Decimal::ZERO).floor())
}

#[no_mangle]
pub extern "C" fn js_decimal_ceil(handle: Handle) -> Handle {
    register(val(handle).unwrap_or(Decimal::ZERO).ceil())
}

#[no_mangle]
pub extern "C" fn js_decimal_to_fixed(handle: Handle, decimals: f64) -> *const StringHeader {
    let v = val(handle).unwrap_or(Decimal::ZERO);
    let dp = decimals as u32;
    let rounded = v.round_dp(dp);
    let formatted = if dp == 0 {
        rounded.trunc().to_string()
    } else {
        format!("{:.*}", dp as usize, rounded)
    };
    alloc_string(&formatted).as_raw()
}

#[no_mangle]
pub extern "C" fn js_decimal_to_string(handle: Handle) -> *const StringHeader {
    let v = val(handle).unwrap_or(Decimal::ZERO);
    alloc_string(&v.to_string()).as_raw()
}

#[no_mangle]
pub extern "C" fn js_decimal_to_number(handle: Handle) -> f64 {
    val(handle).and_then(|v| v.to_f64()).unwrap_or(0.0)
}

#[no_mangle]
pub extern "C" fn js_decimal_eq(handle: Handle, other: Handle) -> f64 {
    b(val(handle).unwrap_or(Decimal::ZERO) == val(other).unwrap_or(Decimal::ZERO))
}

#[no_mangle]
pub extern "C" fn js_decimal_lt(handle: Handle, other: Handle) -> f64 {
    b(val(handle).unwrap_or(Decimal::ZERO) < val(other).unwrap_or(Decimal::ZERO))
}

#[no_mangle]
pub extern "C" fn js_decimal_lte(handle: Handle, other: Handle) -> f64 {
    b(val(handle).unwrap_or(Decimal::ZERO) <= val(other).unwrap_or(Decimal::ZERO))
}

#[no_mangle]
pub extern "C" fn js_decimal_gt(handle: Handle, other: Handle) -> f64 {
    b(val(handle).unwrap_or(Decimal::ZERO) > val(other).unwrap_or(Decimal::ZERO))
}

#[no_mangle]
pub extern "C" fn js_decimal_gte(handle: Handle, other: Handle) -> f64 {
    b(val(handle).unwrap_or(Decimal::ZERO) >= val(other).unwrap_or(Decimal::ZERO))
}

#[no_mangle]
pub extern "C" fn js_decimal_is_zero(handle: Handle) -> f64 {
    b(val(handle).unwrap_or(Decimal::ZERO).is_zero())
}

#[no_mangle]
pub extern "C" fn js_decimal_is_positive(handle: Handle) -> f64 {
    b(val(handle).unwrap_or(Decimal::ZERO) > Decimal::ZERO)
}

#[no_mangle]
pub extern "C" fn js_decimal_is_negative(handle: Handle) -> f64 {
    b(val(handle).unwrap_or(Decimal::ZERO) < Decimal::ZERO)
}

#[no_mangle]
pub extern "C" fn js_decimal_cmp(handle: Handle, other: Handle) -> f64 {
    let a = val(handle).unwrap_or(Decimal::ZERO);
    let b = val(other).unwrap_or(Decimal::ZERO);
    use std::cmp::Ordering;
    match a.cmp(&b) {
        Ordering::Less => -1.0,
        Ordering::Equal => 0.0,
        Ordering::Greater => 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_round_trip() {
        let a = js_decimal_from_number(1.5);
        let b = js_decimal_from_number(2.5);
        let sum = js_decimal_plus(a, b);
        assert_eq!(js_decimal_to_number(sum), 4.0);

        let prod = js_decimal_times(a, b);
        assert_eq!(js_decimal_to_number(prod), 3.75);
    }

    #[test]
    fn comparison_predicates() {
        let one = js_decimal_from_number(1.0);
        let two = js_decimal_from_number(2.0);
        assert_eq!(js_decimal_lt(one, two), 1.0);
        assert_eq!(js_decimal_gt(one, two), 0.0);
        assert_eq!(js_decimal_eq(one, one), 1.0);
        assert_eq!(js_decimal_cmp(one, two), -1.0);
        assert_eq!(js_decimal_cmp(two, one), 1.0);
        assert_eq!(js_decimal_cmp(one, one), 0.0);
    }

    #[test]
    fn divide_by_zero_returns_zero() {
        let one = js_decimal_from_number(1.0);
        let zero = js_decimal_from_number(0.0);
        let q = js_decimal_div(one, zero);
        assert_eq!(js_decimal_to_number(q), 0.0);
    }

    #[test]
    fn to_fixed_rounds() {
        let pi = js_decimal_from_number(3.14159);
        let s_ptr = js_decimal_to_fixed(pi, 2.0);
        let s = read_string(unsafe { JsString::from_raw(s_ptr as *mut _) }).expect("non-null");
        assert_eq!(s, "3.14");
    }
}
