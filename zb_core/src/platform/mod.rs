pub mod termux;

/// Host is Android (Termux), not GNU/Linux.
pub fn is_android_host() -> bool {
    cfg!(target_os = "android")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_host_matches_target_os_cfg() {
        assert_eq!(is_android_host(), cfg!(target_os = "android"));
    }
}
