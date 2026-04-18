use clap::Parser;
use mlink_cli::{Cli, Commands, TrustAction};

#[test]
fn test_scan_command() {
    let cli = Cli::try_parse_from(["mlink", "scan"]).expect("scan should parse");
    assert!(matches!(cli.command, Commands::Scan));
}

#[test]
fn test_connect_command() {
    let cli = Cli::try_parse_from(["mlink", "connect", "abc123"]).expect("connect should parse");
    match cli.command {
        Commands::Connect { peer_id } => assert_eq!(peer_id, "abc123"),
        other => panic!("expected Connect, got {other:?}"),
    }
}

#[test]
fn test_ping_command() {
    let cli = Cli::try_parse_from(["mlink", "ping", "abc123"]).expect("ping should parse");
    match cli.command {
        Commands::Ping { peer_id } => assert_eq!(peer_id, "abc123"),
        other => panic!("expected Ping, got {other:?}"),
    }
}

#[test]
fn test_send_command() {
    let cli =
        Cli::try_parse_from(["mlink", "send", "abc123", "hello"]).expect("send should parse");
    match cli.command {
        Commands::Send { peer_id, message } => {
            assert_eq!(peer_id, "abc123");
            assert_eq!(message, "hello");
        }
        other => panic!("expected Send, got {other:?}"),
    }
}

#[test]
fn test_trust_list() {
    let cli = Cli::try_parse_from(["mlink", "trust", "list"]).expect("trust list should parse");
    match cli.command {
        Commands::Trust { action } => assert!(matches!(action, TrustAction::List)),
        other => panic!("expected Trust, got {other:?}"),
    }
}

#[test]
fn test_trust_remove() {
    let cli = Cli::try_parse_from(["mlink", "trust", "remove", "abc123"])
        .expect("trust remove should parse");
    match cli.command {
        Commands::Trust {
            action: TrustAction::Remove { peer_id },
        } => assert_eq!(peer_id, "abc123"),
        other => panic!("expected Trust::Remove, got {other:?}"),
    }
}

#[test]
fn test_doctor() {
    let cli = Cli::try_parse_from(["mlink", "doctor"]).expect("doctor should parse");
    assert!(matches!(cli.command, Commands::Doctor));
}

#[test]
fn test_status() {
    let cli = Cli::try_parse_from(["mlink", "status"]).expect("status should parse");
    assert!(matches!(cli.command, Commands::Status));
}

#[test]
fn test_no_args_fails() {
    let res = Cli::try_parse_from(["mlink"]);
    assert!(res.is_err(), "bare `mlink` with no subcommand should fail");
}

#[test]
fn test_unknown_subcommand_fails() {
    let res = Cli::try_parse_from(["mlink", "bogus"]);
    assert!(res.is_err(), "unknown subcommand should fail");
}

#[test]
fn test_connect_missing_peer_id_fails() {
    let res = Cli::try_parse_from(["mlink", "connect"]);
    assert!(res.is_err(), "connect without peer_id should fail");
}

#[test]
fn test_send_missing_message_fails() {
    let res = Cli::try_parse_from(["mlink", "send", "abc123"]);
    assert!(res.is_err(), "send without message should fail");
}
