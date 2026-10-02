use nix::sys::resource::{self, Resource, rlim_t};
use nix::unistd::geteuid;

pub fn is_user_admin() -> bool {
    geteuid().is_root()
}

/// macOS's setrlimit(2) COMPATIBILITY section rejects `rlim_cur = RLIM_INFINITY` for
/// `RLIMIT_NOFILE` specifically (`EINVAL`), unlike every other resource, so an unbounded hard
/// limit is capped at this historical BSD ceiling instead of being requested outright.
const OPEN_MAX: rlim_t = 10_240;

/// The soft `RLIMIT_NOFILE` to request, given the process's current soft and hard limits.
/// Never below `soft`: raising must never lower an inherited limit (this workstation's shells
/// start at 1,048,576, which [`OPEN_MAX`] would otherwise cut down to 10,240). Never above
/// `hard`: the kernel rejects that outright. An unbounded hard limit
/// (`nix::sys::resource::RLIM_INFINITY`) is itself far larger than [`OPEN_MAX`], so
/// `hard.min(OPEN_MAX)` reads as "no ceiling" the same way for both a finite and an infinite
/// hard limit, without a separate branch for either.
fn raised_soft_limit(soft: rlim_t, hard: rlim_t) -> rlim_t {
    soft.max(hard.min(OPEN_MAX))
}

/// Raises the soft `RLIMIT_NOFILE` toward the hard limit at startup, before anything else in
/// the process opens a file, so a large scan has headroom past macOS's and Linux's ordinary
/// interactive defaults (256 and 1,024). The scan store's own descriptor bound
/// (`scan_store/session.rs`) is what keeps a scan within reach of a tight limit; this raise
/// leaves headroom for the descriptors the store does not own.
///
/// A failed raise (an already-maxed hard limit, a sandboxed environment that refuses the
/// call, ...) is not an error: excise keeps running at whatever limit it inherited, and this
/// prints nothing, since it is routine background tuning, not a user-facing condition.
pub fn raise_soft_descriptor_limit() {
    let Ok((soft, hard)) = resource::getrlimit(Resource::RLIMIT_NOFILE) else {
        return;
    };
    let target = raised_soft_limit(soft, hard);
    if target > soft {
        let _ = resource::setrlimit(Resource::RLIMIT_NOFILE, target, hard);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::resource::RLIM_INFINITY;

    #[test]
    fn raises_toward_open_max_when_hard_is_unbounded() {
        assert_eq!(raised_soft_limit(256, RLIM_INFINITY), OPEN_MAX);
    }

    #[test]
    fn never_lowers_a_soft_limit_already_above_the_target() {
        assert_eq!(raised_soft_limit(1_048_576, RLIM_INFINITY), 1_048_576);
    }

    #[test]
    fn stops_at_a_hard_limit_below_open_max() {
        assert_eq!(raised_soft_limit(256, 5_000), 5_000);
    }

    #[test]
    fn makes_no_request_when_soft_already_equals_hard() {
        assert_eq!(raised_soft_limit(256, 256), 256);
    }
}
