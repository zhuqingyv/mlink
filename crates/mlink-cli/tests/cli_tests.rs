use clap::Parser;
use mlink_cli::{Cli, Commands, RoomAction, TrustAction};

fn cmd(cli: &Cli) -> &Commands {
    cli.command.as_ref().expect("expected a subcommand")
}

#[test]
fn test_scan_command() {
    let cli = Cli::try_parse_from(["mlink", "scan"]).expect("scan should parse");
    assert!(matches!(cmd(&cli), Commands::Scan));
}

#[test]
fn test_connect_command() {
    let cli = Cli::try_parse_from(["mlink", "connect", "abc123"]).expect("connect should parse");
    match cmd(&cli) {
        Commands::Connect { peer_id } => assert_eq!(peer_id, "abc123"),
        other => panic!("expected Connect, got {other:?}"),
    }
}

#[test]
fn test_ping_command() {
    let cli = Cli::try_parse_from(["mlink", "ping", "abc123"]).expect("ping should parse");
    match cmd(&cli) {
        Commands::Ping { peer_id } => assert_eq!(peer_id, "abc123"),
        other => panic!("expected Ping, got {other:?}"),
    }
}

#[test]
fn test_send_command() {
    let cli =
        Cli::try_parse_from(["mlink", "send", "482193", "hello"]).expect("send should parse");
    match cmd(&cli) {
        Commands::Send { code, file, message } => {
            assert_eq!(code, "482193");
            assert_eq!(message.as_deref(), Some("hello"));
            assert!(file.is_none());
        }
        other => panic!("expected Send, got {other:?}"),
    }
}

#[test]
fn test_trust_list() {
    let cli = Cli::try_parse_from(["mlink", "trust", "list"]).expect("trust list should parse");
    match cmd(&cli) {
        Commands::Trust { action } => assert!(matches!(action, TrustAction::List)),
        other => panic!("expected Trust, got {other:?}"),
    }
}

#[test]
fn test_trust_remove() {
    let cli = Cli::try_parse_from(["mlink", "trust", "remove", "abc123"])
        .expect("trust remove should parse");
    match cmd(&cli) {
        Commands::Trust {
            action: TrustAction::Remove { peer_id },
        } => assert_eq!(peer_id, "abc123"),
        other => panic!("expected Trust::Remove, got {other:?}"),
    }
}

#[test]
fn test_doctor() {
    let cli = Cli::try_parse_from(["mlink", "doctor"]).expect("doctor should parse");
    assert!(matches!(cmd(&cli), Commands::Doctor));
}

#[test]
fn test_status() {
    let cli = Cli::try_parse_from(["mlink", "status"]).expect("status should parse");
    assert!(matches!(cmd(&cli), Commands::Status));
}

#[test]
fn test_no_args_is_one_shot_mode() {
    // Bare `mlink` with no args now means "generate a new room and serve".
    let cli = Cli::try_parse_from(["mlink"]).expect("bare mlink should parse");
    assert!(cli.command.is_none());
    assert!(cli.code.is_none());
}

#[test]
fn test_bare_code_is_one_shot_join() {
    // `mlink 482193` is the new one-shot "join this room" syntax.
    let cli = Cli::try_parse_from(["mlink", "482193"]).expect("bare code should parse");
    assert!(cli.command.is_none());
    assert_eq!(cli.code.as_deref(), Some("482193"));
}

#[test]
fn test_unknown_subcommand_fails() {
    // `bogus` is parsed as the positional `code` (it doesn't match any known
    // subcommand), so parsing succeeds at clap's level — runtime validation
    // rejects it because it isn't 6 digits. We assert that shape here.
    let cli = Cli::try_parse_from(["mlink", "bogus"]).expect("parses as bare code");
    assert!(cli.command.is_none());
    assert_eq!(cli.code.as_deref(), Some("bogus"));
}

#[test]
fn test_connect_missing_peer_id_fails() {
    let res = Cli::try_parse_from(["mlink", "connect"]);
    assert!(res.is_err(), "connect without peer_id should fail");
}

#[test]
fn test_send_missing_message_parses_ok_but_runtime_errors() {
    // `send <code>` with no message and no --file parses successfully (both
    // are Option<…>); the runtime layer enforces "at least one is required".
    let cli = Cli::try_parse_from(["mlink", "send", "482193"]).expect("parse ok");
    match cmd(&cli) {
        Commands::Send { code, file, message } => {
            assert_eq!(code, "482193");
            assert!(file.is_none());
            assert!(message.is_none());
        }
        other => panic!("expected Send, got {other:?}"),
    }
}

// ---- Phase 10: new room-code commands ---------------------------------------

#[test]
fn test_serve_command() {
    let cli = Cli::try_parse_from(["mlink", "serve"]).expect("serve should parse");
    assert!(matches!(cmd(&cli), Commands::Serve));
}

