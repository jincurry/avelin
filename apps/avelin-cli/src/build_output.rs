//! Publish build outputs with exclusive creation; never replace an existing file.
use anyhow::{Context, Result};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
};

fn create_output(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create new build output {}", path.display()))
}

pub(crate) fn copy_new_file(source: &Path, destination: &Path) -> Result<()> {
    let mut source = File::open(source)?;
    let mut output = create_output(destination)?;
    std::io::copy(&mut source, &mut output)?;
    output.sync_all()?;
    Ok(())
}

pub(crate) fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut output = create_output(path)?;
    output.write_all(bytes)?;
    output.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};

    struct OutputFixture(PathBuf);
    impl OutputFixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "avelin-build-output-test-{}",
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir(&directory).unwrap();
            Self(directory)
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for OutputFixture {
        fn drop(&mut self) {
            // Only remove the exact files created by this isolated fixture.
            for name in ["source.bin", "output.bin", "copied.bin", "written.json"] {
                let _ = fs::remove_file(self.path(name));
            }
            let _ = fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn sidecar_writers_preserve_outputs_created_before_exclusive_open() {
        let fixture = OutputFixture::new();
        let source = fixture.path("source.bin");
        let output = fixture.path("output.bin");
        fs::write(&source, b"new build bytes").unwrap();
        fs::write(&output, b"existing output must survive").unwrap();
        assert!(copy_new_file(&source, &output).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"existing output must survive");
        assert!(write_new_file(&output, b"replacement metadata").is_err());
        assert_eq!(fs::read(&output).unwrap(), b"existing output must survive");
    }

    #[test]
    fn sidecar_writers_create_new_outputs_with_exact_contents() {
        let fixture = OutputFixture::new();
        let source = fixture.path("source.bin");
        fs::write(&source, b"backend bytes").unwrap();
        copy_new_file(&source, &fixture.path("copied.bin")).unwrap();
        write_new_file(&fixture.path("written.json"), br#"{"schema_version":1}"#).unwrap();
        assert_eq!(
            fs::read(fixture.path("copied.bin")).unwrap(),
            b"backend bytes"
        );
        assert_eq!(
            fs::read(fixture.path("written.json")).unwrap(),
            br#"{"schema_version":1}"#
        );
    }
}
