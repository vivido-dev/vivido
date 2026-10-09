//! Child environment defaults must never mutate an embedding host's environment.

use std::process::Command;

use vivido::tty;

#[test]
fn concurrent_setup_only_changes_explicit_child_commands() {
    let before = std::env::vars_os().collect::<std::collections::BTreeMap<_, _>>();
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                let environment = tty::setup_env();
                let mut child = Command::new("unused");
                environment.apply(&mut child);
                let term = child.get_envs().find(|(key, _)| *key == "TERM");
                assert!(term.is_some_and(|(_, value)| value.is_some()));
                child.env("TERM", "embedding-override");
                assert_eq!(
                    child.get_envs().find(|(key, _)| *key == "TERM").unwrap().1,
                    Some(std::ffi::OsStr::new("embedding-override")),
                );
            });
        }
    });
    assert_eq!(std::env::vars_os().collect::<std::collections::BTreeMap<_, _>>(), before);
}
