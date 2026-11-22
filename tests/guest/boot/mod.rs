use crate::support;
/// Guards: 4aaef99f private state root and runtime-generated owner UUID.
#[test]
fn actual_awman_boots_a_guest() {
    let s = support::sandbox("boot::actual_awman_boots_a_guest");
    let archive = support::fixture(&s);
    support::watch(&s);
    support::session::boot(&s, support::awman_exe(), &archive);
}
