use std::sync::OnceLock;

use maki_config::ClockFormat;

pub fn hm(format: ClockFormat) -> &'static str {
    if is_12h(format) { "%-I:%M %p" } else { "%H:%M" }
}

pub fn hms(format: ClockFormat) -> &'static str {
    if is_12h(format) {
        "%-I:%M:%S %p"
    } else {
        "%H:%M:%S"
    }
}

fn is_12h(format: ClockFormat) -> bool {
    static SYSTEM_12H: OnceLock<bool> = OnceLock::new();
    match format {
        ClockFormat::Hour12 => true,
        ClockFormat::Hour24 => false,
        ClockFormat::System => *SYSTEM_12H.get_or_init(system_uses_12h),
    }
}

/// A POSIX `T_FMT` pattern is 12-hour when it renders a 12-hour field:
/// hour (`%I`, `%l`), AM/PM marker (`%p`), or the whole time (`%r`).
#[cfg(any(unix, test))]
fn posix_fmt_is_12h(fmt: &[u8]) -> bool {
    fmt.windows(2)
        .any(|w| matches!(w, b"%I" | b"%l" | b"%p" | b"%r"))
}

#[cfg(unix)]
fn system_uses_12h() -> bool {
    use std::ffi::CStr;
    unsafe {
        let loc = libc::newlocale(libc::LC_TIME_MASK, c"".as_ptr(), std::ptr::null_mut());
        if loc.is_null() {
            return false;
        }
        let fmt = libc::nl_langinfo_l(libc::T_FMT, loc);
        let uses_12h = !fmt.is_null() && posix_fmt_is_12h(CStr::from_ptr(fmt).to_bytes());
        libc::freelocale(loc);
        uses_12h
    }
}

#[cfg(windows)]
fn system_uses_12h() -> bool {
    use windows_sys::Win32::Globalization::{GetLocaleInfoEx, LOCALE_STIMEFORMAT};
    let mut buf = [0u16; 128];
    let len = unsafe {
        GetLocaleInfoEx(
            std::ptr::null(),
            LOCALE_STIMEFORMAT,
            buf.as_mut_ptr(),
            buf.len() as i32,
        )
    };
    // `len` counts the terminating NUL. Windows patterns spell 24-hour
    // hours as `H` and 12-hour ones as `h`, so no `H` means 12-hour.
    len > 1 && !buf[..(len - 1) as usize].contains(&u16::from(b'H'))
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::posix_fmt_is_12h;

    #[test_case(b"%r", true ; "twelve_hour_r")]
    #[test_case(b"%I:%M:%S %p", true ; "twelve_hour_i_and_p")]
    #[test_case(b"%l:%M %p", true ; "twelve_hour_l")]
    #[test_case(b"%T", false ; "twenty_four_hour_t")]
    #[test_case(b"%H:%M:%S", false ; "twenty_four_hour_h")]
    #[test_case(b"", false ; "empty_defaults_to_24h")]
    fn posix_fmt(fmt: &[u8], expected: bool) {
        assert_eq!(posix_fmt_is_12h(fmt), expected);
    }
}
