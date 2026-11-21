use std::process::Command;

fn tui() -> Command {
    Command::new(env!("CARGO_BIN_EXE_losslessd-tui"))
}

#[test]
fn no_daemon_exits_cleanly() {
    let dir = std::env::temp_dir().join(format!("tui-nodaemon-{}", std::process::id()));
    let sock = dir.join("dead.sock");
    let out = tui().arg("--socket").arg(&sock).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let err = String::from_utf8(out.stderr).unwrap();
    assert_eq!(
        err,
        format!(
            "losslessd-tui: connecting to {} (is losslessd running?)\n",
            sock.display()
        )
    );
}

#[test]
fn bad_profile_exits_2() {
    let out = tui().args(["-p", "bad/name"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(!out.stderr.is_empty());
}

#[test]
fn help_prints_usage() {
    let out = tui().arg("--help").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .starts_with("usage: losslessd-tui")
    );
}
