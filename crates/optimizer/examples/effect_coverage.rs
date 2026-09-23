//! Print the Gate 1 table: how much of the game has an effect record.
//!
//! `docs/sprints/008-data-driven-simulator.md` Gate 1 is a set of counts.
//! This is the instrument that prints them, so "done" is a number rather
//! than a report. The computation lives in the library
//! (`gw2_optimizer::data::effect_coverage`), which is what the addon and the
//! tests read too — this example only formats it.
//!
//!   cargo run --release -p gw2-optimizer --example effect_coverage

use gw2_optimizer::data::effect_coverage::{coverage_table, profession_sources};
use gw2_optimizer::gamedb::GameDb;

fn main() {
    let profession = std::env::args().nth(1);
    let addon_dir = match gw2_api::dev_config::addons_dir() {
        Ok(dir) => dir.join("gw2_build_optimizer"),
        Err(e) => {
            eprintln!("no addons_dir in dev.cfg ({e})");
            std::process::exit(2);
        }
    };
    let cache = gw2_api::cache::DataCache::new(addon_dir.join("cache"));
    let db = match GameDb::load(&cache) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("game data not cached ({e}) — sync it in-game first");
            std::process::exit(2);
        }
    };

    let table = coverage_table(&db);
    println!("# Gate 1: effect-record coverage\n");
    print!("{}", table.render());

    // Every row sums, so the sprint's remaining work is the three columns
    // right of Executable.
    for row in &table.classes {
        assert_eq!(
            row.population,
            row.executable + row.abstaining + row.coverage + row.none,
            "{} does not sum",
            row.class
        );
    }
    let todo: usize = table
        .classes
        .iter()
        .map(|row| row.abstaining + row.coverage + row.none)
        .sum();
    println!(
        "
Sources the simulator cannot execute yet: {todo}"
    );
    println!("Denominators: traits from each specialization; runes and sigils");
    println!("are exotic upgrade components and relics exotic; profession skills");
    println!("are every Profession_* slot, stolen and legend entries included");
    println!("because they land on the bar; elite skills are the Elite slot.");

    if let Some(profession) = profession {
        println!("\n# {profession} trait sources\n");
        for source in profession_sources(&db, &profession) {
            println!(
                "{} {} {} {}: {:?} {}",
                source.slot, source.id, source.specialization, source.name, source.verdict, source.reason
            );
        }
    }
}
