use nix::unistd::geteuid;

pub fn is_user_admin() -> bool {
    geteuid().is_root()
}