#[test]
fn test_room_new() {
    let cli = Cli::try_parse_from(["mlink", "room", "new"]).expect("room new should parse");
    match cmd(&cli) {
        Commands::Room { action } => assert!(matches!(action, RoomAction::New)),
        other => panic!("expected Room, got {other:?}"),
    }
}

#[test]
fn test_room_join() {
    let cli =
        Cli::try_parse_from(["mlink", "room", "join", "482193"]).expect("room join should parse");
    match cmd(&cli) {
        Commands::Room {
            action: RoomAction::Join { code },
        } => assert_eq!(code, "482193"),
        other => panic!("expected Room::Join, got {other:?}"),
    }
}

#[test]
fn test_room_leave() {
    let cli = Cli::try_parse_from(["mlink", "room", "leave", "482193"])
        .expect("room leave should parse");
    match cmd(&cli) {
        Commands::Room {
            action: RoomAction::Leave { code },
        } => assert_eq!(code, "482193"),
        other => panic!("expected Room::Leave, got {other:?}"),
    }
}

#[test]
fn test_room_list() {
    let cli = Cli::try_parse_from(["mlink", "room", "list"]).expect("room list should parse");
    match cmd(&cli) {
        Commands::Room { action } => assert!(matches!(action, RoomAction::List)),
        other => panic!("expected Room, got {other:?}"),
    }
}

#[test]
fn test_room_peers() {
    let cli = Cli::try_parse_from(["mlink", "room", "peers", "482193"])
        .expect("room peers should parse");
    match cmd(&cli) {
        Commands::Room {
            action: RoomAction::Peers { code },
        } => assert_eq!(code, "482193"),
        other => panic!("expected Room::Peers, got {other:?}"),
    }
}

#[test]
fn test_send_with_message() {
    let cli = Cli::try_parse_from(["mlink", "send", "482193", "hello world"])
        .expect("send with message should parse");
    match cmd(&cli) {
        Commands::Send { code, file, message } => {
            assert_eq!(code, "482193");
            assert_eq!(message.as_deref(), Some("hello world"));
            assert!(file.is_none());
        }
        other => panic!("expected Send, got {other:?}"),
    }
}

#[test]
fn test_send_with_file() {
    let cli = Cli::try_parse_from(["mlink", "send", "482193", "--file", "./report.pdf"])
        .expect("send with file should parse");
    match cmd(&cli) {
        Commands::Send { code, file, message } => {
            assert_eq!(code, "482193");
            assert!(message.is_none());
            assert_eq!(
                file.as_ref().and_then(|p| p.to_str()),
                Some("./report.pdf")
            );
        }
        other => panic!("expected Send, got {other:?}"),
    }
}

#[test]
fn test_listen_command() {
    let cli = Cli::try_parse_from(["mlink", "listen"]).expect("listen should parse");
    assert!(matches!(cmd(&cli), Commands::Listen));
}

#[test]
fn test_room_new_extra_arg_fails() {
    let res = Cli::try_parse_from(["mlink", "room", "new", "extra"]);
    assert!(res.is_err(), "room new should not take positional args");
}

#[test]
fn test_room_join_missing_code_fails() {
    let res = Cli::try_parse_from(["mlink", "room", "join"]);
    assert!(res.is_err(), "room join without code should fail");
}

// ---- Phase 11 hotfix: one-line launcher (team-lead spec cases) --------------

#[test]
fn test_oneline_code_567892() {
    // Exactly what the hotfix task named: `mlink 567892`.
    let cli = Cli::try_parse_from(["mlink", "567892"]).expect("mlink 567892 should parse");
    assert_eq!(cli.code.as_deref(), Some("567892"));
    assert!(cli.command.is_none(), "command must be None, got {:?}", cli.command);
}

#[test]
fn test_oneline_no_args_none_none() {
    // Exactly what the hotfix task named: `mlink`.
    let cli = Cli::try_parse_from(["mlink"]).expect("bare mlink should parse");
    assert!(cli.code.is_none(), "code must be None, got {:?}", cli.code);
    assert!(cli.command.is_none(), "command must be None, got {:?}", cli.command);
}

#[test]
fn test_oneline_send_subcommand_still_works() {
    // Exactly what the hotfix task named: `mlink send 567892 "hello"`.
    let cli = Cli::try_parse_from(["mlink", "send", "567892", "hello"])
        .expect("send subcommand must still parse");
    // When a subcommand is given, the top-level `code` should NOT be filled.
    assert!(
        cli.code.is_none(),
        "top-level code should be None when a subcommand follows, got {:?}",
        cli.code
    );
    match cmd(&cli) {
        Commands::Send { code, file, message } => {
            assert_eq!(code, "567892");
            assert_eq!(message.as_deref(), Some("hello"));
            assert!(file.is_none());
        }
        other => panic!("expected Send, got {other:?}"),
    }
}
