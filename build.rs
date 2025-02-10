use prost_build::Config;
use std::io::Result;

fn main() -> Result<()> {
    Config::new()
        .type_attribute(".", "#[derive(Eq)]")
        .compile_protos(&["src/messages.proto"], &["src"])?;
    Ok(())
}
