//! `AWMAN_HOME` relocates every awman path; XDG splits config and data
//! homes when it is unset; the removed path variables are ignored.

use awman::data::config::env::EnvSnapshot;
use awman::data::fs::data_paths::{config_home, data_home, AWMAN_HOME};
use awman::data::fs::{ApiPaths, DataPaths, SquadPaths};
use std::path::PathBuf;

#[test]
fn awman_home_roots_config_data_api_and_squad() {
    let env = EnvSnapshot::with_overrides([
        (AWMAN_HOME, "/aw"),
        ("XDG_CONFIG_HOME", "/xc"),
        ("XDG_DATA_HOME", "/xd"),
    ]);
    assert_eq!(config_home(&env).unwrap(), PathBuf::from("/aw"));
    assert_eq!(data_home(&env).unwrap(), PathBuf::from("/aw"));
    assert_eq!(
        DataPaths::from_env(&env).unwrap().daemon().root(),
        std::path::Path::new("/aw/data/daemon")
    );
    assert_eq!(
        ApiPaths::from_env(&env).unwrap().daemon(),
        SquadPaths::from_env(&env).unwrap().daemon()
    );
    assert_eq!(
        DataPaths::from_env(&env).unwrap().db_path(),
        PathBuf::from("/aw/data/awman.db")
    );
    assert_eq!(
        ApiPaths::from_env(&env).unwrap().root(),
        PathBuf::from("/aw/api")
    );
    assert_eq!(
        SquadPaths::from_env(&env).unwrap().root(),
        PathBuf::from("/aw/squad")
    );
    assert_eq!(
        awman::data::config::global_config_path(&env).unwrap(),
        PathBuf::from("/aw/config.json")
    );
}

#[test]
fn xdg_fallback_and_removed_variables() {
    let env = EnvSnapshot::with_overrides([
        ("XDG_CONFIG_HOME", "/xc"),
        ("XDG_DATA_HOME", "/xd"),
        ("AWMAN_CONFIG_HOME", "/old"),
        ("AWMAN_API_ROOT", "/old-api"),
        ("AWMAN_SQUAD_ROOT", "/old-squad"),
        ("AWMAN_ATTACH_DIR", "/old-attach"),
    ]);
    assert_eq!(
        awman::data::config::global_config_path(&env).unwrap(),
        PathBuf::from("/xc/awman/config.json")
    );
    assert_eq!(
        ApiPaths::from_env(&env).unwrap().root(),
        PathBuf::from("/xd/awman/api")
    );
    assert_eq!(
        SquadPaths::from_env(&env).unwrap().root(),
        PathBuf::from("/xd/awman/squad")
    );
}
