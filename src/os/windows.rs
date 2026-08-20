pub(crate) fn is_user_admin() -> bool {
    is_elevated::is_elevated()
}
