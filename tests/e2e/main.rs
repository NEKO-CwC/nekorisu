//! End-to-end scenarios: tests/e2e/features/*.feature, written in Chinese, run against the real
//! `nekorisu` binary. Each scenario gets its own devices (folders, a running core each), an
//! in-process WebDAV storage and, when it asks for one, a trusted source. See README.md here.
//!
//! `cargo test --test e2e` runs them all. Scenarios tagged `@wip` are skipped; `@unix` and
//! `@windows` run only there; `@claude` and `@codex` only where that program is installed. `E2E_NAME=<part of a scenario name>` runs only matching ones.
//! To run a copy of the test program elsewhere, point `NEKORISU_E2E_BIN` at a `nekorisu` binary
//! and `NEKORISU_E2E_FEATURES` at the features folder.

mod standins;
mod steps;
mod system;
mod world;

use cucumber::World as _;

fn runnable_here(tags: &[String]) -> bool {
    let has = |t: &str| tags.iter().any(|tag| tag == t);
    if has("wip") {
        return false;
    }
    // Needs a test server (tests/e2e/server.sh); without one these are skipped.
    if has("server") && !standins::real_server_configured() {
        return false;
    }
    if has("unix") && !cfg!(unix) {
        return false;
    }
    if has("windows") && !cfg!(windows) {
        return false;
    }
    // Need the developer's own Claude Code or Codex (run with this device's settings folders).
    if has("claude") && system::claude().is_none() {
        return false;
    }
    if has("codex") && system::codex().is_none() {
        return false;
    }
    true
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let only = std::env::var("E2E_NAME").ok();
        world::E2e::cucumber()
            // `cargo test` hands its own flags to every test program; these scenarios take none.
            .with_default_cli()
            .max_concurrent_scenarios(4)
            .fail_on_skipped()
            .after(|_, _, _, _, world| {
                Box::pin(async move {
                    if let Some(world) = world {
                        world.shutdown().await;
                    }
                })
            })
            .filter_run_and_exit(
                std::env::var("NEKORISU_E2E_FEATURES")
                    .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/tests/e2e/features").to_string()),
                move |feature, _, scenario| {
                    let tags: Vec<String> = feature.tags.iter().chain(&scenario.tags).cloned().collect();
                    runnable_here(&tags) && only.as_deref().is_none_or(|part| scenario.name.contains(part))
                },
            )
            .await;
    });
}
