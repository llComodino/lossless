use std::process::Command;

fn tui() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_losslessd-tui"));
    c.env("XDG_CONFIG_HOME", "/nonexistent/lossless-tui-test");
    c
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

mod parity {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::thread;

    use lossless_daemon::{Daemon, bind};
    use lossless_engine::Player;
    use lossless_engine::fixtures::{BLOCK, Dir, PacedSink, flac_at};
    use lossless_proto::catalog::{Catalog, UNKNOWN};
    use lossless_proto::{Body, Command, Outcome, Request, ServerMessage, decode, encode};

    fn ask(sock: &Path, command: Command) -> Body {
        let mut s = UnixStream::connect(sock).unwrap();
        s.write_all(encode(&Request { id: 1, command }).unwrap().as_bytes())
            .unwrap();
        for line in BufReader::new(s).lines() {
            if let ServerMessage::Reply { id: 1, result } = decode(&line.unwrap()).unwrap() {
                match result {
                    Outcome::Ok(body) => return body,
                    other => panic!("{other:?}"),
                }
            }
        }
        panic!("no reply");
    }

    #[test]
    fn artist_columns_match_the_daemon() {
        let dir = Dir::new();
        for n in ["a/01 one.flac", "a/02 two.flac", "b/03 three.flac"] {
            let p = dir.path(&format!("music/{n}"));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            flac_at(&p, 44_100, 16, 2, 5 * BLOCK);
        }
        let sock = dir.path("sock");
        let listener = bind(&sock).unwrap();
        let (player, events) = Player::spawn_with_mixer(PacedSink::default(), None);
        let daemon = Daemon::start(
            player,
            events,
            Some(dir.path("music")),
            dir.path("state.json"),
            Box::new(|| {}),
        )
        .unwrap();
        thread::spawn(move || daemon.serve(listener));
        ask(&sock, Command::Update);

        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let tracks = loop {
            let Body::Library { tracks, .. } = ask(&sock, Command::Tracks) else {
                panic!("library")
            };
            if tracks.len() == 3 {
                break tracks;
            }
            assert!(std::time::Instant::now() < end, "scan never finished");
            thread::sleep(std::time::Duration::from_millis(20));
        };
        let catalog = Catalog::build(&tracks);
        let Body::Names(artists) = ask(&sock, Command::Artists) else {
            panic!("names")
        };
        assert_eq!(artists, catalog.artists());
        assert_eq!(artists, [UNKNOWN]);
        let Body::Names(albums) = ask(
            &sock,
            Command::Albums {
                artist: Some(UNKNOWN.into()),
            },
        ) else {
            panic!("names")
        };
        assert_eq!(albums, catalog.albums(Some(UNKNOWN)));
        let Body::Tracks(found) = ask(
            &sock,
            Command::Find {
                artist: Some(UNKNOWN.into()),
                album: Some(UNKNOWN.into()),
            },
        ) else {
            panic!("tracks")
        };
        let want: Vec<_> = catalog
            .find(&tracks, Some(UNKNOWN), Some(UNKNOWN))
            .into_iter()
            .map(|i| tracks[i].clone())
            .collect();
        assert_eq!(found, want);
    }
}

#[test]
fn broken_tui_ron_exits_2() {
    let dir = std::env::temp_dir().join(format!("tui-badron-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("lossless")).unwrap();
    std::fs::write(dir.join("lossless/tui.ron"), r#"(keys: {"nope": []})"#).unwrap();
    let out = tui()
        .env("XDG_CONFIG_HOME", &dir)
        .args(["--socket", "/nonexistent.sock"])
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("tui.ron:1:") && err.contains("nope"), "{err}");
}
