/// Lua's `(value, err)` convention: a failed call answers with nil and a
/// message instead of raising. Both slots can be filled when a partial value is
/// still useful to the caller.
pub(crate) type Pair<T> = (Option<T>, Option<String>);

pub(crate) fn err_pair<T>(err: impl ToString) -> Pair<T> {
    (None, Some(err.to_string()))
}

pub(crate) fn pair<T, E: ToString>(result: Result<T, E>) -> Pair<T> {
    match result {
        Ok(value) => (Some(value), None),
        Err(e) => err_pair(e),
    }
}

/// Unwrap a `Result` inside a function returning `LuaResult<Pair<_>>`,
/// answering with `(nil, err)` instead of throwing.
macro_rules! try_pair {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return Ok($crate::api::util::pair::err_pair(e)),
        }
    };
}

pub(crate) use try_pair;
