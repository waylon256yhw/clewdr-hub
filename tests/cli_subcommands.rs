//! Smoke tests for the CLI argument parser.
//!
//! These do not exercise verb implementations (most are `unimplemented!`);
//! they verify that the [`clewdr_hub::Args`] / [`clewdr_hub::Command`] parser
//! handles back-compat flags, global path overrides, and subcommand routing
//! the way the plan promises.

use clap::Parser;
use clewdr_hub::{Args, Command};
use std::path::PathBuf;

fn parse(argv: &[&str]) -> Args {
    Args::try_parse_from(argv).expect("parse")
}

#[test]
fn bare_invocation_has_no_subcommand() {
    let args = parse(&["clewdr"]);
    assert!(args.command.is_none());
    #[cfg(feature = "portable")]
    assert!(!args.update);
    assert!(args.global.config.is_none());
}

#[test]
fn explicit_serve_subcommand_parses() {
    let args = parse(&["clewdr", "serve"]);
    assert!(matches!(args.command, Some(Command::Serve)));
}

#[cfg(feature = "portable")]
#[test]
fn legacy_update_flag_parses() {
    let args = parse(&["clewdr", "--update"]);
    assert!(args.update);
    assert!(args.command.is_none());
}

#[cfg(feature = "portable")]
#[test]
fn update_subcommand_parses() {
    let args = parse(&["clewdr", "update"]);
    assert!(matches!(args.command, Some(Command::Update)));
    assert!(!args.update);
}

#[test]
fn global_flag_before_subcommand() {
    let args = parse(&[
        "clewdr",
        "--config",
        "/tmp/x.toml",
        "reset-admin-password",
        "--username",
        "bob",
    ]);
    assert_eq!(args.global.config, Some(PathBuf::from("/tmp/x.toml")));
    match args.command {
        Some(Command::ResetAdminPassword(reset_args)) => {
            assert_eq!(reset_args.username, "bob");
        }
        other => panic!("expected ResetAdminPassword, got {other:?}"),
    }
}

#[test]
fn global_flag_after_subcommand() {
    let args = parse(&[
        "clewdr",
        "reset-admin-password",
        "--config",
        "/tmp/x.toml",
        "--username",
        "bob",
    ]);
    assert_eq!(args.global.config, Some(PathBuf::from("/tmp/x.toml")));
    assert!(matches!(args.command, Some(Command::ResetAdminPassword(_))));
}

#[test]
fn nested_service_subcommand_parses() {
    let args = parse(&["clewdr", "service", "uninstall", "--purge"]);
    match args.command {
        Some(Command::Service(clewdr_hub::cli::service::ServiceCommand::Uninstall(a))) => {
            assert!(a.purge);
        }
        other => panic!("expected Service::Uninstall(purge), got {other:?}"),
    }
}

#[test]
fn import_mode_value_enum_parses() {
    use clewdr_hub::cli::import::Mode;
    let args = parse(&[
        "clewdr",
        "import-config",
        "/tmp/b.bundle",
        "--mode",
        "restore",
        "--yes",
    ]);
    match args.command {
        Some(Command::ImportConfig(a)) => {
            assert!(matches!(a.mode, Mode::Restore));
            assert!(a.yes);
            assert_eq!(a.path, PathBuf::from("/tmp/b.bundle"));
        }
        other => panic!("expected ImportConfig, got {other:?}"),
    }
}

#[test]
fn unknown_subcommand_is_rejected() {
    let res = Args::try_parse_from(["clewdr", "definitely-not-a-verb"]);
    assert!(res.is_err());
}
