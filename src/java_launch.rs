use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum JavaLaunchError {
    #[error("invalid Java launch configuration: {0}")]
    Configuration(&'static str),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Archive(#[from] zip::result::ZipError),
}

pub(crate) struct JavaLaunchPath {
    pub(crate) argument: OsString,
    _manifest: Option<tempfile::NamedTempFile>,
}

impl AsRef<OsStr> for JavaLaunchPath {
    fn as_ref(&self) -> &OsStr {
        &self.argument
    }
}

pub(crate) fn classpath(paths: &[PathBuf]) -> Result<JavaLaunchPath, JavaLaunchError> {
    let separator = if cfg!(windows) { b';' } else { b':' };
    if let Ok(argument) = std::env::join_paths(paths)
        && paths
            .iter()
            .all(|path| !path.as_os_str().as_encoded_bytes().contains(&separator))
        && argument.as_encoded_bytes().is_ascii()
        && argument.as_encoded_bytes().len() <= 16_384
    {
        return Ok(JavaLaunchPath {
            argument,
            _manifest: None,
        });
    }
    manifest_jar(paths, None)
}

pub(crate) fn manifest_jar(
    paths: &[PathBuf],
    main_class: Option<&str>,
) -> Result<JavaLaunchPath, JavaLaunchError> {
    let manifest = classpath_manifest(paths, main_class)?;
    let mut file = classpath_tempfile()?;
    {
        let mut archive = zip::ZipWriter::new(file.as_file_mut());
        archive.start_file(
            "META-INF/MANIFEST.MF",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )?;
        archive.write_all(&manifest)?;
        archive.finish()?;
    }
    file.as_file().sync_all()?;
    Ok(JavaLaunchPath {
        argument: file.path().as_os_str().to_owned(),
        _manifest: Some(file),
    })
}

pub(crate) fn classpath_manifest(
    paths: &[PathBuf],
    main_class: Option<&str>,
) -> Result<Vec<u8>, JavaLaunchError> {
    let mut manifest = b"Manifest-Version: 1.0\r\n".to_vec();
    if let Some(main_class) = main_class {
        if main_class.len() > 1024
            || !main_class.split('.').all(|part| {
                let mut bytes = part.bytes();
                bytes
                    .next()
                    .is_some_and(|byte| byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$'))
                    && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$'))
            })
        {
            return Err(JavaLaunchError::Configuration("invalid main class"));
        }
        append_header(
            &mut manifest,
            format!("Main-Class: {main_class}").as_bytes(),
        );
    }
    let mut value = String::from("Class-Path:");
    for path in paths {
        let url = reqwest::Url::from_file_path(path).map_err(|_| {
            JavaLaunchError::Configuration("classpath path cannot be encoded as a file URL")
        })?;
        if url.scheme() != "file" || !url.as_str().is_ascii() {
            return Err(JavaLaunchError::Configuration(
                "classpath file URL is not ASCII",
            ));
        }
        value.push(' ');
        value.push_str(url.as_str());
        if value.len() > 65_535 {
            return Err(JavaLaunchError::Configuration(
                "classpath manifest exceeds 64 KiB",
            ));
        }
    }
    append_header(&mut manifest, value.as_bytes());
    manifest.extend_from_slice(b"\r\n");
    Ok(manifest)
}

fn append_header(manifest: &mut Vec<u8>, value: &[u8]) {
    for (index, part) in value.chunks(69).enumerate() {
        if index != 0 {
            manifest.push(b' ');
        }
        manifest.extend_from_slice(part);
        manifest.extend_from_slice(b"\r\n");
    }
}

fn classpath_tempfile() -> Result<tempfile::NamedTempFile, JavaLaunchError> {
    let mut directories = vec![std::env::temp_dir()];
    #[cfg(windows)]
    if let Some(windows) = std::env::var_os("SystemRoot") {
        directories.push(PathBuf::from(windows).join("Temp"));
    }
    #[cfg(unix)]
    directories.push(PathBuf::from("/tmp"));
    for directory in directories {
        let Ok(directory) = std::path::absolute(directory) else {
            continue;
        };
        if !directory.as_os_str().as_encoded_bytes().is_ascii() {
            continue;
        }
        let separator = if cfg!(windows) { b';' } else { b':' };
        if directory
            .as_os_str()
            .as_encoded_bytes()
            .contains(&separator)
        {
            continue;
        }
        if let Ok(file) = tempfile::Builder::new()
            .prefix("zerqon-java-classpath-")
            .suffix(".jar")
            .tempfile_in(directory)
        {
            return Ok(file);
        }
    }
    Err(JavaLaunchError::Configuration(
        "Java classpath requires a writable temporary directory with an ASCII path",
    ))
}
