//! Checks against the installed game, which a normal test run cannot assume is there:
//!
//! ```text
//! ZAR_GAME_DIR=<game folder> cargo test -p zar-logs-layoutgen -- --ignored
//! ```
//!
//! They check that the three public functions agree with each other on the installed game.

use std::path::PathBuf;

#[test]
#[ignore = "needs the installed game: set ZAR_GAME_DIR"]
fn value_text_and_cache_name_agree() {
    let game = PathBuf::from(std::env::var_os("ZAR_GAME_DIR").expect("ZAR_GAME_DIR"));
    let value = zar_logs_layoutgen::generate(&game).unwrap();
    let text = zar_logs_layoutgen::generate_text(&game).unwrap();
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap(), value);

    assert_eq!(value["version"], zar_logs_layoutgen::VERSION);
    let build = &value["build"];
    assert_eq!(
        zar_logs_layoutgen::cache_file_name(&game).unwrap(),
        format!("layout_{}_{}.json", build["dll_size"], build["dll_mtime"])
    );
    // what the meter cannot start without
    assert!(value["gamerunner_typeinfo_rva"].as_u64().unwrap() > 0);
    assert!(value["gamerunner_statics"]["<Instance>k__BackingField"].is_u64());
    assert!(value["structs"]["GameEvent"]["size"].as_u64().unwrap() > 0);
    assert!(!value["event_extra"].as_object().unwrap().is_empty());
}

/// The four gear-maths factors as read from the installed build's code, with their evidence (printed with
/// `--nocapture`). The expected numbers are those of the build this was written against; after a game update
/// that changes one, this is the test to look at, not necessarily a bug.
#[test]
#[ignore = "needs the installed game: set ZAR_GAME_DIR"]
fn code_constants_of_the_installed_build() {
    let game = PathBuf::from(std::env::var_os("ZAR_GAME_DIR").expect("ZAR_GAME_DIR"));
    let started = std::time::Instant::now();
    let value = zar_logs_layoutgen::code_constants(&game).unwrap();
    println!("code_constants(): {:.3} s", started.elapsed().as_secs_f64());
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
    assert_eq!(value["accuracy_percent_per_mod_point"], 1.25);
    assert_eq!(value["enhancement_reload_percent_per_mod_point"], 2.0);
    assert_eq!(value["enhancement_crit_percent_per_mod_point"], 0.8);
    assert_eq!(value["enhancement_magazine_percent_per_mod_point"], 4.0);
    assert_eq!(value.as_object().unwrap().len(), 5, "four numbers and their evidence");
}
