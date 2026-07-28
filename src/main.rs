//! roomba -- sweep up after Bazel. Nothing it removes is precious; it all rebuilds.

mod cas;
mod cli;
mod layout;
mod liveness;
mod policy;
mod reaper;
mod util;

#[cfg(test)]
mod testutil;

fn main() {
    std::process::exit(cli::main());
}
