use std::io::Result;
fn main() -> Result<()> {
    let mut prost_build = prost_build::Config::new();
    prost_build
        .protoc_executable("C:\\Users\\franko\\Downloads\\protoc-25.6-win64\\bin\\protoc.exe");
    prost_build.compile_protos(&["src/messages.proto"], &["src"])?;
    Ok(())
}
