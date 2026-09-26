//! The Windows sandbox launcher on its own, for tests and other embedders
//! of `ferrule-sandbox`; `ferrule __sandbox-launch` is the same thing.

fn main() {
    ferrule_sandbox::launch::intercept();
    eprintln!(
        "ferrule-sandbox-launch: run by the sandbox as `{} <program> <args…>`",
        ferrule_sandbox::launch::LAUNCH_ARG
    );
    std::process::exit(ferrule_sandbox::launch::LAUNCH_FAILED);
}
