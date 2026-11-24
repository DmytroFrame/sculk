//! `sculk-engine`: Horizon's game with Sculk's hooks. A worker starts it as `sculk-engine --dedicated ...`; every
//! argument is Horizon's (docs/server.md in Horizon).

#[global_allocator]
static ALLOCATOR: horizon_threads::Allocator = horizon_threads::ALLOCATOR;

/// The mod API level this game serves, kept in the program as Horizon's executables keep it; `main` reads it, so
/// the linker keeps it.
static MOD_API_LEVEL: [u8; horizon_mods::api_level::MARK.len()] = horizon_mods::api_level::MARK;

fn main() {
    std::hint::black_box(&MOD_API_LEVEL);
    if let Err(e) = sculk_engine::register() {
        eprintln!("sculk-engine: {e:#}");
        std::process::exit(1);
    }
    horizon_client::entry::main();
}
