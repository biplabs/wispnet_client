//! Small build metadata helper; requires only Cargo and Git.
#[path = "../../../scripts/build_metadata.rs"]
mod metadata;

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => println!("{}", metadata::identity(&root, env!("CARGO_PKG_VERSION"))?),
        [arg] if arg == "--version" => println!("{}", env!("CARGO_PKG_VERSION")),
        [arg] if arg == "--android-version-code" => {
            let parts = env!("CARGO_PKG_VERSION")
                .split('.')
                .map(str::parse::<u32>)
                .collect::<Result<Vec<_>, _>>()?;
            let [major, minor, patch] = parts.as_slice() else {
                return Err("Expected major.minor.patch release version".into());
            };
            let code = major
                .checked_mul(1_000_000)
                .and_then(|code| {
                    minor
                        .checked_mul(1_000)
                        .and_then(|minor| code.checked_add(minor))
                })
                .and_then(|code| code.checked_add(*patch))
                .filter(|code| *code > 0 && *code <= 2_100_000_000)
                .ok_or("Android version code outside supported range")?;
            println!("{code}");
        }
        _ => {
            return Err(
                "Usage: scripts/build_identity.sh [--version | --android-version-code]".into(),
            );
        }
    }
    Ok(())
}
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Build metadata failed: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
